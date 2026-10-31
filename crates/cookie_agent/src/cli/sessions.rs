//! `cookie sessions`: converting session logs to this build's history
//! format.

use std::path::Path;

use anyhow::Context as _;
use clap::Subcommand;

use super::data_dir;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(super) enum SessionsCommand {
    /// Convert the sessions of this directory to this build's history
    /// format now, as opening each one would. The originals stay beside
    /// them as `events.jsonl.v<N>.bak`. A session another cookie process
    /// holds is left for it to convert.
    Upgrade {
        /// Convert the sessions of every directory, not only this one's.
        #[arg(long)]
        all: bool,
    },
}

/// `cookie sessions upgrade [--all]`: converts session trees on disk and
/// reports each one that needed it, or could not be converted.
pub(super) fn run_sessions(command: SessionsCommand, workspace: &Path) -> anyhow::Result<()> {
    let SessionsCommand::Upgrade { all } = command;
    let data_root = data_dir()?;
    let upgrades =
        cookie_agent_engine::session::upgrade_sessions(&data_root, (!all).then_some(workspace))
            .context("upgrade sessions")?;
    let (mut current, mut converted, mut in_use, mut failed) = (0, 0, 0, 0);
    for (_, root, upgrade) in &upgrades {
        match upgrade {
            cookie_agent_engine::session::TreeUpgrade::Current => current += 1,
            cookie_agent_engine::session::TreeUpgrade::Converted(conversion) => {
                if !conversion.converted.is_empty() {
                    converted += 1;
                    let formats = conversion
                        .converted
                        .iter()
                        .map(|(_, format)| *format)
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .map(|format| format.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "{root}: converted {} log(s) from history format {formats} to {}",
                        conversion.converted.len(),
                        cookie_agent_protocol::HISTORY_FORMAT
                    );
                }
                for (session, format) in &conversion.left {
                    failed += 1;
                    match format {
                        None => println!(
                            "{root}: {session} was written before history formats and is not \
                             converted"
                        ),
                        Some(format) => println!(
                            "{root}: {session} has history format {format}, which this build \
                             cannot convert"
                        ),
                    }
                }
            }
            cookie_agent_engine::session::TreeUpgrade::InUse => {
                in_use += 1;
                println!("{root}: skipped, another cookie process holds it");
            }
            cookie_agent_engine::session::TreeUpgrade::Failed(error) => {
                failed += 1;
                println!(
                    "{root}: not converted: {}",
                    cookie_agent_protocol::diagnostics::detail(&error.to_string())
                );
            }
        }
    }
    println!(
        "{} session tree(s): {converted} converted, {current} already current, {in_use} in use; \
         {failed} session(s) not converted",
        upgrades.len()
    );
    anyhow::ensure!(failed == 0, "{failed} session(s) could not be converted");
    Ok(())
}
