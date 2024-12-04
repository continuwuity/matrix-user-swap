use ruma::{
    api::{self, client::error::ErrorKind, error::FromHttpResponseError},
    client::{self, HttpClient},
    events::{
        room::{
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            member::MembershipState,
            power_levels::{
                RedactedRoomPowerLevelsEventContent, RoomPowerLevels,
                RoomPowerLevelsEventContent,
            },
        },
        StateEventType,
    },
    OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId, UserId,
};
use serde::{de::DeserializeOwned, Deserialize};
use thiserror::Error;

pub(crate) type Client = client::Client<client::http_client::Reqwest>;

pub(crate) type ReqwestError =
    <client::http_client::Reqwest as HttpClient>::Error;
pub(crate) type RumaError = client::Error<ReqwestError, api::client::Error>;

pub(crate) struct StateAccessor {
    client: Client,
}

#[derive(Error, Debug)]
pub(crate) enum InitStateAccessorError {
    #[error("failed to initialize matrix client")]
    InitClient(#[source] RumaError),
}

#[derive(Error, Debug)]
pub(crate) enum StateError {
    #[error("client api request failed")]
    Request(#[from] RumaError),

    #[error("{_0} event did not match expected schema")]
    StateEventDeserialize(StateEventType, #[source] serde_json::Error),
}

impl StateAccessor {
    pub(crate) async fn new(
        hs_url: String,
        access_token: String,
        http_client: client::http_client::Reqwest,
    ) -> Result<StateAccessor, InitStateAccessorError> {
        use InitStateAccessorError as Error;

        let client = client::Client::builder()
            .access_token(Some(access_token))
            .homeserver_url(hs_url)
            .http_client(http_client.clone())
            .await
            .map_err(Error::InitClient)?;

        Ok(StateAccessor {
            client,
        })
    }

    pub(crate) async fn get_user_id(&self) -> Result<OwnedUserId, StateError> {
        let request = api::client::account::whoami::v3::Request::new();
        let response = self.client.send_request(request).await?;
        Ok(response.user_id)
    }

    pub(crate) async fn get_state_event<T: DeserializeOwned>(
        &self,
        room_id: &RoomId,
        kind: StateEventType,
        state_key: String,
    ) -> Result<Option<T>, StateError> {
        use StateError as Error;

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
                return Ok(None)
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

    pub(crate) async fn get_room_alias(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OwnedRoomAliasId>, StateError> {
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

    pub(crate) async fn get_membership(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MembershipState>, StateError> {
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

    pub(crate) async fn get_power_levels(
        &self,
        room_id: &RoomId,
    ) -> Result<RoomPowerLevels, StateError> {
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

    pub(crate) async fn get_join_rule(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<JoinRule>, StateError> {
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

    pub(crate) async fn get_joined_rooms(
        &self,
    ) -> Result<Vec<OwnedRoomId>, StateError> {
        let request = api::client::membership::joined_rooms::v3::Request::new();
        let response = self.client.send_request(request).await?;
        Ok(response.joined_rooms)
    }
}
