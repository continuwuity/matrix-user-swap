use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    fmt,
};

use ruma::{
    events::{
        direct::DirectEventContent,
        ignored_user_list::IgnoredUserListEventContent,
        room::{
            history_visibility::HistoryVisibility, join_rules::JoinRule,
            member::MembershipState, power_levels::RoomPowerLevels,
        },
        tag::TagEventContent,
        AnyGlobalAccountDataEventContent, AnyRoomAccountDataEventContent,
        GlobalAccountDataEventType, RoomAccountDataEventType,
    },
    serde::Raw,
    Int, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId,
};
use serde::{Deserialize, Serialize};
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

#[derive(Default, Deserialize)]
pub(crate) struct PlanSettings {
    /// Leave rooms that are fully migrated with the old user.
    leave: bool,
}

pub(crate) type RoomAccountData =
    BTreeMap<RoomAccountDataEventType, Raw<AnyRoomAccountDataEventContent>>;

#[derive(Serialize)]
pub(crate) struct RoomPlan<S: StateAccessor> {
    // TODO: store this as RoomIdentity instead of separating the alias and id?
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) alias: Option<OwnedRoomAliasId>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub(crate) invite: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub(crate) join: bool,
    #[serde(default, skip_serializing_if = "is_default")]
    pub(crate) leave: bool,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) power_level: Option<Int>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) account_data: RoomAccountData,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(bound(serialize = "RoomPlanError<S>: Serialize"))]
    pub(crate) errors: Vec<RoomPlanError<S>>,
}

#[derive(Serialize)]
pub(crate) struct Plan<S: StateAccessor> {
    pub(crate) new_user_id: OwnedUserId,
    #[serde(bound(serialize = "RoomPlan<S>: Serialize"))]
    pub(crate) rooms: BTreeMap<OwnedRoomId, RoomPlan<S>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) global_account_data: BTreeMap<
        GlobalAccountDataEventType,
        Raw<AnyGlobalAccountDataEventContent>,
    >,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(bound(serialize = "PlanError<S>: Serialize"))]
    pub(crate) errors: Vec<PlanError<S>>,
}

struct MakePlanState<'a, S: StateAccessor> {
    settings: PlanSettings,
    old: &'a S,
    new: &'a S,
    new_user_id: OwnedUserId,
    old_user_id: OwnedUserId,
    new_joined_rooms: HashSet<OwnedRoomId>,

    plan: Plan<S>,
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

#[derive(Error, Debug, Serialize)]
pub(crate) enum RoomPlanError<S: StateAccessor> {
    #[error(
        "failed to get alias. This is mostly inconsequential, and just might \
         make it harder to identify the room in log messages."
    )]
    GetAlias(
        #[source]
        #[serde(skip)]
        S::Error,
    ),

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

    #[error("cannot migrate room tags")]
    #[serde(bound(serialize = "RoomTagsPlanError<S>: Serialize"))]
    RoomTagsFailed(#[source] RoomTagsPlanError<S>),

    #[error(
        "cannot migrate {} tag. Both the old and new users have a tag with \
         this key, but they have different values. Old value is {}. New value \
         is {}.",
        _0.key,
        _0.old_value,
        _0.new_value
    )]
    RoomTagMerge(JsonMergeError),

    #[error(
        "old user does not have permission to copy their power level \
         ({old_power_level}) to new user"
    )]
    CopyPowerLevel {
        old_power_level: Int,
    },

    #[error(
        "failed to get history visibility state. Unable to determine whether \
         message history visible to the old user in this room may be hidden \
         from the new user and lost if the old user leaves."
    )]
    GetHistoryVisibility(#[serde(skip)] S::Error),

    #[error(
        "some of the message history visible to the old user may not be \
         visible to the new user, because the room has restricted visible \
         message history to only messages sent after a user {}. If the old \
         user leaves this room, some message history may be lost.{}",
        describe_history_visibility(visibility),
        if *already_joined {
            "The new user has already joined this room, but may have joined \
             it later than the old user."
        } else {
            ""
        }
    )]
    RestrictedHistoryVisibility {
        visibility: HistoryVisibility,
        already_joined: bool,
    },
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
    #[error("cannot migrate direct message mapping")]
    #[serde(bound(serialize = "DirectAccountDataPlanError<S>: Serialize"))]
    DirectAccountDataFailed(#[from] DirectAccountDataPlanError<S>),

    #[error("cannot migrate ignored users list")]
    #[serde(bound(
        serialize = "IgnoredUsersAccountDataPlanError<S>: Serialize"
    ))]
    IgnoredUsersAccountDataFailed(#[from] IgnoredUsersAccountDataPlanError<S>),

    #[error(
        "old user and new user both have entries in the ignored users list \
         for the user {}, but they have different values. The 1.12 spec \
         doesn't specify any semantics for these values, so the old user's \
         entry cannot be merged into the new user's safely. The old value is
         {}. The new value is {}.", _0.key, _0.old_value, _0.new_value
    )]
    IgnoredUserMerge(JsonMergeError),
}

fn describe_history_visibility(
    visibility: &HistoryVisibility,
) -> Cow<'static, str> {
    match visibility {
        HistoryVisibility::WorldReadable => {
            "allows even users that are not in the room to see message history"
                .into()
        }
        HistoryVisibility::Shared => "allows users to see all message history \
                                      after they join the room"
            .into(),
        HistoryVisibility::Invited => "restricts visible history to only \
                                       messages that were sent after a user \
                                       was invited"
            .into(),
        HistoryVisibility::Joined => "restricted visible history to only \
                                      messages that were sent after a user \
                                      joined"
            .into(),
        _ => format!(
            "has an unrecognized history visibility setting {:?}",
            visibility.as_str()
        )
        .into(),
    }
}

impl<S: StateAccessor> RoomPlan<S> {
    /// Returns `true` if no actions need to be taken for this room and no
    /// errors were recorded.
    fn is_empty(&self) -> bool {
        !self.invite
            && !self.join
            && !self.leave
            && self.power_level.is_none()
            && self.account_data.is_empty()
            && self.errors.is_empty()
    }
}

// Can't use the derive macro because it adds a S: Default bound :(
impl<S: StateAccessor> Default for RoomPlan<S> {
    fn default() -> RoomPlan<S> {
        RoomPlan {
            alias: None,
            invite: false,
            join: false,
            leave: false,
            power_level: None,
            account_data: BTreeMap::new(),
            errors: vec![],
        }
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
        let mut plan = RoomPlan::default();

        if let Err(error) = self.plan_room_inner(&room_id, &mut plan).await {
            plan.errors.push(error);
        }

        if !plan.is_empty() {
            match self.old.get_room_alias(&room_id).await {
                Ok(alias) => plan.alias = alias,
                Err(error) => plan.errors.push(RoomPlanError::GetAlias(error)),
            }

            self.plan.rooms.insert(room_id, plan);
        }
    }

    async fn plan_room_inner(
        &mut self,
        room_id: &RoomId,
        plan: &mut RoomPlan<S>,
    ) -> Result<(), RoomPlanError<S>> {
        use RoomPlanError as Error;

        let power_levels = self
            .old
            .get_power_levels(room_id)
            .await
            .map_err(Error::GetPowerLevels)?;

        let set_power_level =
            self.plan_power_level(&power_levels, &mut plan.errors);

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
        };

        plan.join = need_join;
        plan.invite = need_invite;
        plan.power_level = set_power_level;

        let mut account_data = RoomAccountData::new();
        self.plan_room_account_data_tags(
            room_id,
            &mut account_data,
            &mut plan.errors,
        )
        .await;
        plan.account_data = account_data;

        self.check_history_visibility(room_id, need_join, &mut plan.errors)
            .await;

        Ok(())
    }

    /// If the old user's power level needs to (and can) be propagated to the
    /// new user in a given room, returns the power level to set.
    fn plan_power_level(
        &self,
        power_levels: &RoomPowerLevels,
        errors: &mut Vec<RoomPlanError<S>>,
    ) -> Option<Int> {
        let old_power_level = power_levels.for_user(&self.old_user_id);
        let new_power_level = power_levels.for_user(&self.new_user_id);
        if new_power_level < old_power_level {
            if power_levels.user_can_change_user_power_level(
                &self.old_user_id,
                &self.new_user_id,
            ) {
                Some(old_power_level)
            } else {
                errors.push(RoomPlanError::CopyPowerLevel {
                    old_power_level,
                });
                None
            }
        } else {
            None
        }
    }

    async fn plan_room_account_data_tags(
        &mut self,
        room_id: &RoomId,
        account_data: &mut RoomAccountData,
        errors: &mut Vec<RoomPlanError<S>>,
    ) {
        match self.plan_room_account_data_tags_inner(room_id, errors).await {
            Ok(Some(content)) => {
                account_data
                    .insert(RoomAccountDataEventType::Tag, content.cast());
            }
            Ok(None) => (),
            Err(error) => {
                errors.push(RoomPlanError::RoomTagsFailed(error));
            }
        }
    }

    async fn plan_room_account_data_tags_inner(
        &mut self,
        room_id: &RoomId,
        errors: &mut Vec<RoomPlanError<S>>,
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

        let (merged, merge_errors) = merge_json(old, new);

        let merge_errors =
            merge_errors.into_iter().map(RoomPlanError::RoomTagMerge);
        errors.extend(merge_errors);
        let merged = merged.map(|merged| {
            Raw::new(&merged)
                .expect("serialization should always succeed")
                .cast()
        });

        Ok(merged)
    }

    async fn check_history_visibility(
        &mut self,
        room_id: &RoomId,
        need_join: bool,
        errors: &mut Vec<RoomPlanError<S>>,
    ) {
        let result =
            self.check_history_visibility_inner(room_id, need_join).await;
        if let Err(e) = result {
            errors.push(e);
        }
    }

    async fn check_history_visibility_inner(
        &self,
        room_id: &RoomId,
        need_join: bool,
    ) -> Result<(), RoomPlanError<S>> {
        use RoomPlanError as Error;

        let visibility = self
            .old
            .get_history_visibility(room_id)
            .await
            .map_err(Error::GetHistoryVisibility)?;
        match visibility {
            Some(HistoryVisibility::WorldReadable)
            | Some(HistoryVisibility::Shared)
            | None => Ok(()),
            Some(visibility) => Err(Error::RestrictedHistoryVisibility {
                visibility,
                already_joined: !need_join,
            }),
        }
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
                self.plan.errors.push(e.into());
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
                self.plan.errors.push(e.into());
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

        self.plan
            .errors
            .extend(errors.into_iter().map(PlanError::IgnoredUserMerge));
        let merged = merged.map(|merged| {
            Raw::new(&merged)
                .expect("serialization should always succeed")
                .cast()
        });

        Ok(merged)
    }
}

pub(crate) async fn make_plan<S: StateAccessor>(
    settings: PlanSettings,
    old: &S,
    new: &S,
) -> Result<Plan<S>, FatalPlanError<S>> {
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
        settings,
        old,
        new,
        old_user_id,
        new_user_id: new_user_id.clone(),
        new_joined_rooms,

        plan: Plan {
            new_user_id,
            rooms: BTreeMap::new(),
            global_account_data: BTreeMap::new(),
            errors: vec![],
        },
    };

    for room_id in old_joined_rooms {
        state.plan_room(room_id).await;
    }

    state.plan_account_data_direct().await;
    state.plan_account_data_ignored_users().await;

    Ok(state.plan)
}

impl<S: StateAccessor> fmt::Display for Plan<S> {
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
            if room.leave {
                write!(f, "leave")?;
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
    use std::{fs, path::Path};

    use insta::assert_json_snapshot;
    use serde::Deserialize;

    use super::*;
    use crate::state::mock::{MockState, MockStateAccessor};

    #[derive(Deserialize)]
    struct TestInput {
        #[serde(default)]
        settings: PlanSettings,
        #[serde(flatten)]
        state: MockState,
    }

    async fn run_test(path: &Path) {
        let input_source = fs::read(path).unwrap();
        let input =
            serde_json5::from_slice::<TestInput>(&input_source).unwrap();

        let old = MockStateAccessor::new(UserKind::Old, &input.state);
        let new = MockStateAccessor::new(UserKind::New, &input.state);
        let plan = make_plan(input.settings, &old, &new).await.unwrap();
        // Needed because assert_json_snapshot! otherwise mangles Raw<_>
        let plan_json = serde_json::to_value(&plan).unwrap();

        insta::with_settings!({ snapshot_path => "../tests/output" }, {
            assert_json_snapshot!(plan_json);
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
