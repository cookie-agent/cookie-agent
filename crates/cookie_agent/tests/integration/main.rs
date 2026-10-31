//! The `cookie` crate's integration tests, one module per surface, in one
//! test binary: each links the whole daemon, so one binary saves a link of
//! it per module.

// One isolated home for the whole process, as every module's tests expect.
#[path = "../../../../test-support/config_harness.rs"]
mod config_harness;

mod daemon_restart;
mod local_daemon;
mod remote_runtime;
mod run_headless;
mod runtime_command;
mod startup_environment;
mod tool_reference;
