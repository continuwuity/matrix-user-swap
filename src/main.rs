use std::process::ExitCode;

use clap::Parser;
use derive_more::Display;
use ruma::client::{self};
use thiserror::Error;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{self, prelude::*};
use wee_woo::ErrorExt;

mod plan;
mod state;

use crate::{
    plan::{make_plan, MakePlanError},
    state::{InitStateAccessorError, StateAccessor},
};

#[derive(Debug, Display, Copy, Clone)]
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
}

#[derive(Error, Debug)]
enum Error {
    #[error("failed to initialize logging")]
    InitLogging(#[from] InitLoggingError),

    #[error("failed to initialize {_0} user")]
    InitUser(UserKind, #[source] InitStateAccessorError),

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

async fn try_main() -> Result<(), Error> {
    init_logging()?;
    let cli = Cli::parse();

    let http_client = client::http_client::Reqwest::new();

    let old_state = StateAccessor::new(
        cli.old_hs_url,
        cli.old_access_token,
        http_client.clone(),
    )
    .await
    .map_err(|e| Error::InitUser(UserKind::Old, e))?;
    let new_state =
        StateAccessor::new(cli.new_hs_url, cli.new_access_token, http_client)
            .await
            .map_err(|e| Error::InitUser(UserKind::New, e))?;

    let plan = make_plan(&old_state, &new_state).await?;
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
