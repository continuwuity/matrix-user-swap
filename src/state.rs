use std::sync::OnceLock;

use ruma::{
    OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId, ServerName, UserId,
    api::{
        self,
        client::{
            error::ErrorKind, membership::invite_user::v3::InvitationRecipient,
        },
        error::FromHttpResponseError,
    },
    events::{
        AnyGlobalAccountDataEventContent, AnyRoomAccountDataEventContent,
        EmptyStateKey, GlobalAccountDataEventType, RoomAccountDataEventType,
        StateEventType,
        room::{
            create::RoomCreateEventContent,
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
use ruma_client as client;
use serde::{Deserialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{RumaError, rate_limit::RateLimitedClient, sync::SyncLoop};

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

    async fn get_room_version(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<String>, Self::Error> {
        #[derive(Deserialize)]
        struct Extract {
            room_version: Option<String>,
        }
        let extract = self
            .get_state_event::<Extract>(
                room_id,
                StateEventType::RoomCreate,
                "".to_owned(),
            )
            .await?;
        Ok(extract.and_then(|e| e.room_version))
    }

    /// Get the list of room creators for a room.
    /// 
    /// Room creators have infinite power level ONLY in room version 12 and later.
    /// In earlier room versions, creators are just regular users and their power
    /// level is determined by the m.room.power_levels event.
    /// 
    /// In room versions 1-10, there is a single creator field.
    /// In room version 11, the creator field is deprecated in favor of using the
    /// m.room.create event's sender.
    /// In room version 12+, additional_creators can specify multiple creators,
    /// all of whom have infinite power level.
    /// 
    /// Returns an empty vector if the room create event is not found or has no creators.
    async fn get_room_creators(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<OwnedUserId>, Self::Error> {
        let create_event = self
            .get_state_event::<RoomCreateEventContent>(
                room_id,
                StateEventType::RoomCreate,
                "".to_owned(),
            )
            .await?;
        
        if let Some(create) = create_event {
            let mut creators = Vec::new();
            
            // Add the original creator (deprecated in room version 11+)
            #[allow(deprecated)]
            if let Some(creator) = create.creator {
                creators.push(creator);
            }
            
            // Add any additional creators from room version 11+
            if !create.additional_creators.is_empty() {
                creators.extend(create.additional_creators);
            }
            
            Ok(creators)
        } else {
            Ok(Vec::new())
        }
    }

    async fn get_power_levels(
        &self,
        room_id: &RoomId,
    ) -> Result<RoomPowerLevels, Self::Error> {
        // We only care about the keys that are preserved on redaction, so just
        // deserialize to the redacted type. Redactable fields will be dropped.
        // 
        // This method also retrieves room creators and the room version to properly
        // construct RoomPowerLevels with the correct authorization rules. 
        // Note: Room creators only have infinite power level in room version 12+.
        // In earlier versions, they are just regular users.
        let content = self
            .get_state_event::<RedactedRoomPowerLevelsEventContent>(
                room_id,
                StateEventType::RoomPowerLevels,
                "".to_owned(),
            )
            .await?;
        
        // Get room creators for proper power level handling
        let creators = self.get_room_creators(room_id).await.unwrap_or_default();
        
        // Get room version to determine which authorization rules to use
        let room_version = self.get_room_version(room_id).await.unwrap_or_default();
        let rules = get_authorization_rules(room_version.as_deref());
        
        // Use RoomPowerLevelsSource to convert from the redacted content
        use ruma::events::room::power_levels::RoomPowerLevelsSource;
        let source = RoomPowerLevelsSource::from(content);
        Ok(RoomPowerLevels::new(
            source,
            rules,
            creators,
        ))
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

    async fn wait_for_invite(
        &self,
        room_id: &RoomId,
    ) -> Result<(), Self::Error>;
}

/// Get the appropriate authorization rules for a room version.
/// Returns V11 rules as the default for unknown versions.
fn get_authorization_rules(room_version: Option<&str>) -> &'static ruma::room_version_rules::AuthorizationRules {
    use ruma::room_version_rules::AuthorizationRules;
    match room_version {
        Some("1") | Some("2") => &AuthorizationRules::V1,
        Some("3") | Some("4") | Some("5") => &AuthorizationRules::V3,
        Some("6") => &AuthorizationRules::V6,
        Some("7") => &AuthorizationRules::V7,
        Some("8") | Some("9") => &AuthorizationRules::V8,
        Some("10") => &AuthorizationRules::V10,
        Some("11") => &AuthorizationRules::V11,
        Some("12") => &AuthorizationRules::V12,
        _ => &AuthorizationRules::V11, // Default for unknown versions
    }
}

pub(crate) trait WriteState {
    type Error: std::error::Error + 'static;

    async fn invite(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<(), Self::Error>;

    async fn join(
        &self,
        room_id: &RoomId,
        via: Option<&ServerName>,
    ) -> Result<(), Self::Error>;

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
    client: RateLimitedClient,
    user_id: OwnedUserId,
    sync_loop: OnceLock<SyncLoop>,
}

impl ClientStateAccessor {
    pub(crate) async fn new(
        client: RateLimitedClient,
    ) -> Result<ClientStateAccessor, ClientReadStateError> {
        let request = api::client::account::whoami::v3::Request::new();
        let response = client.send_request(request).await?;
        Ok(ClientStateAccessor {
            client,
            user_id: response.user_id,
            sync_loop: OnceLock::new(),
        })
    }

    pub(crate) fn inner(&self) -> &RateLimitedClient {
        &self.client
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
            api::client::state::get_state_event_for_key::v3::Request::new(
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
            Err(err) if err.error_kind() == Some(&ErrorKind::NotFound) => {
                return Ok(None);
            }
            Err(client::Error::FromHttpResponse(
                FromHttpResponseError::Server(e),
            )) if e.status_code.as_u16() == 404 => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let content = Raw::from_json(response.event_or_content)
            .deserialize()
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
            Err(err) if err.error_kind() == Some(&ErrorKind::NotFound) => {
                return Ok(None);
            }
            Err(client::Error::FromHttpResponse(
                FromHttpResponseError::Server(e),
            )) if e.status_code.as_u16() == 404 => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        // Deserialize the Raw<AnyGlobalAccountDataEventContent> by converting through JSON
        let json_value = response.account_data.json();
        let content: T = serde_json::from_str(json_value.get())
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
            Err(err) if err.error_kind() == Some(&ErrorKind::NotFound) => {
                return Ok(None);
            }
            Err(client::Error::FromHttpResponse(
                FromHttpResponseError::Server(e),
            )) if e.status_code.as_u16() == 404 => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        // Deserialize the Raw<AnyRoomAccountDataEventContent> by converting through JSON
        let json_value = response.account_data.json();
        let content: T = serde_json::from_str(json_value.get())
            .map_err(|e| Error::RoomAccountDataEventDeserialize(kind, e))?;
        Ok(Some(content))
    }

    async fn wait_for_invite(
        &self,
        room_id: &RoomId,
    ) -> Result<(), Self::Error> {
        let sync_loop =
            self.sync_loop.get_or_init(|| SyncLoop::new(self.client.clone()));
        sync_loop.wait_for_invite(room_id.to_owned()).await;
        Ok(())
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

    async fn join(
        &self,
        room_id: &RoomId,
        via: Option<&ServerName>,
    ) -> Result<(), Self::Error> {
        let mut request =
            api::client::membership::join_room_by_id_or_alias::v3::Request::new(
                room_id.to_owned().into(),
            );
        request.via = via.into_iter().map(|server| server.to_owned()).collect();
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
