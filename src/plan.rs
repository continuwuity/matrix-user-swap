use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

use ruma::{
    events::{
        direct::DirectEventContent,
        ignored_user_list::IgnoredUserListEventContent,
        room::{join_rules::JoinRule, member::MembershipState},
        tag::TagEventContent,
        AnyGlobalAccountDataEventContent, AnyRoomAccountDataEventContent,
        GlobalAccountDataEventType, RoomAccountDataEventType,
    },
    serde::Raw,
    Int, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId,
};
use serde::Serialize;
use thiserror::Error;
use tracing as t;

use crate::{
    state::StateAccessor,
    utils::{merge_json, JsonMap, JsonMergeError},
    UserKind,
};

fn is_default<T: Default + Eq>(value: &T) -> bool {
    value == &T::default()
}

#[derive(Serialize)]
pub(crate) struct RoomPlan {
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) alias: Option<OwnedRoomAliasId>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub(crate) invite: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub(crate) join: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) power_level: Option<Int>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) account_data:
        BTreeMap<RoomAccountDataEventType, Raw<AnyRoomAccountDataEventContent>>,
}

#[derive(Serialize)]
pub(crate) struct Plan {
    pub(crate) new_user_id: OwnedUserId,
    pub(crate) rooms: BTreeMap<OwnedRoomId, RoomPlan>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) global_account_data: BTreeMap<
        GlobalAccountDataEventType,
        Raw<AnyGlobalAccountDataEventContent>,
    >,
}

struct MakePlanState<'a, S: StateAccessor> {
    old: &'a S,
    new: &'a S,
    new_user_id: OwnedUserId,
    old_user_id: OwnedUserId,
    new_joined_rooms: HashSet<OwnedRoomId>,
    errors: Vec<PlanError<S>>,

    plan: Plan,
}

/// Errors that prevent determining migration plan entirely.
#[derive(Error, Debug)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum FatalPlanError<S: StateAccessor> {
    #[error("failed to get user id for {_0} user")]
    GetUserId(UserKind, #[source] S::Error),

    #[error("failed to get joined room list for {_0} user")]
    GetJoinedRooms(UserKind, #[source] S::Error),
}

/// Errors that prevent determining migration plan for a specific room.
#[derive(Error, Debug, Serialize)]
pub(crate) enum RoomPlanError<S: StateAccessor> {
    #[error("failed to get join rules")]
    GetJoinRule(
        #[source]
        #[serde(skip)]
        S::Error,
    ),

    #[error("failed to get new user membership state")]
    GetMembership(
        #[source]
        #[serde(skip)]
        S::Error,
    ),

    #[error("failed to get power levels")]
    GetPowerLevels(
        #[source]
        #[serde(skip)]
        S::Error,
    ),

    #[error("failed to get server ACL")]
    GetServerAcl(
        #[source]
        #[serde(skip)]
        S::Error,
    ),

    #[error(
        "room is invite-only, but old user does not have permission to invite \
         new user"
    )]
    CannotInvite,

    #[error("new user is banned from room. Will not attempt to join")]
    Banned,

    #[error(
        "new user's server is ACL banned from room. Will not attempt to join"
    )]
    AclBanned,
}

/// Fatal errors migrating `m.tags` room account data event.
#[derive(Error, Debug, Serialize)]
pub(crate) enum RoomTagsPlanError<S: StateAccessor> {
    #[error("failed to get tags for {_0} user")]
    GetEvent(
        UserKind,
        #[source]
        #[serde(skip)]
        S::Error,
    ),
}

/// Fatal errors migrating `m.direct` global account data event.
#[derive(Error, Debug, Serialize)]
pub(crate) enum DirectAccountDataPlanError<S: StateAccessor> {
    #[error("failed to get direct message mapping for {_0} user")]
    GetEvent(
        UserKind,
        #[source]
        #[serde(skip)]
        S::Error,
    ),
}

/// Fatal errors migrating `m.ignored_user_list` global account data event.
#[derive(Error, Debug, Serialize)]
pub(crate) enum IgnoredUsersAccountDataPlanError<S: StateAccessor> {
    #[error("failed to get ignored users list for {_0} user")]
    GetEvent(
        UserKind,
        #[source]
        #[serde(skip)]
        S::Error,
    ),
}

/// Non-fatal errors determining a migration plan.
///
/// These may block fully migrating particular rooms or data, but are not fatal
/// for the migration as a whole.
#[derive(Error, Debug, Serialize)]
pub(crate) enum PlanError<S: StateAccessor> {
    #[error("cannot migrate room {_0}")]
    #[serde(bound(serialize = "RoomPlanError<S>: Serialize"))]
    RoomFailed(OwnedRoomId, #[source] RoomPlanError<S>),

    #[error("cannot migrate tags in room {_0}")]
    #[serde(bound(serialize = "RoomTagsPlanError<S>: Serialize"))]
    RoomTagsFailed(OwnedRoomId, #[source] RoomTagsPlanError<S>),

    #[error("cannot migrate {} tag in room {}. Both the old and new users have a tag with this key, but they have different values. Old value is {}. New value is {}.", error.key, room_id, error.old_value, error.new_value)]
    #[serde(bound(serialize = "RoomTagsPlanError<S>: Serialize"))]
    RoomTagMerge {
        room_id: OwnedRoomId,
        error: JsonMergeError,
    },

    #[error("cannot migrate direct message mapping")]
    #[serde(bound(serialize = "DirectAccountDataPlanError<S>: Serialize"))]
    DirectAccountDataFailed(#[from] DirectAccountDataPlanError<S>),

    #[error("cannot migrate ignored users list")]
    #[serde(bound(
        serialize = "IgnoredUsersAccountDataPlanError<S>: Serialize"
    ))]
    IgnoredUsersAccountDataFailed(#[from] IgnoredUsersAccountDataPlanError<S>),

    #[error(
        "failed to get alias for room {_0}. This is mostly inconsequential, \
         and just might make it harder to identify the room in log messages."
    )]
    AliasFailed(
        OwnedRoomId,
        #[source]
        #[serde(skip)]
        S::Error,
    ),

    #[error(
        "old user does not have permission to copy their power level \
         ({old_power_level}) to new user in room {room_id}"
    )]
    CannotCopyPowerLevel {
        room_id: OwnedRoomId,
        old_power_level: Int,
    },

    #[error(
        "old user and new user both have entries in the ignored users list \
         for the user {}, but they have different values. The 1.12 spec \
         doesn't specify any semantics for these values, so the old user's \
         entry cannot be merged into the new user's safely. The old value is
         {}. The new value is {}.", _0.key, _0.old_value, _0.new_value
    )]
    IgnoredUserMerge(JsonMergeError),
}

impl RoomPlan {
    /// Returns `true` if no actions need to be taken for this room.
    fn is_empty(&self) -> bool {
        !self.invite
            && !self.join
            && self.power_level.is_none()
            && self.account_data.is_empty()
    }
}

impl<S: StateAccessor> MakePlanState<'_, S> {
    /// Returns whether the new user is expected to be joined to a given room
    /// after the migration is executed.
    fn will_join(&self, room_id: &RoomId) -> bool {
        self.new_joined_rooms.contains(room_id)
            || self
                .plan
                .rooms
                .get(room_id)
                .map(|room| room.join)
                .unwrap_or(false)
    }

    async fn plan_room(&mut self, room_id: OwnedRoomId) {
        let result = self.plan_room_inner(&room_id).await;
        match result {
            Ok(Some(plan)) => {
                self.plan.rooms.insert(room_id, plan);
            }
            Ok(None) => (),
            Err(e) => {
                self.errors.push(PlanError::RoomFailed(room_id, e));
            }
        }
    }

    async fn plan_room_inner(
        &mut self,
        room_id: &RoomId,
    ) -> Result<Option<RoomPlan>, RoomPlanError<S>> {
        use RoomPlanError as Error;

        // TODO: skip fetching the alias when we don't need it (this can happen
        // if a room is already fully migrated and we don't need to print an
        // error)
        let alias = match self.old.get_room_alias(room_id).await {
            Ok(alias) => alias,
            Err(e) => {
                self.errors.push(PlanError::AliasFailed(room_id.to_owned(), e));
                None
            }
        };

        let power_levels = self
            .old
            .get_power_levels(room_id)
            .await
            .map_err(Error::GetPowerLevels)?;

        let old_power_level = power_levels.for_user(&self.old_user_id);
        let new_power_level = power_levels.for_user(&self.new_user_id);
        let set_power_level = if new_power_level < old_power_level {
            if power_levels.user_can_change_user_power_level(
                &self.old_user_id,
                &self.new_user_id,
            ) {
                Some(old_power_level)
            } else {
                self.errors.push(PlanError::CannotCopyPowerLevel {
                    room_id: room_id.to_owned(),
                    old_power_level,
                });
                None
            }
        } else {
            None
        };

        let need_join = !self.new_joined_rooms.contains(room_id);

        let need_invite = if !need_join {
            false
        } else {
            let membership = self
                .old
                .get_membership(room_id, &self.new_user_id)
                .await
                .map_err(Error::GetMembership)?
                .unwrap_or(MembershipState::Leave);

            if membership == MembershipState::Ban {
                return Err(Error::Banned);
            }

            let server_acl = self
                .old
                .get_server_acl(room_id)
                .await
                .map_err(Error::GetServerAcl)?;

            if let Some(server_acl) = server_acl {
                if !server_acl.is_allowed(self.new_user_id.server_name()) {
                    return Err(Error::AclBanned);
                }
            }

            let invited = membership == MembershipState::Invite;

            let join_rule = self
                .old
                .get_join_rule(room_id)
                .await
                .map_err(Error::GetJoinRule)?;
            // TODO: handle 'allow' field of 'm.room.join_rules', which could
            // allow us to skip invites.
            !invited && !matches!(join_rule, Some(JoinRule::Public))
        };

        if need_invite {
            let can_invite = power_levels.user_can_invite(&self.old_user_id);

            if !can_invite {
                return Err(Error::CannotInvite);
            }
        }

        let mut account_data = BTreeMap::new();

        self.plan_room_account_data_tags(room_id, &mut account_data).await;

        let room_plan = RoomPlan {
            alias,
            invite: need_invite,
            join: need_join,
            power_level: set_power_level,
            account_data,
        };

        Ok(if !room_plan.is_empty() {
            Some(room_plan)
        } else {
            None
        })
    }

    async fn plan_room_account_data_tags(
        &mut self,
        room_id: &RoomId,
        account_data: &mut BTreeMap<
            RoomAccountDataEventType,
            Raw<AnyRoomAccountDataEventContent>,
        >,
    ) {
        match self.plan_room_account_data_tags_inner(room_id).await {
            Ok(Some(content)) => {
                account_data
                    .insert(RoomAccountDataEventType::Tag, content.cast());
            }
            Ok(None) => (),
            Err(error) => {
                self.errors
                    .push(PlanError::RoomTagsFailed(room_id.to_owned(), error));
            }
        }
    }

    async fn plan_room_account_data_tags_inner(
        &mut self,
        room_id: &RoomId,
    ) -> Result<Option<Raw<TagEventContent>>, RoomTagsPlanError<S>> {
        use RoomTagsPlanError as Error;

        let old = self
            .old
            .get_room_account_data_event::<JsonMap>(
                room_id,
                RoomAccountDataEventType::Tag,
            )
            .await
            .map_err(|e| Error::GetEvent(UserKind::Old, e))?;
        let Some(old) = old else {
            return Ok(None);
        };

        let new = self
            .new
            .get_room_account_data_event::<JsonMap>(
                room_id,
                RoomAccountDataEventType::Tag,
            )
            .await
            .map_err(|e| Error::GetEvent(UserKind::New, e))?
            .unwrap_or_default();

        let (merged, errors) = merge_json(old, new);

        let errors = errors.into_iter().map(|error| PlanError::RoomTagMerge {
            room_id: room_id.to_owned(),
            error,
        });
        self.errors.extend(errors);
        let merged = merged.map(|merged| {
            Raw::new(&merged)
                .expect("serialization should always succeed")
                .cast()
        });

        Ok(merged)
    }

    async fn plan_account_data_direct(&mut self) {
        let result = self.plan_account_data_direct_inner().await;
        match result {
            Ok(Some(content)) => {
                self.plan
                    .global_account_data
                    .insert(GlobalAccountDataEventType::Direct, content.cast());
            }
            Ok(None) => (),
            Err(e) => {
                self.errors.push(e.into());
            }
        }
    }

    async fn plan_account_data_direct_inner(
        &mut self,
    ) -> Result<Option<Raw<DirectEventContent>>, DirectAccountDataPlanError<S>>
    {
        use DirectAccountDataPlanError as Error;

        // TODO: shotgun parsing to deal with element[1] :(
        // [1]: https://github.com/element-hq/element-web/issues/27630

        let old = self
            .old
            .get_global_account_data_event::<DirectEventContent>(
                GlobalAccountDataEventType::Direct,
            )
            .await
            .map_err(|e| Error::GetEvent(UserKind::Old, e))?;
        let Some(old) = old else {
            return Ok(None);
        };

        let mut new = self
            .new
            .get_global_account_data_event::<DirectEventContent>(
                GlobalAccountDataEventType::Direct,
            )
            .await
            .map_err(|e| Error::GetEvent(UserKind::New, e))?
            .unwrap_or_default();

        let mut changed = false;

        for (user, rooms) in old.0 {
            // Don't try to migrate the m.direct mapping for DMs between the old
            // and new users. The result would be the new user recording a DM
            // with themselves.
            if user == self.new_user_id {
                continue;
            }

            for room in rooms {
                // TODO: it's possible for the join to fail in the execution
                // step, Try to handle that?
                if !self.will_join(&room) {
                    continue;
                }

                let new_rooms = new.entry(user.to_owned()).or_default();
                if !new_rooms.contains(&room) {
                    changed = true;
                    new_rooms.push(room);
                }
            }
        }

        if changed {
            Ok(Some(
                Raw::new(&new).expect("serialization should always succeed"),
            ))
        } else {
            Ok(None)
        }
    }

    async fn plan_account_data_ignored_users(&mut self) {
        let result = self.plan_account_data_ignored_users_inner().await;
        match result {
            Ok(Some(content)) => {
                self.plan.global_account_data.insert(
                    GlobalAccountDataEventType::IgnoredUserList,
                    content.cast(),
                );
            }
            Ok(None) => (),
            Err(e) => {
                self.errors.push(e.into());
            }
        }
    }

    async fn plan_account_data_ignored_users_inner(
        &mut self,
    ) -> Result<
        Option<Raw<IgnoredUserListEventContent>>,
        IgnoredUsersAccountDataPlanError<S>,
    > {
        use IgnoredUsersAccountDataPlanError as Error;

        let old = self
            .old
            .get_global_account_data_event::<JsonMap>(
                GlobalAccountDataEventType::IgnoredUserList,
            )
            .await
            .map_err(|e| Error::GetEvent(UserKind::Old, e))?;
        let Some(old) = old else {
            return Ok(None);
        };

        let new = self
            .new
            .get_global_account_data_event::<JsonMap>(
                GlobalAccountDataEventType::IgnoredUserList,
            )
            .await
            .map_err(|e| Error::GetEvent(UserKind::New, e))?
            .unwrap_or_default();

        let (merged, errors) = merge_json(old, new);

        self.errors.extend(errors.into_iter().map(PlanError::IgnoredUserMerge));
        let merged = merged.map(|merged| {
            Raw::new(&merged)
                .expect("serialization should always succeed")
                .cast()
        });

        Ok(merged)
    }
}

pub(crate) async fn make_plan<S: StateAccessor>(
    old: &S,
    new: &S,
) -> Result<(Plan, Vec<PlanError<S>>), FatalPlanError<S>> {
    use FatalPlanError as Error;

    let old_user_id = old
        .get_user_id()
        .await
        .map_err(|e| Error::GetUserId(UserKind::Old, e))?;
    let new_user_id = new
        .get_user_id()
        .await
        .map_err(|e| Error::GetUserId(UserKind::New, e))?;

    t::info!("fetching joined rooms for old user");
    let mut old_joined_rooms = old
        .get_joined_rooms()
        .await
        .map_err(|e| Error::GetJoinedRooms(UserKind::Old, e))?;

    // Ensure a deterministic order for snapshot tests
    old_joined_rooms.sort();

    t::info!("fetching joined rooms for new user");
    let new_joined_rooms = new
        .get_joined_rooms()
        .await
        .map_err(|e| Error::GetJoinedRooms(UserKind::New, e))?;

    let new_joined_rooms = new_joined_rooms.into_iter().collect::<HashSet<_>>();

    t::info!("need to evaluate {} rooms", old_joined_rooms.len());

    let mut state = MakePlanState {
        old,
        new,
        old_user_id,
        new_user_id: new_user_id.clone(),
        new_joined_rooms,
        errors: vec![],

        plan: Plan {
            new_user_id,
            rooms: BTreeMap::new(),
            global_account_data: BTreeMap::new(),
        },
    };

    for room_id in old_joined_rooms {
        state.plan_room(room_id).await;
    }

    state.plan_account_data_direct().await;
    state.plan_account_data_ignored_users().await;

    Ok((state.plan, state.errors))
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "New userid: {}", self.new_user_id)?;

        writeln!(f, "Rooms:")?;
        for (id, room) in &self.rooms {
            write!(f, "  - {id} (")?;
            if room.invite {
                write!(f, "invite,")?;
            }
            if room.join {
                write!(f, "join")?;
            }
            write!(f, ")")?;
            if let Some(alias) = &room.alias {
                write!(f, " [{alias}]")?;
            }
            if let Some(power_level) = room.power_level {
                write!(f, " (power={power_level})")?;
            }
            writeln!(f)?;
        }

        writeln!(f, "Global account data:")?;
        for (kind, content) in &self.global_account_data {
            let content_str = serde_json::to_string_pretty(content)
                .expect("Raw<T> serialization should always succeed");
            writeln!(f, "{kind}: {}", content_str)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::path::Path;

    use insta::assert_json_snapshot;
    use serde_json::json;

    use super::*;
    use crate::state::mock::{MockState, MockStateAccessor};

    async fn run_test(path: &Path) {
        let state = MockState::new(path).unwrap();
        let old = MockStateAccessor::new(UserKind::Old, &state);
        let new = MockStateAccessor::new(UserKind::New, &state);
        let (plan, errors) = make_plan(&old, &new).await.unwrap();

        insta::with_settings!({ snapshot_path => "../tests/output" }, {
            assert_json_snapshot!(json!({
                "errors": errors,
                "plan": plan,
            }));
        });
    }

    #[test]
    fn make_plan_tests() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        insta::glob!("../tests", "input/*.json5", |path| rt
            .block_on(run_test(path)));
    }
}
