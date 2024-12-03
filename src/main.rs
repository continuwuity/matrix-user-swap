use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    process::ExitCode,
};

use clap::Parser;
use derive_more::Display;
use ruma::{
    api::{self, client::error::ErrorKind, error::FromHttpResponseError},
    client::{self, HttpClient},
    events::{
        room::{
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
use tracing::{self as t, level_filters::LevelFilter};
use tracing_subscriber::{self, prelude::*};
use wee_woo::ErrorExt;

// TODO: support server discovery from userid
// TODO: support password auth
#[derive(Parser)]
struct Cli {
    #[clap(long)]
    old_access_token: String,
    #[clap(long)]
    old_hs_url: String,

    #[clap(long)]
    new_access_token: String,
    #[clap(long)]
    new_hs_url: String,
}

#[derive(Debug, Display, Copy, Clone)]
enum UserKind {
    #[display("old")]
    Old,
    #[display("new")]
    New,
}

type Client = client::Client<client::http_client::Reqwest>;

type ReqwestError = <client::http_client::Reqwest as HttpClient>::Error;
type RumaError = client::Error<ReqwestError, api::client::Error>;

#[derive(Error, Debug)]
enum Error {
    #[error("failed to initialize logging")]
    InitLogging(#[from] InitLoggingError),

    #[error("failed to initialize {_0} user")]
    InitUser(UserKind, #[source] InitUserError),

    #[error("failed to compute migration plan")]
    MakePlan(#[from] MakePlanError),
}

#[derive(Error, Debug)]
enum InitLoggingError {
    #[error(
        "failed to parse filter from MATRIX_USER_SWAP_LOG environment variable"
    )]
    ParseEnvFilter(#[from] tracing_subscriber::filter::FromEnvError),
}

#[derive(Error, Debug)]
enum InitUserError {
    #[error("failed to initialize matrix client")]
    InitClient(#[source] RumaError),

    #[error("failed to get user id")]
    GetUserId(#[source] RumaError),
}

#[derive(Error, Debug)]
enum GetStateEventError {
    #[error("failed to get {_0} event from server")]
    Request(StateEventType, #[source] RumaError),

    #[error("{_0} event did not match expected schema")]
    Deserialize(StateEventType, #[source] serde_json::Error),
}

#[derive(Error, Debug)]
#[allow(clippy::enum_variant_names)]
enum MakePlanError {
    #[error("failed to get joined room list for {_0} user")]
    GetJoinedRooms(UserKind, #[source] RumaError),

    #[error("failed to get new user membership state in room {_0}")]
    GetMembership(OwnedRoomId, #[source] GetStateEventError),

    #[error("failed to get power levels state room {_0}")]
    GetPowerLevels(OwnedRoomId, #[source] GetStateEventError),
}

fn init_logging() -> Result<(), InitLoggingError> {
    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("MATRIX_USER_SWAP_LOG")
        .from_env()?;
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(env_filter)
        .init();
    Ok(())
}

struct User {
    kind: UserKind,
    user_id: OwnedUserId,
    client: Client,
}

impl User {
    async fn new(
        kind: UserKind,
        hs_url: String,
        access_token: String,
        http_client: client::http_client::Reqwest,
    ) -> Result<User, InitUserError> {
        use InitUserError as Error;

        let client = client::Client::builder()
            .access_token(Some(access_token))
            .homeserver_url(hs_url)
            .http_client(http_client.clone())
            .await
            .map_err(Error::InitClient)?;

        let request = api::client::account::whoami::v3::Request::new();
        let response =
            client.send_request(request).await.map_err(Error::GetUserId)?;
        let user_id = response.user_id;

        Ok(User {
            kind,
            user_id,
            client,
        })
    }

    async fn get_state_event<T: DeserializeOwned>(
        &self,
        room_id: &RoomId,
        kind: StateEventType,
        state_key: String,
    ) -> Result<Option<T>, GetStateEventError> {
        use GetStateEventError as Error;

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
            Err(e) => return Err(Error::Request(kind, e)),
        };

        let content = response
            .content
            .deserialize_as::<T>()
            .map_err(|e| Error::Deserialize(kind, e))?;
        Ok(Some(content))
    }

    async fn get_room_alias(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OwnedRoomAliasId>, GetStateEventError> {
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
    ) -> Result<Option<MembershipState>, GetStateEventError> {
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
    ) -> Result<RoomPowerLevels, GetStateEventError> {
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

    async fn get_joined_rooms(&self) -> Result<Vec<OwnedRoomId>, RumaError> {
        let request = api::client::membership::joined_rooms::v3::Request::new();
        let response = self.client.send_request(request).await?;
        Ok(response.joined_rooms)
    }
}

struct RoomPlan {
    alias: Option<OwnedRoomAliasId>,
    invite: bool,
    join: bool,
}

struct Plan {
    rooms: BTreeMap<OwnedRoomId, RoomPlan>,
}

async fn make_plan(old: &User, new: &User) -> Result<Plan, MakePlanError> {
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
        let invite = match membership {
            MembershipState::Invite => false,
            // New user joined in between fetching the joined user list and now
            MembershipState::Join => continue,
            _ => true,
        };

        if invite {
            let power_levels = old
                .get_power_levels(&room_id)
                .await
                .map_err(|e| Error::GetPowerLevels(room_id.clone(), e))?;
            let can_invite = power_levels.user_can_invite(&old.user_id);

            if !can_invite {
                t::warn!(
                    "old user does not have permissions to invite new user to \
                     {room_str}"
                );
                continue;
            }
        }

        rooms.insert(
            room_id.to_owned(),
            RoomPlan {
                alias,
                invite,
                join: true,
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
            writeln!(f)?;
        }
        Ok(())
    }
}

async fn try_main() -> Result<(), Error> {
    init_logging()?;
    let cli = Cli::parse();

    let http_client = client::http_client::Reqwest::new();

    let old_user = User::new(
        UserKind::Old,
        cli.old_hs_url,
        cli.old_access_token,
        http_client.clone(),
    )
    .await
    .map_err(|e| Error::InitUser(UserKind::Old, e))?;
    let new_user = User::new(
        UserKind::New,
        cli.new_hs_url,
        cli.new_access_token,
        http_client,
    )
    .await
    .map_err(|e| Error::InitUser(UserKind::New, e))?;

    let plan = make_plan(&old_user, &new_user).await?;
    println!("{plan}");

    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(err) = try_main().await {
        eprintln!("Error: {}", err.display_with_sources("\n  Caused by: "));
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
