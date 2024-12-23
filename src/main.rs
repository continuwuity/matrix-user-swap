use std::{mem, process::ExitCode};

use clap::Parser;
use derive_more::Display;
use rand::{thread_rng, Rng};
use ruma::{
    api,
    client::{self, HttpClient},
    events::RoomAccountDataEventType,
    server_name, OwnedRoomAliasId, OwnedRoomId, RoomAliasId, RoomId,
};
use serde::Serialize;
use thiserror::Error;
use tracing as t;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{self, prelude::*};
use wee_woo::ErrorExt;

mod plan;
mod state;
mod utils;

use crate::{
    plan::{make_plan, FatalPlanError, Plan, PlanSettings},
    state::{ClientStateAccessor, ClientStateError},
    utils::RoomIdentity,
};

#[derive(Debug, Display, Copy, Clone, Serialize)]
pub(crate) enum UserKind {
    #[display("old")]
    Old,
    #[display("new")]
    New,
}

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

    /// Leave rooms that are fully migrated with the old user
    ///
    /// The recommended way to use this option is to run the tool without
    /// `--leave` first, confirm that the end state is what you expect for the
    /// new user, and then run it a second time with `--leave` if you're sure
    /// that everything is correct.
    #[clap(long)]
    leave: bool,

    /// Anonymize room IDs/aliases to make sharing example output easier
    ///
    /// This will _not_ anonymize server names and room IDs/aliases.
    #[clap(long)]
    anonymize: bool,
}

pub(crate) type Client = client::Client<client::http_client::Reqwest>;
pub(crate) type ReqwestError =
    <client::http_client::Reqwest as HttpClient>::Error;
pub(crate) type RumaError = client::Error<ReqwestError, api::client::Error>;

#[derive(Error, Debug)]
enum Error {
    #[error("failed to initialize logging")]
    InitLogging(#[from] InitLoggingError),

    #[error("failed to initialize matrix client for {_0} user")]
    InitClient(UserKind, #[source] RumaError),

    #[error("failed to initialize state accessor for {_0} user")]
    InitClientState(UserKind, #[source] ClientStateError),

    #[error("failed to compute migration plan")]
    MakePlan(#[from] FatalPlanError<ClientStateAccessor>),
}

#[derive(Error, Debug)]
enum InitLoggingError {
    #[error(
        "failed to parse filter from MATRIX_USER_SWAP_LOG environment variable"
    )]
    ParseEnvFilter(#[from] tracing_subscriber::filter::FromEnvError),
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

fn anonymize_room_id(room_id: &RoomId) -> OwnedRoomId {
    let server_name = room_id
        .server_name()
        .unwrap_or(server_name!("invalid-server-name.com"));
    RoomId::new(server_name)
}

fn anonymize_room_alias(room_alias: &RoomAliasId) -> OwnedRoomAliasId {
    let alias = format!(
        "#alias-{}:{}",
        thread_rng().gen_range(0u32..1024),
        room_alias.server_name()
    );
    RoomAliasId::parse(&alias)
        .expect("just constructed alias id should be valid")
}

fn anonymize_plan(plan: &mut Plan<ClientStateAccessor>) {
    // This dance is needed because RoomPlan doesn't impl Clone
    let rooms = mem::take(&mut plan.rooms);
    plan.rooms = rooms
        .into_iter()
        .map(|(room_id, mut room)| {
            room.alias = room.alias.as_deref().map(anonymize_room_alias);
            (anonymize_room_id(&room_id), room)
        })
        .collect();
}

fn print_column(label: &str, enabled: bool) {
    if enabled {
        print!("  {label}");
    } else {
        for _ in 0..label.len() + 2 {
            print!(" ");
        }
    }
}

fn print_plan(plan: &Plan<ClientStateAccessor>) {
    println!("Attempting to migrate the following rooms:\n");

    let max_room_len = 48;

    for (room_id, room) in &plan.rooms {
        let name = room
            .alias
            .as_ref()
            .map(|alias| alias.as_str())
            .unwrap_or(room_id.as_str());
        if name.len() <= max_room_len {
            print!("{name:<max_room_len$}");
        } else {
            print!("{name}\n{:<max_room_len$}", "");
        }

        print_column("errors", !room.errors.is_empty());
        print_column("invite", room.invite);
        print_column("join", room.join);
        print_column("leave", room.leave);
        print_column("power_level", room.power_level.is_some());

        let mut tags = false;
        for kind in room.account_data.keys() {
            // When we add support for planning a new type, we need to add it
            // here.
            match kind {
                RoomAccountDataEventType::Tag => tags = true,
                _ => t::error!(
                    "unrecognized room account data event type {kind}"
                ),
            }
        }
        print_column("tags", tags);
        println!();
    }

    println!();
    println!("Key:");
    println!(
        "  errors: one or more errors were encountered, which may prevent \
         completely migrating this room"
    );
    println!("  invite: the old user will invite the new user to this room");
    println!("  join: the new user will join this room");
    println!("  leave: the old user will leave this room");
    println!(
        "  power_level: the old user will promote the new user to their \
         current power level"
    );
    println!(
        "  tags: room tags (from the m.room.tags account data event) will be \
         copied from the old user to the new user"
    );

    if !plan.global_account_data.is_empty() {
        println!();
        println!("Migrating the following global account data events:");

        for kind in plan.global_account_data.keys() {
            println!("  - {kind}");
        }
    }

    let any_errors = !plan.errors.is_empty()
        || plan.rooms.values().any(|room| !room.errors.is_empty());
    if any_errors {
        println!();
        println!("Encountered the following errors:");

        for error in &plan.errors {
            println!("\n  {}", error.display_with_sources("\n    "));
        }

        for (room_id, room) in &plan.rooms {
            if room.errors.is_empty() {
                continue;
            }

            let identity = RoomIdentity {
                id: room_id.clone(),
                alias: room.alias.clone(),
            };
            println!("\n  In room {identity}:");

            for error in &room.errors {
                println!("    {}", error.display_with_sources("\n      "));
            }
        }
    }

    println!();
}

fn print_warnings() {
    println!(
        "Warning: the tool will only attempt to migrate the following account \
         data event types. It is possible that there are other account data \
         events that should be migrated, but the tool doesn't have support \
         for. There is no efficient way to determine a full list of account \
         data events for a user, so there will be no warnings for \
         unrecognized types:"
    );
    println!(" - m.direct (global)");
    println!(" - m.ignored_user_list (global)");
    println!(" - m.tag (per-room)");
    println!();
    println!(
        "Warning: the tool will warn about rooms where the current history \
         visibility setting may result in lost history when the old user \
         leaves the room. There is no efficient way to determine whether a \
         previous history visibility setting may result in lost history, so \
         this check may have false negatives."
    );
    println!();
    println!(
        "Warning: the tool does not currently attempt to migrate per-room \
         avatars or displaynames."
    );
}

async fn try_main() -> Result<(), Error> {
    init_logging()?;
    let cli = Cli::parse();

    let settings = PlanSettings {
        leave: cli.leave,
    };

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
        .http_client(http_client.clone())
        .await
        .map_err(|e| Error::InitClient(UserKind::New, e))?;

    let old = ClientStateAccessor::new(old_client)
        .await
        .map_err(|e| Error::InitClientState(UserKind::Old, e))?;
    let new = ClientStateAccessor::new(new_client)
        .await
        .map_err(|e| Error::InitClientState(UserKind::New, e))?;

    let mut plan = make_plan(settings, &old, &new).await?;

    if cli.anonymize {
        anonymize_plan(&mut plan);
    }

    print_plan(&plan);
    print_warnings();

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
