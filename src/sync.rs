use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{Arc, Mutex},
    time::Duration,
};

use ruma::{
    OwnedRoomId,
    api::{
        self,
        client::filter::{Filter, FilterDefinition, RoomEventFilter},
    },
    events::{StaticEventContent, room::create::RoomCreateEventContent},
    presence::PresenceState,
};
use tokio::sync::Notify;
use tracing as t;

use crate::rate_limit::RateLimitedClient;

#[derive(Debug)]
enum InviteState {
    /// The user has not been invited to this room yet, and there are tasks
    /// waiting on an invite to be received.
    Pending(Arc<Notify>),
    /// The user has already been invited to this room.
    Invited,
}

#[derive(Debug)]
pub(crate) struct SyncLoop {
    invites: Arc<Mutex<HashMap<OwnedRoomId, InviteState>>>,
}

impl SyncLoop {
    pub(crate) fn new(client: RateLimitedClient) -> SyncLoop {
        let invites = Default::default();
        tokio::spawn(sync_loop(client, Arc::clone(&invites)));
        SyncLoop {
            invites,
        }
    }

    /// Wait until the client receives an invite to `room_id`.
    pub(crate) async fn wait_for_invite(&self, room_id: OwnedRoomId) {
        let notified = {
            let mut invites = self.invites.lock().unwrap();
            let state = invites.entry(room_id).or_insert_with(|| {
                InviteState::Pending(Arc::new(Notify::new()))
            });

            match state {
                // We need to make sure to mark this task as waiting *before*
                // releasing the mutex. If we called `.notified()` outside of
                // the mutex period, it would be possible for the other tasks
                // to be notified before we start waiting, and we would wait
                // forever.
                InviteState::Pending(notify) => {
                    Arc::clone(notify).notified_owned()
                }
                InviteState::Invited => return,
            }
        };
        notified.await
    }
}

#[t::instrument(skip_all)]
async fn sync_loop(
    client: RateLimitedClient,
    invites: Arc<Mutex<HashMap<OwnedRoomId, InviteState>>>,
) {
    // We only care about determining whether the user has been invited to a
    // room, so filter down to only the create event. This should cause each
    // room to show up once (since each room has exactly one create event), and
    // we can tell which rooms are invites because they show up in the `invite`
    // field of the response.
    let mut filter = FilterDefinition::empty();
    filter.presence = Filter::ignore_all();
    filter.account_data = Filter::ignore_all();
    filter.room.account_data = RoomEventFilter::ignore_all();
    filter.room.ephemeral = RoomEventFilter::ignore_all();
    filter.room.state = RoomEventFilter::ignore_all();
    filter.room.timeline.types =
        Some(vec![RoomCreateEventContent::TYPE.to_owned()]);

    let mut request = api::client::sync::sync_events::v3::Request::new();
    request.filter = Some(
        api::client::sync::sync_events::v3::Filter::FilterDefinition(filter),
    );
    request.set_presence = PresenceState::Offline;
    request.timeout = Some(Duration::from_secs(30));

    loop {
        let response = match client.send_request(request.clone()).await {
            Ok(response) => response,
            Err(error) => {
                t::error!(
                    ?error,
                    "Sync request returned an error, aborting sync loop"
                );
                // TODO: propagate this error to `wait_for_invite`
                break;
            }
        };

        // Update shared invite state and notify any waiting tasks
        if !response.rooms.invite.is_empty() {
            let mut invites = invites.lock().unwrap();
            for room_id in response.rooms.invite.into_keys() {
                match invites.entry(room_id) {
                    Entry::Occupied(mut entry) => match entry.get() {
                        InviteState::Invited => (),
                        InviteState::Pending(notify) => {
                            t::debug!(room_id = %entry.key(), "Received invite");
                            notify.notify_waiters();
                            entry.insert(InviteState::Invited);
                        }
                    },
                    Entry::Vacant(entry) => {
                        t::debug!(room_id = %entry.key(), "Received invite");
                        entry.insert(InviteState::Invited);
                    }
                }
            }
        }

        request.since = Some(response.next_batch);
    }
}
