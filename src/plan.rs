use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

use ruma::{
    events::room::{join_rules::JoinRule, member::MembershipState},
    Int, OwnedRoomAliasId, OwnedRoomId,
};
use thiserror::Error;
use tracing as t;

use crate::user::{GetJoinedRoomsError, GetStateEventError, User, UserKind};

pub(crate) struct RoomPlan {
    pub(crate) alias: Option<OwnedRoomAliasId>,
    pub(crate) invite: bool,
    pub(crate) join: bool,
    pub(crate) power_level: Option<Int>,
}

pub(crate) struct Plan {
    pub(crate) rooms: BTreeMap<OwnedRoomId, RoomPlan>,
}

#[derive(Error, Debug)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum MakePlanError {
    #[error("failed to get joined room list for {_0} user")]
    GetJoinedRooms(UserKind, #[source] GetJoinedRoomsError),

    #[error("failed to get join rules for room {_0}")]
    GetJoinRule(OwnedRoomId, #[source] GetStateEventError),

    #[error("failed to get new user membership state in room {_0}")]
    GetMembership(OwnedRoomId, #[source] GetStateEventError),

    #[error("failed to get power levels state room {_0}")]
    GetPowerLevels(OwnedRoomId, #[source] GetStateEventError),
}

pub(crate) async fn make_plan(
    old: &User,
    new: &User,
) -> Result<Plan, MakePlanError> {
    use MakePlanError as Error;

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
            .get_membership(&room_id, &new.user_id)
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
            let can_invite = power_levels.user_can_invite(&old.user_id);

            if !can_invite {
                t::warn!(
                    "old user does not have permissions to invite new user to \
                     {room_str}"
                );
                continue;
            }
        }

        let old_power_level = power_levels.for_user(&old.user_id);
        let new_power_level = power_levels.for_user(&new.user_id);
        let set_power_level = if new_power_level < old_power_level {
            if power_levels
                .user_can_change_user_power_level(&old.user_id, &new.user_id)
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
        rooms,
    })
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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
