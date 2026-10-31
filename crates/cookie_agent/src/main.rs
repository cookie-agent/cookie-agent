use std::{env, time::Duration};

use clap::Parser;

/// The environment variable a daemon token may come from, which clap reads
/// into the `--token` arguments.
const DAEMON_TOKEN_ENV: &str = "COOKIE_DAEMON_TOKEN";

/// How this process was started, for `cookie daemon` to restart itself.
struct Launch {
    /// The command to start again; `None` when it could not be found, and
    /// the daemon then refuses `daemon.restart`.
    relaunch: Option<daemon_restart::Relaunch>,
    /// This process was started by a restart, so the daemon waits for its
    /// port to free.
    restarted: bool,
    /// The daemon stopped to restart: relaunch once the runtime is gone.
    relaunch_now: bool,
}

/// The most async runtime worker threads: the daemon waits on models,
/// tools, and clients far more than it computes, and every worker, with the
/// malloc arena it ends up with, costs memory on a many-core machine while
/// more of them only add scheduling overhead. `TOKIO_WORKER_THREADS`
/// overrides it.
const MAX_WORKER_THREADS: usize = 8;

/// The most glibc malloc arenas, where glibc would make up to eight per
/// core: each holds on to what its threads freed. `MALLOC_ARENA_MAX`
/// overrides it.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const MALLOC_ARENA_MAX: libc::c_int = 2;

/// Caps glibc's malloc arenas, unless `MALLOC_ARENA_MAX` already did. Runs
/// before any thread exists, which an arena limit needs to apply to all of
/// them.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn limit_malloc_arenas() {
    if env::var_os("MALLOC_ARENA_MAX").is_none() {
        // SAFETY: `mallopt` only sets an allocator parameter; no other
        // thread exists yet to allocate concurrently.
        unsafe { libc::mallopt(libc::M_ARENA_MAX, MALLOC_ARENA_MAX) };
    }
}

fn main() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    limit_malloc_arenas();
    let cli = cli::Cli::parse();
    // Before anything changes the environment: how to start this command
    // again.
    let mut launch = Launch {
        relaunch: daemon_restart::Relaunch::capture(),
        restarted: env::var_os(daemon_restart::RESTARTED_ENV).is_some(),
        relaunch_now: false,
    };
    // Likewise the path ACP agents' MCP bridges start from, before an
    // update can replace the binary under it.
    cookie_agent_coding_runtime::server::pin_executable();
    #[cfg(windows)]
    cli::update::remove_replaced_binaries();
    // SAFETY: as for the token below, no other thread exists yet.
    unsafe { env::remove_var(daemon_restart::RESTARTED_ENV) };
    // Clap has read the token into `cli`. Drop the variable so no process
    // cookie starts (the `bash` tool, MCP servers, plugins, runtime
    // templates, editors) inherits the daemon's credential.
    //
    // SAFETY: `remove_var` races only with other threads using the
    // environment. This runs on the main thread before the Tokio runtime or
    // anything else that starts a thread exists; argument parsing starts
    // none.
    unsafe { env::remove_var(DAEMON_TOKEN_ENV) };
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if env::var_os("TOKIO_WORKER_THREADS").is_none() {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        builder.worker_threads(cores.min(MAX_WORKER_THREADS));
    }
    let runtime = match builder.enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cookie: cannot start the async runtime: {error}");
            std::process::exit(1);
        }
    };
    let error_details = cli.error_details;
    let result = runtime.block_on(cli::dispatch(cli, &mut launch));
    if result.is_ok()
        && launch.relaunch_now
        && let Some(relaunch) = &launch.relaunch
    {
        // Every task and socket goes with the runtime before the new image
        // starts.
        runtime.shutdown_timeout(Duration::from_secs(5));
        let error = daemon_restart::relaunch(relaunch);
        eprintln!(
            "cookie: {}",
            cookie_agent_protocol::diagnostics::detail(&format!("{error:#}"))
        );
        std::process::exit(1);
    }
    if let Err(error) = result {
        eprintln!(
            "cookie: {}",
            cookie_agent_protocol::diagnostics::detail(&format!("{error:#}"))
        );
        // A daemon error shows its message and hint above; its technical
        // detail only on request.
        if let Some(detail) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<cookie_agent_protocol_transport::ClientError>())
            .and_then(cookie_agent_protocol_transport::ClientError::detail)
        {
            if error_details {
                eprintln!(
                    "details: {}",
                    cookie_agent_protocol::diagnostics::detail(&detail)
                );
            } else {
                eprintln!("(run again with --error-details for the technical detail)");
            }
        }
        std::process::exit(1);
    }
}

mod cli;
mod daemon_restart;
mod local_daemon;

#[cfg(test)]
mod cli_reference;

#[cfg(test)]
#[path = "../../../test-support/config_harness.rs"]
mod config_harness;
