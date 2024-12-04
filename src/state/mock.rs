use std::{collections::HashMap, fs, io, path::Path};

use ruma::{
    events::{room::member::MembershipState, StateEventType},
    OwnedRoomId, OwnedUserId, RoomId,
};
use serde::{de::DeserializeOwned, Deserialize};
use thiserror::Error;

use crate::{state::StateAccessor, UserKind};

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
pub(crate) enum LoadMockStateError {
    #[error("error reading state file")]
    Read(#[source] io::Error),

    #[error("error deserializing state file")]
    Deserialize(#[source] serde_json5::Error),

    #[error(
        "duplicate state events with type {kind} and state key {state_key}"
    )]
    DuplicateState {
        kind: StateEventType,
        state_key: String,
    },
}

impl TryFrom<RawRoom> for Room {
    type Error = LoadMockStateError;

    fn try_from(raw: RawRoom) -> Result<Room, LoadMockStateError> {
        use LoadMockStateError as Error;

        let mut state_events = HashMap::new();
        for event in raw.state_events {
            let key = (event.kind, event.state_key);
            let duplicate = state_events.insert(key.clone(), event.content);
            if duplicate.is_some() {
                return Err(Error::DuplicateState {
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

impl MockState {
    pub(crate) fn new<P: AsRef<Path>>(
        path: P,
    ) -> Result<MockState, LoadMockStateError> {
        use LoadMockStateError as Error;

        let contents = fs::read(path).map_err(Error::Read)?;
        serde_json5::from_slice(&contents).map_err(Error::Deserialize)
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
pub(crate) enum MockStateError {
    #[error("{_0} event did not match expected schema")]
    StateEventDeserialize(StateEventType, #[source] serde_json::Error),
}

impl StateAccessor for MockStateAccessor<'_> {
    type Error = MockStateError;

    async fn get_user_id(&self) -> Result<OwnedUserId, MockStateError> {
        Ok(self.user().user_id.clone())
    }

    async fn get_state_event<T: DeserializeOwned>(
        &self,
        room_id: &RoomId,
        kind: StateEventType,
        state_key: String,
    ) -> Result<Option<T>, MockStateError> {
        use MockStateError as Error;

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

    async fn get_joined_rooms(
        &self,
    ) -> Result<Vec<OwnedRoomId>, MockStateError> {
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
