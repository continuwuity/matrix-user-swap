use std::collections::HashMap;

use ruma::{
    events::{
        room::member::MembershipState, GlobalAccountDataEventType,
        RoomAccountDataEventType, StateEventType,
    },
    OwnedRoomId, OwnedUserId, RoomId,
};
use serde::{de::DeserializeOwned, Deserialize};
use thiserror::Error;

use crate::{state::ReadState, UserKind};

#[derive(Debug, Deserialize)]
struct StateEvent {
    #[serde(rename = "type")]
    kind: StateEventType,
    state_key: String,
    // We can't use ruma::serde::Raw because the source is json5, not json
    content: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct RawRoom {
    state_events: Vec<StateEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "RawRoom")]
struct Room {
    state_events: HashMap<(StateEventType, String), serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct User {
    user_id: OwnedUserId,
    #[serde(default)]
    global_account_data: HashMap<GlobalAccountDataEventType, serde_json::Value>,
    #[serde(default)]
    room_account_data: HashMap<
        OwnedRoomId,
        HashMap<RoomAccountDataEventType, serde_json::Value>,
    >,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MockState {
    rooms: HashMap<OwnedRoomId, Room>,
    old_user: User,
    new_user: User,
}

#[derive(Debug)]
pub(crate) struct MockStateAccessor<'a> {
    kind: UserKind,
    state: &'a MockState,
}

#[derive(Error, Debug)]
#[error("duplicate state events with type {kind} and state key {state_key}")]
pub(crate) struct DuplicateStateError {
    kind: StateEventType,
    state_key: String,
}

impl TryFrom<RawRoom> for Room {
    type Error = DuplicateStateError;

    fn try_from(raw: RawRoom) -> Result<Room, DuplicateStateError> {
        let mut state_events = HashMap::new();
        for event in raw.state_events {
            let key = (event.kind, event.state_key);
            let duplicate = state_events.insert(key.clone(), event.content);
            if duplicate.is_some() {
                return Err(DuplicateStateError {
                    kind: key.0,
                    state_key: key.1,
                });
            }
        }
        Ok(Room {
            state_events,
        })
    }
}

impl<'a> MockStateAccessor<'a> {
    pub(crate) fn new(
        kind: UserKind,
        state: &'a MockState,
    ) -> MockStateAccessor<'a> {
        MockStateAccessor {
            kind,
            state,
        }
    }

    fn user(&self) -> &'a User {
        match self.kind {
            UserKind::Old => &self.state.old_user,
            UserKind::New => &self.state.new_user,
        }
    }
}

#[derive(Error, Debug)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum MockReadStateError {
    #[error("{_0} event did not match expected schema")]
    StateEventDeserialize(StateEventType, #[source] serde_json::Error),

    #[error("{_0} event did not match expected schema")]
    GlobalAccountDataEventDeserialize(
        GlobalAccountDataEventType,
        #[source] serde_json::Error,
    ),

    #[error("{_0} event did not match expected schema")]
    RoomAccountDataEventDeserialize(
        RoomAccountDataEventType,
        #[source] serde_json::Error,
    ),
}

impl ReadState for MockStateAccessor<'_> {
    type Error = MockReadStateError;

    async fn get_user_id(&self) -> Result<OwnedUserId, MockReadStateError> {
        Ok(self.user().user_id.clone())
    }

    async fn get_state_event<T: DeserializeOwned>(
        &self,
        room_id: &RoomId,
        kind: StateEventType,
        state_key: String,
    ) -> Result<Option<T>, MockReadStateError> {
        use MockReadStateError as Error;

        let key = (kind, state_key);
        let Some(room) = self.state.rooms.get(room_id) else {
            return Ok(None);
        };
        let Some(content) = room.state_events.get(&key) else {
            return Ok(None);
        };
        let content = serde_json::from_value(content.clone())
            .map_err(|e| Error::StateEventDeserialize(key.0, e))?;
        Ok(Some(content))
    }

    async fn get_global_account_data_event<T: DeserializeOwned>(
        &self,
        kind: GlobalAccountDataEventType,
    ) -> Result<Option<T>, MockReadStateError> {
        use MockReadStateError as Error;

        let Some(content) = self.user().global_account_data.get(&kind) else {
            return Ok(None);
        };
        let content = serde_json::from_value(content.clone())
            .map_err(|e| Error::GlobalAccountDataEventDeserialize(kind, e))?;
        Ok(Some(content))
    }

    async fn get_room_account_data_event<T: DeserializeOwned>(
        &self,
        room_id: &RoomId,
        kind: RoomAccountDataEventType,
    ) -> Result<Option<T>, MockReadStateError> {
        use MockReadStateError as Error;

        let Some(room) = self.user().room_account_data.get(room_id) else {
            return Ok(None);
        };
        let Some(content) = room.get(&kind) else {
            return Ok(None);
        };
        let content = serde_json::from_value(content.clone())
            .map_err(|e| Error::RoomAccountDataEventDeserialize(kind, e))?;
        Ok(Some(content))
    }

    async fn get_joined_rooms(
        &self,
    ) -> Result<Vec<OwnedRoomId>, MockReadStateError> {
        let user_id = self.get_user_id().await?;
        let mut rooms = vec![];
        for room_id in self.state.rooms.keys() {
            let membership = self.get_membership(room_id, &user_id).await?;
            if membership == Some(MembershipState::Join) {
                rooms.push(room_id.to_owned())
            }
        }
        Ok(rooms)
    }
}
