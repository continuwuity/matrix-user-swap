use std::process::ExitCode;

use clap::Parser;
use derive_more::Display;
use ruma::{
    api,
    client::{self, HttpClient},
};
use serde::Serialize;
use thiserror::Error;
use tracing as t;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{self, prelude::*};
use wee_woo::ErrorExt;

mod plan;
mod state;

use crate::{
    plan::{make_plan, FatalPlanError},
    state::{ClientStateAccessor, ClientStateError},
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
        .http_client(http_client.clone())
        .await
        .map_err(|e| Error::InitClient(UserKind::New, e))?;

    let old = ClientStateAccessor::new(old_client)
        .await
        .map_err(|e| Error::InitClientState(UserKind::Old, e))?;
    let new = ClientStateAccessor::new(new_client)
        .await
        .map_err(|e| Error::InitClientState(UserKind::New, e))?;

    let (plan, errors) = make_plan(&old, &new).await?;

    for error in errors {
        t::error!("{}", error.display_with_sources("\n  Caused by: "));
    }

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
