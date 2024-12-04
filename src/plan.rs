use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

use ruma::{
    events::room::{join_rules::JoinRule, member::MembershipState},
    Int, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId,
};
use serde::Serialize;
use thiserror::Error;
use tracing as t;

use crate::{state::StateAccessor, UserKind};

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
}

#[derive(Serialize)]
pub(crate) struct Plan {
    pub(crate) new_user_id: OwnedUserId,
    pub(crate) rooms: BTreeMap<OwnedRoomId, RoomPlan>,
}

struct MakePlanState<'a, S: StateAccessor> {
    old: &'a S,
    new: &'a S,
    new_user_id: OwnedUserId,
    old_user_id: OwnedUserId,
    new_joined_rooms: HashSet<OwnedRoomId>,
    errors: Vec<PlanError<S>>,
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
#[derive(Error, Debug)]
pub(crate) enum RoomPlanError<S: StateAccessor> {
    #[error("failed to get join rules")]
    GetJoinRule(#[source] S::Error),

    #[error("failed to get new user membership state")]
    GetMembership(#[source] S::Error),

    #[error("failed to get power levels")]
    GetPowerLevels(#[source] S::Error),

    #[error(
        "room is invite-only, but old user does not have permission to invite \
         new user"
    )]
    CannotInvite,
}

/// Non-fatal errors determining a migration plan.
///
/// These may block fully migrating particular rooms or data, but are not fatal
/// for the migration as a whole.
#[derive(Error, Debug)]
pub(crate) enum PlanError<S: StateAccessor> {
    #[error("cannot migrate room {_0}")]
    RoomFailed(OwnedRoomId, #[source] RoomPlanError<S>),

    #[error(
        "failed to get alias for room {_0}. This is mostly inconsequential, \
         and just might make it harder to identify the room in log messages."
    )]
    AliasFailed(OwnedRoomId, #[source] S::Error),

    #[error(
        "old user does not have permission to copy their power level \
         ({old_power_level}) to new user in room {room_id}"
    )]
    CannotCopyPowerLevel {
        room_id: OwnedRoomId,
        old_power_level: Int,
    },
}

impl RoomPlan {
    /// Returns `true` if no actions need to be taken for this room.
    fn is_empty(&self) -> bool {
        !self.invite && !self.join && self.power_level.is_none()
    }
}

impl<S: StateAccessor> MakePlanState<'_, S> {
    async fn make_room_plan(&mut self, room_id: &RoomId) -> Option<RoomPlan> {
        let result = self.make_room_plan_inner(room_id).await;
        match result {
            Ok(plan) => plan,
            Err(e) => {
                self.errors.push(PlanError::RoomFailed(room_id.to_owned(), e));
                None
            }
        }
    }

    async fn make_room_plan_inner(
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

        let room_plan = RoomPlan {
            alias,
            invite: need_invite,
            join: need_join,
            power_level: set_power_level,
        };

        Ok(if !room_plan.is_empty() {
            Some(room_plan)
        } else {
            None
        })
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
    let old_joined_rooms = old
        .get_joined_rooms()
        .await
        .map_err(|e| Error::GetJoinedRooms(UserKind::Old, e))?;

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
        new_user_id,
        old_user_id,
        new_joined_rooms,
        errors: vec![],
    };

    let mut rooms = BTreeMap::new();
    for room_id in old_joined_rooms {
        if let Some(room_plan) = state.make_room_plan(&room_id).await {
            rooms.insert(room_id, room_plan);
        }
    }

    let plan = Plan {
        new_user_id: state.new_user_id,
        rooms,
    };
    Ok((plan, state.errors))
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
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::path::Path;

    use insta::assert_json_snapshot;

    use super::*;
    use crate::state::mock::{MockState, MockStateAccessor};

    async fn run_test(path: &Path) {
        let state = MockState::new(path).unwrap();
        let old = MockStateAccessor::new(UserKind::Old, &state);
        let new = MockStateAccessor::new(UserKind::New, &state);
        // TODO: include errors in snapshot
        let (plan, _) = make_plan(&old, &new).await.unwrap();

        insta::with_settings!({ snapshot_path => "../tests/output" }, {
            assert_json_snapshot!(plan);
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
