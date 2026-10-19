//! `cookie memory`: list, edit, and delete a daemon's memory files with the
//! `memory.*` methods, so it works against a remote daemon
//! (`docs/site/specs/memory.md` section 10).

use anyhow::Context as _;
use clap::Subcommand;
use cookie_agent_protocol::{
    Client, ClientError, MemoryInfo, MemoryLayer, MemoryListParams, MemoryListResult, errors,
};

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum MemoryCommand {
    /// List the memories, of every layer or of one: `user`, `agent`,
    /// `machine`, or `project`. Invalid files and entries that are not
    /// memories are listed with the reason.
    List {
        #[arg(value_parser = parse_layer)]
        layer: Option<MemoryLayer>,
    },
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

/// The terminal and `$VISUAL`, else `$EDITOR`, else `vi`.
struct StdioMemoryIo;

impl MemoryIo for StdioMemoryIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        println!("{line}");
        Ok(())
    }

    fn edit(&mut self, _url: &str, text: &str) -> anyhow::Result<String> {
        crate::config_command::edit_in_editor("cookie-memory-", ".md", text)
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
        .handshake_as(crate::client_identity("cli"))
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
            for line in memory_lines(&listed) {
                io.write_line(&line)?;
            }
            Ok(())
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

/// A daemon refusal as its message and hint, the way a user reads it.
fn daemon_error(error: ClientError) -> anyhow::Error {
    let hint = error.error_data().and_then(|data| data.hint);
    let error = anyhow::Error::new(error);
    match hint {
        Some(hint) => error.context(hint),
        None => error,
    }
}

/// The message of a daemon error, without its code.
fn message_of(error: &ClientError) -> String {
    match error {
        ClientError::Rpc(rpc) => rpc.message.clone(),
        error => error.to_string(),
    }
}

/// Opens `url` in the editor and writes it back at the revision read; a new
/// memory starts from [`NEW_MEMORY`]. Invalid text can be edited again; a
/// memory that changed meanwhile can be reloaded and edited again.
async fn edit<I: MemoryIo>(client: &Client, url: &str, io: &mut I) -> anyhow::Result<()> {
    let (mut original, mut revision) = match client.read_memory(url).await {
        Ok(read) => {
            if let Some(reason) = &read.invalid {
                io.write_line(&format!("{} is not a valid memory: {reason}", read.url))?;
            }
            (read.text, Some(read.revision))
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
                return io.write_line(&format!("{verb} {}.", written.url));
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
                    Ok(read) => (read.text, Some(read.revision)),
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

/// `cookie memory list`: one line per memory, under a heading per layer and
/// per agent, machine, or project group, then what the memory folder holds
/// besides memories.
pub(crate) fn memory_lines(listed: &MemoryListResult) -> Vec<String> {
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
    line.push_str(&format!("  [{} B]", memory.size));
    line
}
