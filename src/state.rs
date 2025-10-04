use ruma::{
    OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId, UserId,
    api::{
        self,
        client::{
            error::ErrorKind, membership::invite_user::v3::InvitationRecipient,
        },
        error::FromHttpResponseError,
    },
    client,
    events::{
        AnyGlobalAccountDataEventContent, AnyRoomAccountDataEventContent,
        EmptyStateKey, GlobalAccountDataEventType, RoomAccountDataEventType,
        StateEventType,
        room::{
            history_visibility::HistoryVisibility,
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            member::MembershipState,
            power_levels::{
                RedactedRoomPowerLevelsEventContent, RoomPowerLevels,
                RoomPowerLevelsEventContent,
            },
            server_acl::RoomServerAclEventContent,
        },
    },
    serde::Raw,
};
use serde::{Deserialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{Client, RumaError};

#[cfg(test)]
pub(crate) mod mock;

pub(crate) trait ReadState {
    type Error: std::error::Error + 'static;

    async fn get_user_id(&self) -> Result<OwnedUserId, Self::Error>;

    async fn get_joined_rooms(&self) -> Result<Vec<OwnedRoomId>, Self::Error>;

    async fn get_state_event<T: DeserializeOwned>(
        &self,
        room_id: &RoomId,
        kind: StateEventType,
        state_key: String,
    ) -> Result<Option<T>, Self::Error>;

    async fn get_global_account_data_event<T: DeserializeOwned>(
        &self,
        kind: GlobalAccountDataEventType,
    ) -> Result<Option<T>, Self::Error>;

    async fn get_room_account_data_event<T: DeserializeOwned>(
        &self,
        room: &RoomId,
        kind: RoomAccountDataEventType,
    ) -> Result<Option<T>, Self::Error>;

    async fn get_room_alias(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OwnedRoomAliasId>, Self::Error> {
        #[derive(Deserialize)]
        struct Extract {
            alias: Option<OwnedRoomAliasId>,
        }
        let extract = self
            .get_state_event::<Extract>(
                room_id,
                StateEventType::RoomCanonicalAlias,
                "".to_owned(),
            )
            .await?;
        Ok(extract.and_then(|extract| extract.alias))
    }

    async fn get_membership(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MembershipState>, Self::Error> {
        #[derive(Deserialize)]
        struct Extract {
            membership: MembershipState,
        }
        let extract = self
            .get_state_event::<Extract>(
                room_id,
                StateEventType::RoomMember,
                user_id.as_str().to_owned(),
            )
            .await?;
        Ok(extract.map(|extract| extract.membership))
    }

    async fn get_power_levels(
        &self,
        room_id: &RoomId,
    ) -> Result<RoomPowerLevels, Self::Error> {
        // We only care about the keys that are preserved on redaction, so just
        // deserialize to the redacted type. Redactable fields will be dropped.
        let content = self
            .get_state_event::<RedactedRoomPowerLevelsEventContent>(
                room_id,
                StateEventType::RoomPowerLevels,
                "".to_owned(),
            )
            .await?;
        if let Some(content) = content {
            Ok(content.into())
        } else {
            Ok(RoomPowerLevelsEventContent::default().into())
        }
    }

    async fn get_join_rule(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<JoinRule>, Self::Error> {
        // TODO: why doesn't using RedactedRoomJoinRulesEventContent here work?
        let content = self
            .get_state_event::<RoomJoinRulesEventContent>(
                room_id,
                StateEventType::RoomJoinRules,
                "".to_owned(),
            )
            .await?;
        Ok(content.map(|content| content.join_rule))
    }

    async fn get_server_acl(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<RoomServerAclEventContent>, Self::Error> {
        // TODO: don't error if the event was redacted, just return None
        let content = self
            .get_state_event::<RoomServerAclEventContent>(
                room_id,
                StateEventType::RoomServerAcl,
                "".to_owned(),
            )
            .await?;
        Ok(content)
    }

    async fn get_history_visibility(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<HistoryVisibility>, Self::Error> {
        #[derive(Deserialize)]
        struct Extract {
            history_visibility: Option<HistoryVisibility>,
        }

        let extract = self
            .get_state_event::<Extract>(
                room_id,
                StateEventType::RoomHistoryVisibility,
                "".to_owned(),
            )
            .await?;
        Ok(extract.and_then(|extract| extract.history_visibility))
    }
}

pub(crate) trait WriteState {
    type Error: std::error::Error + 'static;

    async fn invite(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<(), Self::Error>;

    async fn join(&self, room_id: &RoomId) -> Result<(), Self::Error>;

    async fn leave(&self, room_id: &RoomId) -> Result<(), Self::Error>;

    async fn set_power_levels(
        &self,
        room_id: &RoomId,
        power_levels: &RoomPowerLevelsEventContent,
    ) -> Result<(), Self::Error>;

    async fn set_global_account_data_event(
        &self,
        kind: GlobalAccountDataEventType,
        content: Raw<AnyGlobalAccountDataEventContent>,
    ) -> Result<(), Self::Error>;

    async fn set_room_account_data_event(
        &self,
        room: &RoomId,
        kind: RoomAccountDataEventType,
        content: Raw<AnyRoomAccountDataEventContent>,
    ) -> Result<(), Self::Error>;
}

#[derive(Error, Debug)]
pub(crate) enum ClientReadStateError {
    #[error("client api request failed")]
    Request(#[from] RumaError),

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

#[derive(Debug)]
pub(crate) struct ClientStateAccessor {
    client: Client,
    user_id: OwnedUserId,
}

impl ClientStateAccessor {
    pub(crate) async fn new(
        client: Client,
    ) -> Result<ClientStateAccessor, (ClientReadStateError, Client)> {
        let request = api::client::account::whoami::v3::Request::new();
        match client.send_request(request).await {
            Ok(response) => Ok(ClientStateAccessor {
                client,
                user_id: response.user_id,
            }),
            Err(error) => Err((error.into(), client)),
        }
    }

    pub(crate) fn into_inner(self) -> Client {
        self.client
    }
}

impl ReadState for ClientStateAccessor {
    type Error = ClientReadStateError;

    async fn get_user_id(&self) -> Result<OwnedUserId, ClientReadStateError> {
        Ok(self.user_id.clone())
    }

    async fn get_state_event<T: DeserializeOwned>(
        &self,
        room_id: &RoomId,
        kind: StateEventType,
        state_key: String,
    ) -> Result<Option<T>, ClientReadStateError> {
        use ClientReadStateError as Error;

        let request =
            api::client::state::get_state_events_for_key::v3::Request::new(
                room_id.to_owned(),
                kind.clone(),
                state_key,
            );
        let response = self.client.send_request(request).await;

        let response = match response {
            Ok(response) => response,
            // Spec says that "The room has no state with the given type or
            // key." is 404, but does not specify a errcode, so this
            // is the best we can do.
            Err(e) if e.error_kind() == Some(&ErrorKind::NotFound) => {
                return Ok(None);
            }
            Err(client::Error::FromHttpResponse(
                FromHttpResponseError::Server(e),
            )) if e.status_code.as_u16() == 404 => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let content = response
            .content
            .deserialize_as::<T>()
            .map_err(|e| Error::StateEventDeserialize(kind, e))?;
        Ok(Some(content))
    }

    async fn get_joined_rooms(
        &self,
    ) -> Result<Vec<OwnedRoomId>, ClientReadStateError> {
        let request = api::client::membership::joined_rooms::v3::Request::new();
        let response = self.client.send_request(request).await?;
        Ok(response.joined_rooms)
    }

    async fn get_global_account_data_event<T: DeserializeOwned>(
        &self,
        kind: GlobalAccountDataEventType,
    ) -> Result<Option<T>, Self::Error> {
        use ClientReadStateError as Error;

        let request =
            api::client::config::get_global_account_data::v3::Request::new(
                self.user_id.clone(),
                kind.clone(),
            );
        let response = self.client.send_request(request).await;

        let response = match response {
            Ok(response) => response,
            // Spec mentions a 404 response, but doesn't specify errcode or
            // semantics. This is the best we can do.
            Err(e) if e.error_kind() == Some(&ErrorKind::NotFound) => {
                return Ok(None);
            }
            Err(client::Error::FromHttpResponse(
                FromHttpResponseError::Server(e),
            )) if e.status_code.as_u16() == 404 => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let content = response
            .account_data
            .deserialize_as::<T>()
            .map_err(|e| Error::GlobalAccountDataEventDeserialize(kind, e))?;
        Ok(Some(content))
    }

    async fn get_room_account_data_event<T: DeserializeOwned>(
        &self,
        room_id: &RoomId,
        kind: RoomAccountDataEventType,
    ) -> Result<Option<T>, Self::Error> {
        use ClientReadStateError as Error;

        let request =
            api::client::config::get_room_account_data::v3::Request::new(
                self.user_id.clone(),
                room_id.to_owned(),
                kind.clone(),
            );
        let response = self.client.send_request(request).await;

        let response = match response {
            Ok(response) => response,
            // Spec mentions a 404 response, but doesn't specify errcode or
            // semantics. This is the best we can do.
            Err(e) if e.error_kind() == Some(&ErrorKind::NotFound) => {
                return Ok(None);
            }
            Err(client::Error::FromHttpResponse(
                FromHttpResponseError::Server(e),
            )) if e.status_code.as_u16() == 404 => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let content = response
            .account_data
            .deserialize_as::<T>()
            .map_err(|e| Error::RoomAccountDataEventDeserialize(kind, e))?;
        Ok(Some(content))
    }
}

#[derive(Error, Debug)]
pub(crate) enum ClientWriteStateError {
    #[error("client api request failed")]
    Request(#[from] RumaError),

    #[error("serializing {event_type} event failed")]
    Serialize {
        event_type: StateEventType,
        #[source]
        error: serde_json::Error,
    },
}

impl WriteState for ClientStateAccessor {
    type Error = ClientWriteStateError;

    async fn invite(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<(), Self::Error> {
        let recipient = InvitationRecipient::UserId {
            user_id: user_id.to_owned(),
        };
        let request = api::client::membership::invite_user::v3::Request::new(
            room_id.to_owned(),
            recipient,
        );
        self.client.send_request(request).await?;
        Ok(())
    }

    async fn join(&self, room_id: &RoomId) -> Result<(), Self::Error> {
        let request =
            api::client::membership::join_room_by_id::v3::Request::new(
                room_id.to_owned(),
            );
        self.client.send_request(request).await?;
        Ok(())
    }

    async fn leave(&self, room_id: &RoomId) -> Result<(), Self::Error> {
        let request = api::client::membership::leave_room::v3::Request::new(
            room_id.to_owned(),
        );
        self.client.send_request(request).await?;
        Ok(())
    }

    async fn set_power_levels(
        &self,
        room_id: &RoomId,
        power_levels: &RoomPowerLevelsEventContent,
    ) -> Result<(), Self::Error> {
        use ClientWriteStateError as Error;

        let request = api::client::state::send_state_event::v3::Request::new(
            room_id.to_owned(),
            &EmptyStateKey,
            power_levels,
        )
        .map_err(|error| Error::Serialize {
            event_type: StateEventType::RoomPowerLevels,
            error,
        })?;
        self.client.send_request(request).await?;
        Ok(())
    }

    async fn set_global_account_data_event(
        &self,
        kind: GlobalAccountDataEventType,
        content: Raw<AnyGlobalAccountDataEventContent>,
    ) -> Result<(), Self::Error> {
        let request =
            api::client::config::set_global_account_data::v3::Request::new_raw(
                self.user_id.clone(),
                kind,
                content,
            );
        self.client.send_request(request).await?;
        Ok(())
    }

    async fn set_room_account_data_event(
        &self,
        room: &RoomId,
        kind: RoomAccountDataEventType,
        content: Raw<AnyRoomAccountDataEventContent>,
    ) -> Result<(), Self::Error> {
        let request =
            api::client::config::set_room_account_data::v3::Request::new_raw(
                self.user_id.clone(),
                room.to_owned(),
                kind,
                content,
            );
        self.client.send_request(request).await?;
        Ok(())
    }
}
