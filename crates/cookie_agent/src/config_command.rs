//! `cookie config`: read and change a daemon's configuration (`config.toml`
//! and the agent documents) with `config.get`, `config.apply`, and
//! `config.reload`.

use anyhow::Context as _;
use clap::Subcommand;
use cookie_agent_protocol::{
    Client, ClientError, ConfigApplyErrorCode, ConfigApplyParams, ConfigChange, ConfigGetResult,
    ConfigStatus,
};

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Edit a configuration document in `$VISUAL` or `$EDITOR`:
    /// `config.toml` (the default), `agents/<id>.md`, or
    /// `agents/<preset>/<id>.md`. A path that does not exist yet is created.
    Edit { path: Option<String> },
    /// Set one key of `config.toml`, such as `loop_warning.enabled false`.
    /// The value is TOML; anything that does not parse as TOML is a string.
    Set { key: String, value: String },
    /// Remove one key of `config.toml`.
    Unset { key: String },
    /// Remove an agent document; a preset's last one removes the preset.
    Remove { path: String },
    /// Re-read the configuration files and apply them, after editing them by
    /// hand.
    Reload,
    /// Show whether the applied configuration matches the files, why the
    /// last reload failed, and what needs a restart.
    Status,
}

/// What `cookie config` needs from the terminal.
pub(crate) trait ConfigIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()>;
    /// Lets the user edit `text`, the document `path`; the edited text.
    fn edit(&mut self, path: &str, text: &str) -> anyhow::Result<String>;
    fn confirm(&mut self, prompt: &str) -> anyhow::Result<bool>;
}

/// The terminal and the editor of [`cookie_agent_protocol::editor::editor`].
pub(crate) struct StdioConfigIo;

impl ConfigIo for StdioConfigIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        println!("{line}");
        Ok(())
    }

    fn edit(&mut self, path: &str, text: &str) -> anyhow::Result<String> {
        let suffix = if path.ends_with(".md") {
            ".md"
        } else {
            ".toml"
        };
        edit_in_editor("cookie-config-", suffix, text)
    }

    fn confirm(&mut self, prompt: &str) -> anyhow::Result<bool> {
        use std::io::Write as _;
        print!("{prompt} ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        Ok(!matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "n" | "no"
        ))
    }
}

/// Lets the user edit `text` in [`cookie_agent_protocol::editor::editor`], in
/// a temporary file named `<prefix>…<suffix>`; the edited text.
pub(crate) fn edit_in_editor(prefix: &str, suffix: &str, text: &str) -> anyhow::Result<String> {
    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(suffix)
        .tempfile()
        .context("create a file to edit")?;
    std::io::Write::write_all(&mut file, text.as_bytes()).context("write the file to edit")?;
    let editor = cookie_agent_protocol::editor::editor();
    let status = cookie_agent_protocol::editor::editor_command(&editor, file.path())
        .status()
        .with_context(|| format!("run the editor `{editor}`"))?;
    anyhow::ensure!(
        status.success(),
        "the editor `{editor}` exited with {status}"
    );
    std::fs::read_to_string(file.path()).context("read the edited file")
}

pub(crate) async fn run_config(
    url: &str,
    token: &str,
    command: ConfigCommand,
) -> anyhow::Result<()> {
    if matches!(command, ConfigCommand::Edit { .. }) {
        use std::io::IsTerminal as _;
        anyhow::ensure!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "cookie config edit requires an interactive terminal"
        );
    }
    let client = Client::connect_websocket_with_token(url, token).await?;
    client
        .handshake_as(crate::client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    run_config_with(&client, command, &mut StdioConfigIo).await
}

pub(crate) async fn run_config_with<I: ConfigIo>(
    client: &Client,
    command: ConfigCommand,
    io: &mut I,
) -> anyhow::Result<()> {
    match command {
        ConfigCommand::Edit { path } => {
            edit(client, path.as_deref().unwrap_or("config.toml"), io).await
        }
        ConfigCommand::Set { key, value } => {
            let value = toml_value(&value)?;
            set(client, &key, value, io).await
        }
        ConfigCommand::Unset { key } => set(client, &key, serde_json::Value::Null, io).await,
        ConfigCommand::Remove { path } => {
            let config = get(client).await?;
            let document = config
                .documents
                .iter()
                .find(|document| document.path == path)
                .with_context(|| format!("`{path}` is not a configuration document"))?;
            let change = ConfigChange::Delete {
                path: path.clone(),
                expected_fingerprint: document.fingerprint.clone(),
            };
            let status = apply(client, config.revision, vec![change]).await?;
            io.write_line(&format!("Removed {path}."))?;
            write_status(io, &status, None)
        }
        ConfigCommand::Reload => {
            let status = client
                .reload_config()
                .await
                .context("config.reload failed")?
                .status;
            if status.error.is_none() {
                io.write_line("Reloaded the configuration.")?;
            }
            write_status(io, &status, None)
        }
        ConfigCommand::Status => {
            let config = get(client).await?;
            write_status(io, &config.status, Some(&config))
        }
    }
}

async fn get(client: &Client) -> anyhow::Result<ConfigGetResult> {
    client.get_config().await.context("config.get failed")
}

async fn apply(
    client: &Client,
    expected_revision: cookie_agent_protocol::ConfigRevision,
    changes: Vec<ConfigChange>,
) -> Result<ConfigStatus, ApplyFailure> {
    client
        .apply_config(ConfigApplyParams {
            expected_revision,
            changes,
        })
        .await
        .map(|result| result.status)
        .map_err(ApplyFailure::from)
}

/// A refused `config.apply` (`config_apply_refused`): why, and the daemon's
/// detail, which says what to fix.
#[derive(Debug)]
struct Refusal {
    code: ConfigApplyErrorCode,
    message: String,
}

/// A refused `config.apply`, or another failure.
#[derive(Debug)]
enum ApplyFailure {
    Refused(Refusal),
    Other(ClientError),
}

impl std::fmt::Display for ApplyFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(error) => formatter.write_str(&error.message),
            Self::Other(error) => write!(formatter, "config.apply failed: {error}"),
        }
    }
}

impl std::error::Error for ApplyFailure {}

impl From<ClientError> for ApplyFailure {
    fn from(error: ClientError) -> Self {
        if !error.is(&cookie_agent_protocol::errors::CONFIG_APPLY_REFUSED) {
            return Self::Other(error);
        }
        let data = error.error_data().unwrap_or_default();
        Self::Refused(Refusal {
            code: data
                .fields
                .get("reason")
                .and_then(|reason| serde_json::from_value(reason.clone()).ok())
                .unwrap_or(ConfigApplyErrorCode::InvalidChange),
            message: data.detail.unwrap_or_else(|| error.to_string()),
        })
    }
}

const NEW_AGENT: &str = "---\ndescription: \nmode: subagent\nenabled: true\nmodels: [{ model: \"\" }]\npermissions: {}\n---\n";

async fn edit<I: ConfigIo>(client: &Client, path: &str, io: &mut I) -> anyhow::Result<()> {
    let config = get(client).await?;
    let document = config
        .documents
        .iter()
        .find(|document| document.path == path);
    let original = match document {
        Some(document) => document.text.clone(),
        None if path.ends_with(".md") => NEW_AGENT.to_owned(),
        None => String::new(),
    };
    let expected_fingerprint = document.map(|document| document.fingerprint.clone());
    let mut text = original.clone();
    loop {
        text = io.edit(path, &text)?;
        if text == original && document.is_some() {
            io.write_line("No changes.")?;
            return Ok(());
        }
        let change = ConfigChange::Put {
            path: path.to_owned(),
            text: text.clone(),
            expected_fingerprint: expected_fingerprint.clone(),
        };
        match apply(client, config.revision.clone(), vec![change]).await {
            Ok(status) => {
                io.write_line(&format!("Applied {path}."))?;
                return write_status(io, &status, None);
            }
            Err(ApplyFailure::Refused(error))
                if matches!(
                    error.code,
                    ConfigApplyErrorCode::InvalidConfiguration
                        | ConfigApplyErrorCode::InvalidChange
                ) =>
            {
                io.write_line(&format!("Not applied: {}", error.message))?;
                if !io.confirm("Edit again? [Y/n]")? {
                    anyhow::bail!("{path} was not changed");
                }
            }
            Err(ApplyFailure::Refused(error)) => {
                anyhow::bail!("{path} was not changed: {}; edit it again", error.message)
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Sets `key` (dotted, with TOML quoting) to `value`; a `set` does not
/// depend on the rest of the file, so a concurrent change of the revision is
/// retried once.
async fn set<I: ConfigIo>(
    client: &Client,
    key: &str,
    value: serde_json::Value,
    io: &mut I,
) -> anyhow::Result<()> {
    let names = toml_edit::Key::parse(key)
        .with_context(|| format!("`{key}` is not a TOML key"))?
        .iter()
        .map(|name| name.get().to_owned())
        .collect::<Vec<_>>();
    let mut retried = false;
    loop {
        let revision = get(client).await?.revision;
        let change = ConfigChange::Set {
            key: names.clone(),
            value: value.clone(),
        };
        match apply(client, revision, vec![change]).await {
            Ok(status) => {
                io.write_line(&if value.is_null() {
                    format!("Removed {key}.")
                } else {
                    format!("Set {key}.")
                })?;
                return write_status(io, &status, None);
            }
            Err(ApplyFailure::Refused(error))
                if error.code == ConfigApplyErrorCode::Conflict && !retried =>
            {
                retried = true;
            }
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!("{key} was not changed")));
            }
        }
    }
}

/// A TOML value as JSON; text that is not TOML is a string.
fn toml_value(text: &str) -> anyhow::Result<serde_json::Value> {
    let value = format!("value = {text}")
        .parse::<toml::Table>()
        .ok()
        .and_then(|mut table| table.remove("value"))
        .unwrap_or_else(|| toml::Value::String(text.to_owned()));
    serde_json::to_value(value).context("convert the value")
}

fn write_status<I: ConfigIo>(
    io: &mut I,
    status: &ConfigStatus,
    on_disk: Option<&ConfigGetResult>,
) -> anyhow::Result<()> {
    if let Some(config) = on_disk {
        io.write_line(&format!("Applied revision: {}", status.applied_revision))?;
        if config.revision != status.applied_revision {
            io.write_line(
                "The files differ from the applied configuration; run `cookie config reload`.",
            )?;
        }
    }
    if let Some(error) = &status.error {
        io.write_line(&format!("The last reload failed: {error}"))?;
    }
    if !status.restart_required.is_empty() {
        let parts = status
            .restart_required
            .iter()
            .map(|part| match part {
                cookie_agent_protocol::ConfigRestartPart::Server => "server",
                cookie_agent_protocol::ConfigRestartPart::Plugins => "plugins",
                cookie_agent_protocol::ConfigRestartPart::Runtimes => "runtimes",
            })
            .collect::<Vec<_>>()
            .join(", ");
        io.write_line(&format!("Restart the daemon to apply: {parts}."))?;
    }
    Ok(())
}
