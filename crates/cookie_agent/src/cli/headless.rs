//! `cookie run`: one prompt without the interactive TUI, against a daemon
//! composed in this process (`cookie_agent::run`).

use std::env;

use anyhow::Context as _;
use cookie_agent::run;
use cookie_agent_models::{catalog::CatalogManager, provider_store::ProviderStore};

use super::{
    compose::{compose_with, run_catalog_transport},
    data_dir,
};

/// Runs `args` and exits with its exit code; a failure to set up exits
/// with [`run::EXIT_ENVIRONMENT`].
pub(super) async fn run_headless(args: run::RunArgs) -> anyhow::Result<()> {
    let workspace = match env::current_dir().context("determine current run workspace") {
        Ok(path) => path,
        Err(error) => {
            eprintln!(
                "cookie run: {}",
                cookie_agent_protocol::diagnostics::detail(&format!("{error:#}"))
            );
            std::process::exit(run::EXIT_ENVIRONMENT);
        }
    };
    let data_dir = match args.data_dir.clone().map_or_else(data_dir, Ok) {
        Ok(path) => path,
        Err(error) => {
            eprintln!(
                "cookie run: {}",
                cookie_agent_protocol::diagnostics::detail(&format!("{error:#}"))
            );
            std::process::exit(run::EXIT_ENVIRONMENT);
        }
    };
    let mut runtime = match compose_with(
        &workspace,
        run_catalog_transport,
        CatalogManager::standard,
        ProviderStore::standard,
        || Ok(data_dir),
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!(
                "cookie run: {}",
                cookie_agent_protocol::diagnostics::detail(&format!("{error:#}"))
            );
            std::process::exit(run::EXIT_ENVIRONMENT);
        }
    };
    let exit_code = run::execute(&runtime.engine, args).await;
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}
