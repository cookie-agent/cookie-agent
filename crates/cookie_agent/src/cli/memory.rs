//! `cookie memory`: list, show, edit, and delete a daemon's memory files
//! with the `memory.*` methods, so it works against a remote daemon
//! (`docs/site/specs/memory.md` section 10). The listing and `show` give
//! each memory's stamps and its size against its limit (section 16.9);
//! `edit` leaves out the stamp lines the daemon owns.

use anyhow::Context as _;
use clap::Subcommand;
use cookie_agent_client_state::memory::{MemoryStamps, OVER_LIMIT_NOTE, editor_text};
use cookie_agent_protocol::{MemoryInfo, MemoryLayer, MemoryListParams, MemoryListResult, errors};
use cookie_agent_protocol_transport::{Client, ClientError};
use jiff::Timestamp;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum MemoryCommand {
    /// List the memories, of every layer or of one: `user`, `agent`,
    /// `machine`, or `project`. Invalid files and entries that are not
    /// memories are listed with the reason.
    List {
        #[arg(value_parser = parse_layer)]
        layer: Option<MemoryLayer>,
    },
    /// Show a memory, such as `project://web/build`: when it was created,
    /// modified, and last read, its size against its limit, then its text.
    Show { url: String },
    /// Edit a memory in `$VISUAL` or `$EDITOR`, frontmatter included, such
    /// as `project://web/build`; a memory that does not exist yet is
    /// created.
    Edit { url: String },
    /// Delete a memory; asks first unless `--yes`.
    Rm {
        url: String,
        /// Delete without asking.
        #[arg(long, short)]
        yes: bool,
    },
}

/// The layer named `value`.
fn parse_layer(value: &str) -> Result<MemoryLayer, String> {
    MemoryLayer::ALL
        .into_iter()
        .find(|layer| layer.as_str() == value)
        .ok_or_else(|| format!("`{value}` is not a layer: use user, agent, machine, or project"))
}

/// What `cookie memory` needs from the terminal.
pub(crate) trait MemoryIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()>;
    /// Lets the user edit `text`, the memory `url`; the edited text.
    fn edit(&mut self, url: &str, text: &str) -> anyhow::Result<String>;
    /// Asks a yes-or-no question; `default` answers an empty reply.
    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool>;
}

/// The terminal and the editor of [`cookie_agent_protocol::editor::editor`].
pub(crate) struct StdioMemoryIo;

impl MemoryIo for StdioMemoryIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        println!("{line}");
        Ok(())
    }

    fn edit(&mut self, _url: &str, text: &str) -> anyhow::Result<String> {
        super::config::edit_in_editor("cookie-memory-", ".md", text)
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool> {
        use std::io::Write as _;
        print!("{prompt} ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        Ok(match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default,
        })
    }
}

pub(crate) async fn run_memory(
    url: &str,
    token: &str,
    command: MemoryCommand,
) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    match &command {
        MemoryCommand::Edit { .. } => anyhow::ensure!(
            interactive,
            "cookie memory edit requires an interactive terminal"
        ),
        MemoryCommand::Rm { yes: false, .. } => anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "cookie memory rm asks before deleting; pass --yes to delete without a terminal"
        ),
        _ => {}
    }
    let client = Client::connect_websocket_with_token(url, token).await?;
    client
        .handshake_as(super::client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    run_memory_with(&client, command, &mut StdioMemoryIo).await
}

/// The text a new memory starts from.
pub(crate) const NEW_MEMORY: &str = "---\ndescription: \nkeywords: []\n---\n";

pub(crate) async fn run_memory_with<I: MemoryIo>(
    client: &Client,
    command: MemoryCommand,
    io: &mut I,
) -> anyhow::Result<()> {
    match command {
        MemoryCommand::List { layer } => {
            let listed = client
                .list_memory(MemoryListParams {
                    layer,
                    ..MemoryListParams::default()
                })
                .await
                .context("memory.list failed")?;
            for line in memory_lines(&listed, Timestamp::now()) {
                io.write_line(&line)?;
            }
            Ok(())
        }
        MemoryCommand::Show { url } => {
            let read = client.read_memory(&url).await.map_err(daemon_error)?;
            // Only the listing knows when a model last read it.
            let last_read = client
                .list_memory(MemoryListParams::default())
                .await
                .ok()
                .and_then(|listed| {
                    listed
                        .memories
                        .into_iter()
                        .find(|memory| memory.url == read.url)
                })
                .and_then(|memory| memory.last_read);
            let stamps = MemoryStamps::of_read(&read, last_read);
            io.write_line(&read.url)?;
            if let Some(reason) = &read.invalid {
                io.write_line(&format!("invalid: {reason}"))?;
            }
            for line in stamps.detail_lines(Timestamp::now()) {
                io.write_line(&line)?;
            }
            if stamps.over_limit() {
                io.write_line(OVER_LIMIT_NOTE)?;
            }
            io.write_line(&format!("revision: {}", read.revision))?;
            io.write_line("")?;
            let text = editor_text(&read.text, client.daemon_version());
            io.write_line(text.strip_suffix('\n').unwrap_or(&text))
        }
        MemoryCommand::Edit { url } => edit(client, &url, io).await,
        MemoryCommand::Rm { url, yes } => {
            let read = client.read_memory(&url).await.map_err(daemon_error)?;
            if !yes && !io.confirm(&format!("Delete {}? [y/N]", read.url), false)? {
                io.write_line(&format!("{} was not deleted.", read.url))?;
                return Ok(());
            }
            client
                .delete_memory(&read.url, &read.revision)
                .await
                .map_err(daemon_error)?;
            io.write_line(&format!("Deleted {}.", read.url))
        }
    }
}

/// A daemon refusal the way a user reads it. A [`ClientError`] already
/// reads as its message followed by its hint, so nothing is added to it.
fn daemon_error(error: ClientError) -> anyhow::Error {
    anyhow::Error::new(error)
}

/// The message of a daemon error, without its code.
fn message_of(error: &ClientError) -> String {
    match error {
        ClientError::Rpc(rpc) => rpc.message.clone(),
        error => error.to_string(),
    }
}

/// Opens `url` in the editor and writes it back at the revision read; a new
/// memory starts from [`NEW_MEMORY`]. The editor leaves out the stamp lines
/// a daemon that owns them writes on every save. Invalid text can be edited
/// again; a memory that changed meanwhile can be reloaded and edited again.
async fn edit<I: MemoryIo>(client: &Client, url: &str, io: &mut I) -> anyhow::Result<()> {
    let version = client.daemon_version();
    let (mut original, mut revision) = match client.read_memory(url).await {
        Ok(read) => {
            if let Some(reason) = &read.invalid {
                io.write_line(&format!("{} is not a valid memory: {reason}", read.url))?;
            }
            (editor_text(&read.text, version), Some(read.revision))
        }
        Err(error) if error.is(&errors::MEMORY_NOT_FOUND) => (NEW_MEMORY.to_owned(), None),
        Err(error) => return Err(daemon_error(error)),
    };
    let mut text = original.clone();
    loop {
        text = io.edit(url, &text)?;
        if text == original {
            io.write_line(&if revision.is_some() {
                "No changes.".to_owned()
            } else {
                format!("No changes; {url} was not created.")
            })?;
            return Ok(());
        }
        match client
            .write_memory(url, text.clone(), revision.clone())
            .await
        {
            Ok(written) => {
                let verb = if revision.is_some() {
                    "Saved"
                } else {
                    "Created"
                };
                io.write_line(&format!("{verb} {}.", written.url))?;
                if let Some(notice) = &written.notice {
                    io.write_line(notice)?;
                }
                return Ok(());
            }
            Err(error) if error.is(&errors::MEMORY_INVALID) => {
                io.write_line(&format!("Not saved: {}", message_of(&error)))?;
                if !io.confirm("Edit again? [Y/n]", true)? {
                    anyhow::bail!("{url} was not changed");
                }
            }
            Err(error)
                if error.is(&errors::MEMORY_REVISION_MISMATCH)
                    || error.is(&errors::MEMORY_EXISTS)
                    || error.is(&errors::MEMORY_NOT_FOUND) =>
            {
                io.write_line(&format!("Not saved: {url} changed since it was opened."))?;
                if !io.confirm(
                    "Reload it and edit the current version? Your edit is dropped. [Y/n]",
                    true,
                )? {
                    anyhow::bail!("{url} was not changed");
                }
                (original, revision) = match client.read_memory(url).await {
                    Ok(read) => (editor_text(&read.text, version), Some(read.revision)),
                    Err(error) if error.is(&errors::MEMORY_NOT_FOUND) => {
                        (NEW_MEMORY.to_owned(), None)
                    }
                    Err(error) => return Err(daemon_error(error)),
                };
                text = original.clone();
            }
            Err(error) => return Err(daemon_error(error)),
        }
    }
}

/// `cookie memory list`: two lines per memory, its URL and description,
/// then its stamps and size against its limit, under a heading per layer
/// and per agent, machine, or project group, then what the memory folder
/// holds besides memories.
pub(crate) fn memory_lines(listed: &MemoryListResult, now: Timestamp) -> Vec<String> {
    let mut lines = Vec::new();
    let mut heading: Option<(MemoryLayer, Option<&str>)> = None;
    for memory in &listed.memories {
        let here = (memory.layer, memory.group.as_deref());
        if heading.map(|(layer, _)| layer) != Some(memory.layer) {
            lines.push(layer_heading(memory.layer).to_owned());
        }
        if heading != Some(here)
            && let Some(group) = &memory.group
        {
            lines.push(format!("  {group}"));
        }
        heading = Some(here);
        lines.push(memory_line(memory));
        lines.push(stamps_line(memory, now));
    }
    if lines.is_empty() {
        lines.push("No memories.".to_owned());
    }
    if !listed.ignored.is_empty() {
        lines.push("Not memories (ignored)".to_owned());
        lines.extend(
            listed
                .ignored
                .iter()
                .map(|ignored| format!("  {}: {}", ignored.path, ignored.reason)),
        );
    }
    lines
}

const fn layer_heading(layer: MemoryLayer) -> &'static str {
    match layer {
        MemoryLayer::User => "User",
        MemoryLayer::Agent => "Agents",
        MemoryLayer::Machine => "Machines",
        MemoryLayer::Project => "Projects",
        MemoryLayer::Unknown => "Other",
    }
}

fn memory_line(memory: &MemoryInfo) -> String {
    let indent = if memory.group.is_some() { "    " } else { "  " };
    let mut line = format!("{indent}{}", memory.url);
    match (&memory.invalid, &memory.description) {
        (Some(reason), _) => line.push_str(&format!("  INVALID: {reason}")),
        (None, Some(description)) => line.push_str(&format!("  {description}")),
        (None, None) => {}
    }
    if memory.orphaned {
        line.push_str("  (no current agent)");
    }
    line
}

/// The line under a memory's: `created 3 weeks ago · modified 2 days ago ·
/// last read 1 hour ago · 812 bytes of 2 KB`, with what the daemon reports.
fn stamps_line(memory: &MemoryInfo, now: Timestamp) -> String {
    let indent = if memory.group.is_some() {
        "      "
    } else {
        "    "
    };
    format!("{indent}{}", MemoryStamps::of_info(memory).ages(now))
}
