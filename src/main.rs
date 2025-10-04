use std::{borrow::Cow, mem, process::ExitCode};

use clap::{Args, Parser};
use derive_more::Display;
use dialoguer::Confirm;
use rand::{Rng, thread_rng};
use ruma::{
    OwnedRoomAliasId, OwnedRoomId, RoomAliasId, RoomId, api,
    client::{self, HttpClient},
    events::RoomAccountDataEventType,
    server_name,
};
use serde::Serialize;
use thiserror::Error;
use tracing as t;
use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::{self, prelude::*};
use wee_woo::ErrorExt;

mod execute;
mod plan;
mod rate_limit;
mod state;
mod utils;

use crate::{
    execute::{ExecuteError, execute_plan},
    plan::{FatalPlanError, Plan, PlanSettings, make_plan},
    rate_limit::RateLimitedClient,
    state::{ClientReadStateError, ClientStateAccessor},
    utils::RoomIdentity,
};

#[derive(Debug, Display, Copy, Clone, Serialize)]
pub(crate) enum UserKind {
    #[display("old")]
    Old,
    #[display("new")]
    New,
}

// <https://github.com/clap-rs/clap/issues/2621> would make this much nicer :(
#[derive(Args)]
#[group(required = true, multiple = false)]
struct OldAuth {
    #[clap(id = "old-access-token", long)]
    access_token: Option<String>,
    #[clap(id = "old-user", long)]
    user: Option<String>,
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct NewAuth {
    #[clap(id = "new-access-token", long)]
    access_token: Option<String>,
    #[clap(id = "new-user", long)]
    user: Option<String>,
}

// TODO: support server discovery from userid
#[derive(Parser)]
struct Cli {
    #[clap(flatten)]
    old_auth: OldAuth,
    #[clap(long, requires = "old-user")]
    old_password: Option<String>,
    #[clap(long)]
    old_hs_url: String,

    #[clap(flatten)]
    new_auth: NewAuth,
    #[clap(long, requires = "new-user")]
    new_password: Option<String>,
    #[clap(long)]
    new_hs_url: String,

    /// Compute a migration plan, but do not execute it.
    #[clap(short = 'd', long)]
    dry_run: bool,

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

    #[error("migration between different homeservers is currently unsupported")]
    DifferentServers,

    #[error("failed to initialize matrix client for {_0} user")]
    InitClient(UserKind, #[source] RumaError),

    #[error("failed to log in to {_0} user")]
    LogIn(UserKind, #[source] RumaError),

    // No extra details because we log logout failures with tracing.
    //
    // This is because a logout may fail in the middle of processing another
    // error, and we don't have a mechanism to collect both errors together to
    // be presented in `main`.
    //
    // TODO: switch to derail and not have this problem?
    #[error("failed to log out one or both users")]
    LogOut,

    #[error("failed to initialize state accessor for {_0} user")]
    InitClientState(UserKind, #[source] ClientReadStateError),

    #[error("failed to compute migration plan")]
    MakePlan(#[from] FatalPlanError<ClientStateAccessor>),

    #[error("failed to execute migration plan")]
    ExecutePlan(#[from] ExecuteError),
}

#[derive(Error, Debug)]
enum InitLoggingError {
    #[error(
        "failed to parse filter from MATRIX_USER_SWAP_LOG environment variable"
    )]
    ParseEnvFilter(#[from] tracing_subscriber::filter::FromEnvError),
}

fn init_logging() -> Result<(), InitLoggingError> {
    let indicatif_layer = IndicatifLayer::new();
    let env_filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive("matrix_user_swap=info".parse().unwrap())
        .with_env_var("MATRIX_USER_SWAP_LOG")
        .from_env()?;
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(indicatif_layer.get_stderr_writer());
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(indicatif_layer)
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

async fn try_log_out(
    kind: UserKind,
    client: &RateLimitedClient,
) -> Result<(), RumaError> {
    let request = api::client::session::logout::v3::Request::new();
    client.send_request(request).await.inspect_err(|error| {
        t::error!(?error, "Failed to log out session for {kind} user");
    })?;
    Ok(())
}

/// Along with the client, returns whether or not we created a new session so
/// that the caller can determine whether it needs to log out on cleanup.
async fn init_client(
    kind: UserKind,
    http_client: client::http_client::Reqwest,
    hs_url: String,
    access_token: Option<String>,
    user_id: Option<&str>,
    password: Option<&str>,
) -> Result<(ClientStateAccessor, bool), Error> {
    let new_session = access_token.is_none();

    let client = client::Client::builder()
        .access_token(access_token)
        .homeserver_url(hs_url)
        .http_client(http_client)
        .await
        .map_err(|e| Error::InitClient(kind, e))?;

    if new_session {
        let user_id =
            user_id.expect("clap should require exactly one of the auth args");
        let password = match password {
            Some(password) => Cow::Borrowed(password),
            // Unwrap because the only error case is IO on stdout/stdin
            None => Cow::Owned(
                dialoguer::Password::new()
                    .with_prompt(format!("Password for {user_id}"))
                    .interact()
                    .unwrap(),
            ),
        };

        client
            .log_in(user_id, &password, None, Some("matrix-user-swap"))
            .await
            .map_err(|e| Error::LogIn(kind, e))?;
    }

    let client = RateLimitedClient::new(client);
    match ClientStateAccessor::new(client.clone()).await {
        Ok(client_state) => Ok((client_state, new_session)),
        Err(error) => {
            if new_session {
                // Discarding error since it will be logged and we already have
                // a different error to return
                let _ = try_log_out(kind, &client).await;
            }
            Err(Error::InitClientState(kind, error))
        }
    }
}

async fn plan_and_execute(
    cli: Cli,
    old: &ClientStateAccessor,
    new: &ClientStateAccessor,
) -> Result<(), Error> {
    let settings = PlanSettings {
        leave: cli.leave,
    };
    let mut plan = make_plan(settings, old, new).await?;

    if cli.anonymize {
        anonymize_plan(&mut plan);
    }

    print_plan(&plan);
    print_warnings();

    if cli.dry_run {
        t::info!("Not executing plan, because --dry-run was specified");
        return Ok(());
    }

    println!();
    // Unwrap because the only error case is IO on stdout/stdin
    let confirm = Confirm::new()
        .with_prompt("Continue?")
        .wait_for_newline(true)
        .default(false)
        .interact()
        .unwrap();
    if !confirm {
        t::info!("Cancelled plan execution");
        return Ok(());
    }

    execute_plan(&plan, old, new).await?;

    Ok(())
}

async fn try_main() -> Result<(), Error> {
    init_logging()?;
    let cli = Cli::parse();

    if cli.old_hs_url != cli.new_hs_url {
        // TODO: figure out the wait-for-invite problem
        return Err(Error::DifferentServers);
    }

    let http_client = client::http_client::Reqwest::new();

    let (old, old_new_session) = init_client(
        UserKind::Old,
        http_client.clone(),
        cli.old_hs_url.clone(),
        cli.old_auth.access_token.clone(),
        cli.old_auth.user.as_deref(),
        cli.old_password.as_deref(),
    )
    .await?;
    let new_result = init_client(
        UserKind::New,
        http_client.clone(),
        cli.new_hs_url.clone(),
        cli.new_auth.access_token.clone(),
        cli.new_auth.user.as_deref(),
        cli.new_password.as_deref(),
    )
    .await;
    let (new, new_new_session) = match new_result {
        Ok(ok) => ok,
        Err(error) => {
            if old_new_session {
                let _ = try_log_out(UserKind::Old, old.inner()).await;
            }
            return Err(error);
        }
    };

    let result = plan_and_execute(cli, &old, &new).await;

    let mut log_out_error = false;
    if old_new_session {
        log_out_error |= try_log_out(UserKind::Old, old.inner()).await.is_err();
    }
    if new_new_session {
        log_out_error |= try_log_out(UserKind::New, new.inner()).await.is_err();
    }

    if log_out_error && result.is_ok() {
        // Log out errors will already have been logged, but we want to return
        // an error in order to set the right exit status
        Err(Error::LogOut)
    } else {
        result
    }
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
