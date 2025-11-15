use std::{collections::HashSet, time::Duration};

use impl_tools::autoimpl;
use indicatif::ProgressStyle;
use ruma::{
    Int, OwnedRoomId,
    events::{
        AnyGlobalAccountDataEventContent, GlobalAccountDataEventType,
        RoomAccountDataEventType,
        room::power_levels::RoomPowerLevelsEventContent,
    },
    serde::Raw,
};
use thiserror::Error;
use tracing as t;
use tracing::Span;
use tracing_indicatif::span_ext::IndicatifSpanExt;
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

#[derive(Error)]
#[autoimpl(Debug where S: ReadState + WriteState)]
enum PowerLevelError<S: ReadState + WriteState> {
    #[error("failed to get current power levels state")]
    Read(#[source] <S as ReadState>::Error),
    #[error("failed to set new power levels state")]
    Write(#[source] <S as WriteState>::Error),
}

#[derive(Error)]
#[autoimpl(Debug where S: ReadState + WriteState)]
enum RoomError<S: ReadState + WriteState> {
    #[error("failed to invite new user to room")]
    Invite(#[source] <S as WriteState>::Error),
    #[error("failed waiting for new user to receive invite to room")]
    WaitInvite(#[source] <S as ReadState>::Error),
    #[error("failed to join room room as new user")]
    Join(#[source] <S as WriteState>::Error),
    #[error("failed to leave room room as old user")]
    Leave(#[source] <S as WriteState>::Error),
    #[error("failed copy old user's power level to new user")]
    PowerLevel(#[from] PowerLevelError<S>),
    #[error("failed set {event_type} account data event")]
    AccountData {
        event_type: RoomAccountDataEventType,
        #[source]
        error: <S as WriteState>::Error,
    },
}

#[derive(Error)]
#[autoimpl(Debug where S: WriteState)]
#[error("failed to set {event_type} global account data event")]
struct GlobalAccountDataError<S: WriteState> {
    event_type: GlobalAccountDataEventType,
    #[source]
    error: S::Error,
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

// TODO: the 'static bound should in theory be unnecessary, but it is required
// because the thiserror::Error derive macro is adding an unnecessary 'static
// bound on the Error impl. Maybe there's a way around this?
impl<S: ReadState + WriteState + 'static> ExecuteContext<'_, S> {
    fn error<E: std::error::Error>(
        &mut self,
        room: Option<OwnedRoomId>,
        error: E,
    ) {
        t::error!("{}", error.display_with_sources("\n  "));
        self.error_count += 1;
        if let Some(room) = room {
            self.failed_rooms.insert(room);
        } else {
            self.global_errors = true;
        }
    }

    async fn execute(&mut self) -> Result<(), ExecuteError> {
        let plan = self.plan;

        let span = Span::current();
        span.pb_reset();
        span.pb_set_length(u64::try_from(plan.rooms.len()).unwrap());
        span.pb_set_message("Migrating rooms");

        for (room_id, room) in &plan.rooms {
            let identity = RoomIdentity {
                id: room_id.clone(),
                alias: room.alias.clone(),
            };
            self.migrate_room(&identity, room).await;
            span.pb_inc(1);
        }

        span.pb_set_message("Migrating global data");

        for (kind, content) in &self.plan.global_account_data {
            self.migrate_global_account_data(kind, content.clone()).await;
        }

        let leave_count =
            self.plan.rooms.values().filter(|room| room.leave).count();
        if leave_count > 0 {
            if self.global_errors {
                t::warn!(
                    "Not leaving any rooms because there were global \
                     migration errors"
                );
            } else {
                t::info!("Leaving fully-migrated rooms");
                span.pb_reset();
                span.pb_set_length(u64::try_from(plan.rooms.len()).unwrap());
                span.pb_set_message("Leaving rooms");

                for (room_id, room) in &plan.rooms {
                    let identity = RoomIdentity {
                        id: room_id.clone(),
                        alias: room.alias.clone(),
                    };
                    self.leave_room(&identity, room).await;
                    if room.leave {
                        span.pb_inc(1);
                    }
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
        if let Err(error) = self.migrate_room_inner(room, plan).await {
            self.error(Some(room.id.to_owned()), error);
        }
    }

    async fn migrate_room_inner(
        &mut self,
        room: &RoomIdentity,
        plan: &RoomPlan<S>,
    ) -> Result<(), RoomError<S>> {
        use RoomError as Error;

        if plan.invite {
            t::info!("Inviting new user to room");
            self.old
                .invite(&room.id, &self.plan.new_user_id)
                .await
                .map_err(Error::Invite)?;
            self.new
                .wait_for_invite(&room.id)
                .await
                .map_err(Error::WaitInvite)?;
            // Synapse seems to have some issues with joins immediately after
            // receiving the invite over sync, so wait a little bit longer :(
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        if plan.join {
            t::info!("Joining room with new user");
            let old_server = self.plan.old_user_id.server_name();
            self.new
                .join(&room.id, Some(old_server))
                .await
                .map_err(Error::Join)?;
        }

        if let Some(power_level) = plan.power_level
            && let Err(error) = self.set_power_level(room, power_level).await
        {
            self.error(Some(room.id.to_owned()), Error::PowerLevel(error));
        }

        for (kind, content) in &plan.account_data {
            t::info!("Migrating {kind} account data event");

            let result = self
                .new
                .set_room_account_data_event(
                    &room.id,
                    kind.clone(),
                    content.clone(),
                )
                .await;
            if let Err(error) = result {
                self.error(
                    Some(room.id.to_owned()),
                    Error::<S>::AccountData {
                        event_type: kind.clone(),
                        error,
                    },
                );
            }
        }

        Ok(())
    }

    async fn set_power_level(
        &self,
        room: &RoomIdentity,
        power_level: Int,
    ) -> Result<(), PowerLevelError<S>> {
        use PowerLevelError as Error;

        t::info!("Copying old user's power level ({power_level}) to new user");

        // Note: If power_level is Int::MAX, this may be an approximation for a room creator
        // (see plan_power_level() for details on creator handling in room version 12+).
        let power_levels =
            self.old.get_power_levels(&room.id).await.map_err(Error::Read)?;
        let mut power_levels_content = RoomPowerLevelsEventContent::try_from(power_levels)
            .expect("power levels conversion should succeed");
        power_levels_content.users.insert(self.plan.new_user_id.clone(), power_level);
        self.old
            .set_power_levels(&room.id, &power_levels_content)
            .await
            .map_err(Error::Write)?;

        Ok(())
    }

    #[t::instrument(skip_all, fields(%event_type))]
    async fn migrate_global_account_data(
        &mut self,
        event_type: &GlobalAccountDataEventType,
        content: Raw<AnyGlobalAccountDataEventContent>,
    ) {
        use GlobalAccountDataError as Error;

        t::info!("Migrating global account data event");

        // TODO: if some rooms failed, we probably shouldn't put then in
        // m.direct. This kinda breaks the plan/executor abstraction though :(
        let result = self
            .new
            .set_global_account_data_event(event_type.clone(), content)
            .await;
        if let Err(error) = result {
            self.error(
                None,
                Error::<S> {
                    event_type: event_type.clone(),
                    error,
                },
            );
        }
    }

    #[t::instrument(skip_all, fields(%room))]
    async fn leave_room(&mut self, room: &RoomIdentity, plan: &RoomPlan<S>) {
        use RoomError as Error;

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
            self.error(Some(room.id.to_owned()), Error::<S>::Leave(error));
        }
    }
}

#[t::instrument(skip_all)]
pub(crate) async fn execute_plan<S: ReadState + WriteState + 'static>(
    plan: &Plan<S>,
    old: &S,
    new: &S,
) -> Result<(), ExecuteError> {
    let span = Span::current();
    span.pb_set_style(
        &ProgressStyle::with_template("{wide_bar} {pos}/{len} {msg}").unwrap(),
    );

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
