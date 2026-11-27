//! Temporary runtimes' processes and hooks
//! (`docs/site/specs/temporary-runtimes.md` section 5).
//!
//! A temporary template's processes are recorded in
//! `runtime-processes.json` under the daemon's data root while they run, and
//! a per-session template's owners stay recorded until their root session is
//! deleted. A process's `stop_command` runs after it has exited or been
//! killed; a daemon that died with processes running runs theirs when it
//! next starts. `delete_command` runs once a recorded owner's root session
//! is deleted.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use cookie_agent_config::RuntimeTemplateConfig;
use cookie_agent_protocol::SessionId;
use serde::{Deserialize, Serialize};

use super::process::ProcessLifecycle;

/// The file the records live in, under the daemon's data root.
pub(crate) const RECORDS_FILE: &str = "runtime-processes.json";

/// How long a hook may run before it is killed.
const HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// The processes of temporary runtimes, as the daemon last recorded them.
pub(crate) struct ProcessRecords {
    path: PathBuf,
    records: Mutex<Vec<ProcessRecord>>,
}

#[derive(Default, Deserialize, Serialize)]
struct RecordsFile {
    #[serde(default)]
    processes: Vec<ProcessRecord>,
}

/// One process of a temporary runtime, or one owner of a per-session
/// runtime whose root session still exists.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ProcessRecord {
    runtime: String,
    /// The owning root session of a per-session runtime's process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<SessionId>,
    /// Whether the process may still run: set when it starts, cleared once
    /// its `stop_command` has run.
    live: bool,
}

impl ProcessRecords {
    /// The records under `data_root`; none when the file is missing or
    /// unreadable, which is reported.
    pub(crate) fn open(data_root: &Path) -> Self {
        let path = data_root.join(RECORDS_FILE);
        let records = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<RecordsFile>(&bytes)
                .map(|file| file.processes)
                .unwrap_or_else(|error| {
                    eprintln!(
                        "cookie-agent: {} is unreadable and starts empty: {error}",
                        path.display()
                    );
                    Vec::new()
                }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                eprintln!(
                    "cookie-agent: {} is unreadable and starts empty: {error}",
                    path.display()
                );
                Vec::new()
            }
        };
        Self {
            path,
            records: Mutex::new(records),
        }
    }

    fn change(&self, change: impl FnOnce(&mut Vec<ProcessRecord>)) {
        let mut records = lock(&self.records);
        let before = records.clone();
        change(&mut records);
        if *records == before {
            return;
        }
        let file = RecordsFile {
            processes: records.clone(),
        };
        drop(records);
        if let Err(error) = crate::native_compaction::write_atomic(&self.path, &file) {
            eprintln!("cookie-agent: {} not written: {error}", self.path.display());
        }
    }

    /// Records that `owner`'s process of `runtime` started.
    fn started(&self, runtime: &str, owner: Option<SessionId>) {
        self.change(|records| {
            match records
                .iter_mut()
                .find(|record| record.runtime == runtime && record.owner == owner)
            {
                Some(record) => record.live = true,
                None => records.push(ProcessRecord {
                    runtime: runtime.to_owned(),
                    owner,
                    live: true,
                }),
            }
        });
    }

    /// Records that `owner`'s process of `runtime` stopped: a per-session
    /// runtime's owner stays recorded for its `delete_command`.
    fn stopped(&self, runtime: &str, owner: Option<SessionId>) {
        self.change(|records| match owner {
            Some(_) => {
                for record in records
                    .iter_mut()
                    .filter(|record| record.runtime == runtime && record.owner == owner)
                {
                    record.live = false;
                }
            }
            None => records.retain(|record| record.runtime != runtime || record.owner.is_some()),
        });
    }

    /// Forgets `owner` of `runtime`, whose root session is gone.
    fn forget(&self, runtime: &str, owner: SessionId) {
        self.change(|records| {
            records.retain(|record| record.runtime != runtime || record.owner != Some(owner));
        });
    }

    /// Whether `owner` has used per-session runtime `runtime`.
    fn used(&self, runtime: &str, owner: SessionId) -> bool {
        lock(&self.records)
            .iter()
            .any(|record| record.runtime == runtime && record.owner == Some(owner))
    }

    /// The processes recorded as possibly still running.
    fn live(&self) -> Vec<(String, Option<SessionId>)> {
        lock(&self.records)
            .iter()
            .filter(|record| record.live)
            .map(|record| (record.runtime.clone(), record.owner))
            .collect()
    }

    /// Forgets every record of a runtime `config` no longer has.
    fn retain_configured(&self, configured: &BTreeMap<String, RuntimeTemplateConfig>) {
        self.change(|records| {
            records.retain(|record| configured.contains_key(&record.runtime));
        });
    }
}

/// One owner's process of a temporary template: records it and runs its
/// `stop_command`.
pub(crate) struct TemporaryProcess {
    runtime: String,
    owner: Option<SessionId>,
    config: RuntimeTemplateConfig,
    records: Arc<ProcessRecords>,
}

impl TemporaryProcess {
    pub(crate) fn new(
        runtime: &str,
        owner: Option<SessionId>,
        config: RuntimeTemplateConfig,
        records: Arc<ProcessRecords>,
    ) -> Self {
        Self {
            runtime: runtime.to_owned(),
            owner,
            config,
            records,
        }
    }
}

impl ProcessLifecycle for TemporaryProcess {
    fn started(&self) {
        self.records.started(&self.runtime, self.owner);
    }

    fn stopped(&self) -> futures_util::future::BoxFuture<'static, ()> {
        let runtime = self.runtime.clone();
        let owner = self.owner;
        let config = self.config.clone();
        let records = Arc::clone(&self.records);
        Box::pin(async move {
            let launch = config.launch(owner.map(|owner| owner.to_string()).as_deref());
            run_hook(&runtime, "stop_command", &launch.stop_command, &launch.env).await;
            records.stopped(&runtime, owner);
        })
    }
}

/// Runs `delete_command` of per-session template `runtime` for `owner`,
/// whose root session was deleted, when the owner used it, and forgets the
/// owner. The owner's process has stopped.
pub(crate) async fn run_delete(
    records: &ProcessRecords,
    runtime: &str,
    config: &RuntimeTemplateConfig,
    owner: SessionId,
) {
    if !records.used(runtime, owner) {
        return;
    }
    let launch = config.launch(Some(&owner.to_string()));
    run_hook(
        runtime,
        "delete_command",
        &launch.delete_command,
        &launch.env,
    )
    .await;
    records.forget(runtime, owner);
}

/// Runs `stop_command` of every process a daemon before this one left
/// recorded as running, in the background, and forgets the records of
/// runtimes `configured` no longer has.
pub(crate) fn recover(
    records: &Arc<ProcessRecords>,
    configured: &BTreeMap<String, RuntimeTemplateConfig>,
) {
    records.retain_configured(configured);
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    for (runtime, owner) in records.live() {
        let Some(config) = configured.get(&runtime).cloned() else {
            continue;
        };
        let records = Arc::clone(records);
        handle.spawn(async move {
            TemporaryProcess::new(&runtime, owner, config, records)
                .stopped()
                .await;
        });
    }
}

/// Runs `command` (`key` of runtime `runtime`'s template) on the daemon's
/// machine without a shell, with the user's shell environment and `env`,
/// for at most [`HOOK_TIMEOUT`]. A failure is reported, never returned.
async fn run_hook(runtime: &str, key: &str, command: &[String], env: &BTreeMap<String, String>) {
    let Some((program, arguments)) = command.split_first() else {
        return;
    };
    let mut process =
        tokio::process::Command::new(cookie_agent_protocol::paths::resolve_program(program, env));
    let environment = cookie_agent_runtime_api::shell_environment::environment().await;
    cookie_agent_runtime_api::shell_environment::apply(environment.as_deref(), &mut process);
    process
        .args(arguments)
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let outcome = match process.spawn() {
        Ok(child) => match tokio::time::timeout(HOOK_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) if output.status.success() => return,
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let tail = stderr.trim().lines().last().unwrap_or_default().to_owned();
                if tail.is_empty() {
                    format!("exited with {}", output.status)
                } else {
                    format!("exited with {}: {tail}", output.status)
                }
            }
            Ok(Err(error)) => error.to_string(),
            Err(_) => format!("did not finish within {} s", HOOK_TIMEOUT.as_secs()),
        },
        Err(error) => format!("failed to run `{program}`: {error}"),
    };
    eprintln!("cookie-agent: runtime `{runtime}`: `{key}` {outcome}");
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
