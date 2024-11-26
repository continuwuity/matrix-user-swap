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
    events::StateEventType,
    OwnedRoomAliasId, OwnedRoomId, RoomId,
};
use serde::Deserialize;
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

#[derive(Debug, Display)]
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

    #[error("failed to initialize matrix client for {_0} user")]
    InitClient(UserKind, #[source] RumaError),

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
enum GetStateError {
    #[error("failed to get joined room list")]
    GetJoinedRooms(#[source] RumaError),
}

#[derive(Error, Debug)]
enum GetRoomAliasError {
    #[error("failed to get m.room.canonical_alias event from server")]
    Request(#[source] RumaError),

    #[error("m.room.canonical_alias event did not match expected schema")]
    Deserialize(#[source] serde_json::Error),
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

async fn get_room_alias(
    client: &Client,
    room_id: &RoomId,
) -> Result<Option<OwnedRoomAliasId>, GetRoomAliasError> {
    use GetRoomAliasError as Error;

    let request =
        api::client::state::get_state_events_for_key::v3::Request::new(
            room_id.to_owned(),
            StateEventType::RoomCanonicalAlias,
            "".to_owned(),
        );
    let response = client.send_request(request).await;

    let response = match response {
        Ok(response) => response,
        // Spec says that "The room has no state with the given type or key."
        // is 404, but does not specify a errcode, so this is the best we can
        // do.
        Err(e) if e.error_kind() == Some(&ErrorKind::NotFound) => {
            return Ok(None)
        }
        Err(client::Error::FromHttpResponse(
            FromHttpResponseError::Server(e),
        )) if e.status_code.as_u16() == 404 => return Ok(None),
        Err(e) => return Err(Error::Request(e)),
    };

    #[derive(Deserialize)]
    struct ExtractAlias {
        alias: Option<OwnedRoomAliasId>,
    }
    match response.content.deserialize_as::<ExtractAlias>() {
        Ok(ExtractAlias {
            alias,
        }) => Ok(alias),
        Err(e) => Err(Error::Deserialize(e)),
    }
}

async fn get_state(
    kind: UserKind,
    client: &Client,
) -> Result<State, GetStateError> {
    use GetStateError as Error;

    t::info!("fetching state for {kind} user");

    t::info!("fetching list of joined rooms");
    let response = client
        .send_request(api::client::membership::joined_rooms::v3::Request::new())
        .await
        .map_err(Error::GetJoinedRooms)?;
    let joined_rooms = response.joined_rooms;

    Ok(State {
        joined_rooms,
    })
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
    async fn to_plan(&self, client: &Client) -> Plan {
        t::info!("fetching room aliases");

        let mut rooms = BTreeMap::new();
        for room_id in &self.join_rooms {
            let alias = match get_room_alias(client, room_id).await {
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

    let old_client = client::Client::builder()
        .access_token(Some(cli.old_access_token))
        .homeserver_url(cli.old_hs_url)
        .http_client(http_client.clone())
        .await
        .map_err(|e| Error::InitClient(UserKind::Old, e))?;
    let new_client = client::Client::builder()
        .access_token(Some(cli.new_access_token))
        .homeserver_url(cli.new_hs_url)
        .http_client(http_client)
        .await
        .map_err(|e| Error::InitClient(UserKind::New, e))?;

    let old_state = get_state(UserKind::Old, &old_client)
        .await
        .map_err(|e| Error::GetState(UserKind::Old, e))?;
    let new_state = get_state(UserKind::New, &new_client)
        .await
        .map_err(|e| Error::GetState(UserKind::New, e))?;

    let diff = old_state.diff_from(&new_state);
    let plan = diff.to_plan(&old_client).await;
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
