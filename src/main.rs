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
    events::{room::member::MembershipState, StateEventType},
    OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomId,
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

    #[error("failed to get state for {_0} user")]
    GetState(UserKind, #[source] GetStateError),
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
enum GetStateError {
    #[error("failed to get joined room list")]
    GetJoinedRooms(#[source] RumaError),
}

#[derive(Error, Debug)]
enum GetStateEventError {
    #[error("failed to get {_0} event from server")]
    Request(StateEventType, #[source] RumaError),

    #[error("{_0} event did not match expected schema")]
    Deserialize(StateEventType, #[source] serde_json::Error),
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
    ) -> Result<Option<MembershipState>, GetStateEventError> {
        #[derive(Deserialize)]
        struct Extract {
            membership: MembershipState,
        }
        let extract = self
            .get_state_event::<Extract>(
                room_id,
                StateEventType::RoomMember,
                self.user_id.as_str().to_owned(),
            )
            .await?;
        Ok(extract.map(|extract| extract.membership))
    }

    async fn get_joined_rooms(&self) -> Result<Vec<OwnedRoomId>, RumaError> {
        let request = api::client::membership::joined_rooms::v3::Request::new();
        let response = self.client.send_request(request).await?;
        Ok(response.joined_rooms)
    }

    async fn get_state(&self) -> Result<State, GetStateError> {
        use GetStateError as Error;

        t::info!("fetching state for {} user", self.kind);

        t::info!("fetching list of joined rooms");
        let joined_rooms =
            self.get_joined_rooms().await.map_err(Error::GetJoinedRooms)?;

        Ok(State {
            joined_rooms,
        })
    }
}

struct State {
    joined_rooms: Vec<OwnedRoomId>,
}

struct StateDiff {
    join_rooms: Vec<OwnedRoomId>,
}

struct RoomPlan {
    alias: Option<OwnedRoomAliasId>,
    join: bool,
}

struct Plan {
    rooms: BTreeMap<OwnedRoomId, RoomPlan>,
}

impl State {
    fn diff_from(&self, other: &State) -> StateDiff {
        let already_joined = other.joined_rooms.iter().collect::<HashSet<_>>();
        StateDiff {
            join_rooms: self
                .joined_rooms
                .iter()
                .filter(|room_id| !already_joined.contains(room_id))
                .cloned()
                .collect(),
        }
    }
}

impl StateDiff {
    async fn to_plan(&self, user: &User) -> Plan {
        t::info!("fetching room aliases");

        let mut rooms = BTreeMap::new();
        for room_id in &self.join_rooms {
            let alias = match user.get_room_alias(room_id).await {
                Ok(alias) => alias,
                Err(e) => {
                    t::warn!("failed to get alias for room {room_id}:\n  {e}");
                    None
                }
            };

            rooms.insert(
                room_id.to_owned(),
                RoomPlan {
                    alias,
                    join: true,
                },
            );
        }

        Plan {
            rooms,
        }
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "Rooms:")?;
        for (id, room) in &self.rooms {
            write!(f, "  - {id} (")?;
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

    let old_state = old_user
        .get_state()
        .await
        .map_err(|e| Error::GetState(UserKind::Old, e))?;
    let new_state = new_user
        .get_state()
        .await
        .map_err(|e| Error::GetState(UserKind::New, e))?;

    let diff = old_state.diff_from(&new_state);
    let plan = diff.to_plan(&old_user).await;
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
