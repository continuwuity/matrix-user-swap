use std::collections::HashSet;

use ruma::OwnedRoomId;
use thiserror::Error;
use tracing as t;
use wee_woo::ErrorExt;

use crate::{
    plan::{Plan, RoomPlan},
    state::{ReadState, WriteState},
    utils::RoomIdentity,
};

#[derive(Error, Debug)]
#[error(
    "migration encountered {error_count} unexpected errors. See previous logs \
     for details."
)]
pub(crate) struct ExecuteError {
    error_count: usize,
}

struct ExecuteContext<'a, S: ReadState + WriteState> {
    plan: &'a Plan<S>,
    old: &'a S,
    new: &'a S,

    /// Total count of errors that occurred in plan execution
    error_count: usize,
    /// Whether any global (not room-specific) errors occurred
    global_errors: bool,
    /// Which rooms had at least one error in plan execution
    failed_rooms: HashSet<OwnedRoomId>,
}

impl<S: ReadState + WriteState> ExecuteContext<'_, S> {
    fn error<E: std::error::Error>(
        &mut self,
        room: Option<OwnedRoomId>,
        message: &str,
        error: E,
    ) {
        t::error!("{}: {}", message, error.display_with_sources("\n  "));
        self.error_count += 1;
        if let Some(room) = room {
            self.failed_rooms.insert(room);
        } else {
            self.global_errors = true;
        }
    }

    async fn execute(&mut self) -> Result<(), ExecuteError> {
        let plan = self.plan;
        for (room_id, room) in &plan.rooms {
            let identity = RoomIdentity {
                id: room_id.clone(),
                alias: room.alias.clone(),
            };
            self.migrate_room(&identity, room).await;
        }

        let any_leaves = self.plan.rooms.values().any(|room| room.leave);
        if any_leaves {
            if self.global_errors {
                t::warn!(
                    "Not leaving any rooms because there were global \
                     migration errors"
                );
            } else {
                t::info!("Leaving fully-migrated rooms");
                for (room_id, room) in &plan.rooms {
                    let identity = RoomIdentity {
                        id: room_id.clone(),
                        alias: room.alias.clone(),
                    };
                    self.leave_room(&identity, room).await;
                }
            }
        }

        if self.error_count > 0 {
            Err(ExecuteError {
                error_count: self.error_count,
            })
        } else {
            Ok(())
        }
    }

    #[t::instrument(skip_all, fields(%room))]
    async fn migrate_room(&mut self, room: &RoomIdentity, plan: &RoomPlan<S>) {
        if plan.invite {
            t::info!("Inviting new user to room");
            if let Err(error) =
                self.old.invite(&room.id, &self.plan.new_user_id).await
            {
                self.error(
                    Some(room.id.to_owned()),
                    "Failed to invite new user to room",
                    error,
                );
                return;
            }
        }

        if plan.join {
            t::info!("Joining room with new user");
            if let Err(error) = self.new.join(&room.id).await {
                self.error(
                    Some(room.id.to_owned()),
                    "Failed to join room",
                    error,
                );
                return;
            }
        }
    }

    #[t::instrument(skip_all, fields(%room))]
    async fn leave_room(&mut self, room: &RoomIdentity, plan: &RoomPlan<S>) {
        if !plan.leave {
            return;
        }

        if self.failed_rooms.contains(&room.id) {
            t::warn!(
                "Not leaving room with old user because there were previous \
                 errors migrating this room"
            );
            return;
        }

        t::info!("Leaving room with old user");
        if let Err(error) = self.old.leave(&room.id).await {
            self.error(Some(room.id.to_owned()), "Failed to leave room", error);
        }
    }
}

pub(crate) async fn execute_plan<S: ReadState + WriteState>(
    plan: &Plan<S>,
    old: &S,
    new: &S,
) -> Result<(), ExecuteError> {
    let mut ctx = ExecuteContext {
        plan,
        old,
        new,

        error_count: 0,
        global_errors: false,
        failed_rooms: HashSet::new(),
    };
    ctx.execute().await
}
