use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

use ruma::{
    events::room::{join_rules::JoinRule, member::MembershipState},
    Int, OwnedRoomAliasId, OwnedRoomId, OwnedUserId,
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

#[derive(Error, Debug)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum MakePlanError<S: StateAccessor> {
    #[error("failed to get user id for {_0} user")]
    GetUserId(UserKind, #[source] S::Error),

    #[error("failed to get joined room list for {_0} user")]
    GetJoinedRooms(UserKind, #[source] S::Error),

    #[error("failed to get join rules for room {_0}")]
    GetJoinRule(OwnedRoomId, #[source] S::Error),

    #[error("failed to get new user membership state in room {_0}")]
    GetMembership(OwnedRoomId, #[source] S::Error),

    #[error("failed to get power levels state room {_0}")]
    GetPowerLevels(OwnedRoomId, #[source] S::Error),
}

pub(crate) async fn make_plan<S: StateAccessor>(
    old: &S,
    new: &S,
) -> Result<Plan, MakePlanError<S>> {
    use MakePlanError as Error;

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
    let to_join = old_joined_rooms
        .into_iter()
        .filter(|room_id| !new_joined_rooms.contains(room_id))
        .collect::<Vec<_>>();
    t::info!("need to join {} rooms", to_join.len());

    let mut rooms = BTreeMap::new();
    for room_id in to_join {
        let alias = match old.get_room_alias(&room_id).await {
            Ok(alias) => alias,
            Err(e) => {
                t::warn!("failed to get alias for room {room_id}:\n  {e}");
                None
            }
        };
        let room_str = if let Some(alias) = &alias {
            &format!("{room_id} ({alias})")
        } else {
            room_id.as_str()
        };

        let membership = old
            .get_membership(&room_id, &new_user_id)
            .await
            .map_err(|e| Error::GetMembership(room_id.clone(), e))?
            .unwrap_or(MembershipState::Leave);
        let invited = match membership {
            MembershipState::Invite => true,
            // New user joined in between fetching the joined user list and now
            MembershipState::Join => continue,
            _ => false,
        };

        let join_rule = old
            .get_join_rule(&room_id)
            .await
            .map_err(|e| Error::GetJoinRule(room_id.clone(), e))?;

        let power_levels = old
            .get_power_levels(&room_id)
            .await
            .map_err(|e| Error::GetPowerLevels(room_id.clone(), e))?;

        // TODO: handle 'allow' field of 'm.room.join_rules', which could allow
        // us to skip invites.
        let need_invite =
            !invited && !matches!(join_rule, Some(JoinRule::Public));

        if need_invite {
            let can_invite = power_levels.user_can_invite(&old_user_id);

            if !can_invite {
                t::warn!(
                    "old user does not have permissions to invite new user to \
                     {room_str}"
                );
                continue;
            }
        }

        let old_power_level = power_levels.for_user(&old_user_id);
        let new_power_level = power_levels.for_user(&new_user_id);
        let set_power_level = if new_power_level < old_power_level {
            if power_levels
                .user_can_change_user_power_level(&old_user_id, &new_user_id)
            {
                Some(old_power_level)
            } else {
                t::warn!(
                    "old user cannot copy power level {old_power_level} to \
                     new user in {room_str}"
                );
                None
            }
        } else {
            None
        };

        rooms.insert(
            room_id.to_owned(),
            RoomPlan {
                alias,
                invite: need_invite,
                join: true,
                power_level: set_power_level,
            },
        );
    }

    Ok(Plan {
        new_user_id,
        rooms,
    })
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
        let plan = make_plan(&old, &new).await.unwrap();

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
