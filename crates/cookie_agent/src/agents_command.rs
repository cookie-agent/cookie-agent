//! `cookie agents`: list a daemon's agents with where each comes from,
//! refresh its official agents, and check agent documents before publishing
//! them (`docs/site/specs/machines-on-demand.md` section 2.1).

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Subcommand;
use cookie_agent_protocol::Client;
use sha2::{Digest as _, Sha256};

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum AgentsCommand {
    /// List the daemon's agents with their mode and where each comes from:
    /// `user` for a document in `agents/`, `official <version>` for one the
    /// daemon keeps, and the default agent marked.
    List,
    /// Refresh the official agents now.
    Update,
    /// Check the agent documents of a directory: each `<id>.md` loads with
    /// this build's strict parser, on its own as an official agent; an
    /// `index.json` beside them must match their files; a `compaction.md`
    /// keeps the body this build falls back to. Needs no daemon.
    Check {
        /// The agents directory.
        dir: PathBuf,
    },
}

/// `cookie agents list` and `cookie agents update` against a daemon.
pub(crate) async fn run_agents(
    url: &str,
    token: &str,
    command: AgentsCommand,
) -> anyhow::Result<()> {
    let client = Client::connect_websocket_with_token(url, token).await?;
    client
        .handshake_as(crate::client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    for line in agents_with(&client, command).await? {
        println!("{line}");
    }
    Ok(())
}

/// What `cookie agents list` or `update` prints, over `client`, which has
/// made its handshake.
pub(crate) async fn agents_with(
    client: &Client,
    command: AgentsCommand,
) -> anyhow::Result<Vec<String>> {
    let mut lines = Vec::new();
    match command {
        AgentsCommand::List => {
            let snapshot = client
                .runtime_snapshot()
                .await
                .context("runtime.snapshot.get failed")?
                .snapshot;
            let default =
                cookie_agent_client_state::default_agent_id(snapshot.default_agent.as_ref());
            if snapshot.agents.is_empty() {
                lines.push("No agents.".into());
            }
            for agent in &snapshot.agents {
                let origin = match (agent.official, &agent.official_version) {
                    (true, Some(version)) => format!("official {version}"),
                    (true, None) => "official".to_owned(),
                    (false, _) if agent.runtime.is_some() || !agent.runtimes.is_empty() => {
                        "acp".to_owned()
                    }
                    (false, _) => "user".to_owned(),
                };
                let name = match &agent.preset {
                    Some(preset) => format!("{preset}/{}", agent.id),
                    None => agent.id.to_string(),
                };
                let mode = serde_json::to_value(agent.mode)
                    .ok()
                    .and_then(|mode| mode.as_str().map(str::to_owned))
                    .unwrap_or_default();
                let marks = [
                    (agent.preset.is_none() && agent.id == default).then_some("default"),
                    (!agent.enabled).then_some("disabled"),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
                let marks = if marks.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", marks.join(", "))
                };
                lines.push(format!("{name}\t{mode}\t{origin}{marks}"));
            }
        }
        AgentsCommand::Update => {
            let result = client
                .update_agents()
                .await
                .context("agents.update failed")?;
            if !result.enabled {
                lines.push("Official agents are off ([agents] official = false).".into());
                return Ok(lines);
            }
            lines.extend(result.updated.iter().map(|id| format!("Updated {id}")));
            lines.extend(result.removed.iter().map(|id| format!("Removed {id}")));
            for agent in &result.agents {
                lines.push(format!("{}\tofficial {}", agent.id, agent.version));
            }
            if let Some(error) = result.error {
                anyhow::bail!("{}\n{error}", lines.join("\n"));
            }
            if result.updated.is_empty() && result.removed.is_empty() {
                lines.push("The official agents are up to date.".into());
            }
        }
        AgentsCommand::Check { .. } => unreachable!("checked without a daemon"),
    }
    Ok(lines)
}

/// `cookie agents check`: prints each problem and fails when there is one.
pub(crate) fn run_check(dir: &Path) -> anyhow::Result<()> {
    let problems = check(dir)?;
    for problem in &problems {
        println!("{problem}");
    }
    if problems.is_empty() {
        println!("The agents of {} check out.", dir.display());
        Ok(())
    } else {
        anyhow::bail!("{} problem(s) in {}", problems.len(), dir.display())
    }
}

/// The problems of the agent documents in `dir`, each naming its file.
pub(crate) fn check(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut documents = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "md")
                && path
                    .file_name()
                    .is_some_and(|name| !name.to_string_lossy().eq_ignore_ascii_case("README.md"))
        })
        .collect::<Vec<_>>();
    documents.sort();
    if documents.is_empty() {
        anyhow::bail!("{} holds no agent document", dir.display());
    }
    let mut problems = Vec::new();
    let mut ids = Vec::new();
    for path in &documents {
        let id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let id = match cookie_agent_protocol::AgentId::new(&id) {
            Ok(id) => id,
            Err(error) => {
                problems.push(format!("{}: not an agent ID: {error}", path.display()));
                continue;
            }
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                problems.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        let mut tree = cookie_agent_config::ConfigTree::default();
        tree.put_official_agent(id.clone(), bytes);
        match cookie_agent_config::load_from_tree(&tree) {
            Err(error) => problems.push(format!("{}: {error}", path.display())),
            // An official agent carries its whole prompt: the loader leaves
            // out one that uses `{{default_prompt}}`, and says so.
            Ok(loaded) => problems.extend(
                loaded
                    .official_agent_diagnostics
                    .iter()
                    .map(|diagnostic| format!("{}: {diagnostic}", path.display())),
            ),
        }
        ids.push(id);
    }
    let index = dir.join("index.json");
    if index.exists() {
        problems.extend(index_problems(dir, &index, &ids));
    }
    Ok(problems)
}

/// What is wrong with `index.json` against the documents `ids` of `dir`.
fn index_problems(dir: &Path, index: &Path, ids: &[cookie_agent_protocol::AgentId]) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Index {
        version: u32,
        agents: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        id: String,
        version: String,
        requires_cookie: String,
        sha256: String,
    }
    let shown = index.display();
    let parsed = std::fs::read(index)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            serde_json::from_slice::<Index>(&bytes).map_err(|error| error.to_string())
        });
    let index = match parsed {
        Ok(index) => index,
        Err(error) => return vec![format!("{shown}: {error}")],
    };
    let mut problems = Vec::new();
    if index.version != 1 {
        problems.push(format!("{shown}: version must be 1"));
    }
    for entry in &index.agents {
        if entry.version.is_empty() {
            problems.push(format!("{shown}: `{}` has no version", entry.id));
        }
        if cookie_agent_engine::version_satisfies(&entry.requires_cookie, "0.0.0").is_none() {
            problems.push(format!(
                "{shown}: `{}`: `{}` is not a version range",
                entry.id, entry.requires_cookie
            ));
        }
        match std::fs::read(dir.join(format!("{}.md", entry.id))) {
            Ok(bytes) => {
                let actual = format!("{:x}", Sha256::digest(&bytes));
                if !actual.eq_ignore_ascii_case(&entry.sha256) {
                    problems.push(format!(
                        "{shown}: `{}` lists SHA-256 {} but its file has {actual}",
                        entry.id, entry.sha256
                    ));
                }
            }
            Err(_) => problems.push(format!("{shown}: `{}` has no {}.md", entry.id, entry.id)),
        }
    }
    for id in ids {
        if !index.agents.iter().any(|entry| entry.id == id.as_str()) {
            problems.push(format!("{shown}: `{id}.md` is not listed"));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_refuses_an_official_agent_that_uses_default_prompt() {
        let directory = tempfile::tempdir().expect("tempdir");
        let write = |name: &str, body: &str| {
            std::fs::write(
                directory.path().join(name),
                format!(
                    "---\ndescription: Checkpoints\nmode: internal\nmodels: [\"${{parent_model}}\"]\n---\n{body}\n"
                ),
            )
            .expect("document");
        };
        write("compaction.md", "{{default_prompt}}");
        let problems = super::check(directory.path()).expect("checked");
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("never uses `{{default_prompt}}`"),
            "{problems:?}"
        );
        write(
            "compaction.md",
            "Summarize the conversation. Focus: {{focus}}",
        );
        assert_eq!(
            super::check(directory.path()).expect("checked"),
            Vec::<String>::new()
        );
    }
}
