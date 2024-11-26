use std::{
    collections::{HashMap, HashSet},
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

    #[error("failed to get alias for room {_0}")]
    GetRoomAlias(OwnedRoomId, #[source] GetRoomAliasError),
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

struct Room {
    id: OwnedRoomId,
    alias: Option<OwnedRoomAliasId>,
}

struct State {
    joined_rooms: Vec<Room>,
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
    let room_ids = response.joined_rooms;

    t::info!("fetching room aliases");
    let mut aliases = HashMap::new();
    for room_id in &room_ids {
        let alias = get_room_alias(client, room_id)
            .await
            .map_err(|e| GetStateError::GetRoomAlias(room_id.to_owned(), e))?;
        if let Some(alias) = alias {
            aliases.insert(room_id, alias);
        }
    }

    let joined_rooms = room_ids
        .iter()
        .map(|room_id| Room {
            id: room_id.to_owned(),
            alias: aliases.get(room_id).cloned(),
        })
        .collect();
    Ok(State {
        joined_rooms,
    })
}

fn diff_state(old: &State, new: &State) {
    let new_rooms =
        new.joined_rooms.iter().map(|room| &room.id).collect::<HashSet<_>>();

    let to_join =
        old.joined_rooms.iter().filter(|room| !new_rooms.contains(&room.id));

    println!("Rooms to join:");
    for room in to_join {
        print!("  - {}", room.id);
        if let Some(alias) = &room.alias {
            print!(" ({alias})");
        }
        println!();
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

    diff_state(&old_state, &new_state);

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
