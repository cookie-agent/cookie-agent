//! `cookie project`: list, create, rename, delete, and edit a daemon's
//! projects with the `project.*` methods, so it works against a remote
//! daemon (`docs/site/specs/projects.md` section 10.2).

use anyhow::Context as _;
use clap::Subcommand;
use cookie_agent_protocol::{
    Client, ClientError, ProjectCreateParams, ProjectDefaults, ProjectInfo, ProjectName,
    ProjectRuntime, ProjectUpdateParams, RuntimeId, SessionId, errors,
};

use crate::memory_command::MemoryIo;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum ProjectCommand {
    /// List the projects with their sessions, memories, and defaults.
    List,
    /// Create a project, with an empty `project://<name>/overview` memory.
    Create {
        #[arg(value_parser = parse_project_name)]
        name: ProjectName,
        /// One emoji clients show before the name.
        #[arg(long)]
        emoji: Option<String>,
    },
    /// Rename a project; its memory folder moves with it.
    Rename {
        #[arg(value_parser = parse_project_name)]
        old: ProjectName,
        #[arg(value_parser = parse_project_name)]
        new: ProjectName,
    },
    /// Delete a project with its sessions, memories, and `AGENTS.md`; asks
    /// first unless `--yes`.
    Delete {
        #[arg(value_parser = parse_project_name)]
        name: ProjectName,
        /// Delete without asking.
        #[arg(long, short)]
        yes: bool,
    },
    /// Edit a project's `AGENTS.md` in `$VISUAL` or `$EDITOR`; an empty
    /// file removes it.
    Edit {
        #[arg(value_parser = parse_project_name)]
        name: ProjectName,
    },
    /// Set a project's defaults from a session: its runtime attachments,
    /// its agent, and its agent preset.
    Defaults {
        #[arg(value_parser = parse_project_name)]
        name: ProjectName,
        /// The session whose attachments and agent new sessions start with.
        #[arg(long, value_name = "SESSION_ID")]
        from_session: SessionId,
    },
}

pub(crate) fn parse_project_name(value: &str) -> Result<ProjectName, String> {
    ProjectName::new(value).map_err(|_| {
        format!(
            "`{value}` is not a project name: a lowercase letter or digit, then up to 63 lowercase letters, digits, or `-`"
        )
    })
}

pub(crate) async fn run_project(
    url: &str,
    token: &str,
    command: ProjectCommand,
) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    match &command {
        ProjectCommand::Edit { .. } => anyhow::ensure!(
            interactive,
            "cookie project edit requires an interactive terminal"
        ),
        ProjectCommand::Delete { yes: false, .. } => anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "cookie project delete asks before deleting; pass --yes to delete without a terminal"
        ),
        _ => {}
    }
    let client = Client::connect_websocket_with_token(url, token).await?;
    client
        .handshake_as(crate::client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    run_project_with(&client, command, &mut crate::memory_command::StdioMemoryIo).await
}

/// The project named `name`.
pub(crate) async fn find_project(
    client: &Client,
    name: &ProjectName,
) -> anyhow::Result<ProjectInfo> {
    client
        .list_projects()
        .await
        .context("projects.list failed")?
        .projects
        .into_iter()
        .find(|project| project.name == *name)
        .with_context(|| format!("there is no project named {name}; see `cookie project list`"))
}

pub(crate) async fn run_project_with<I: MemoryIo>(
    client: &Client,
    command: ProjectCommand,
    io: &mut I,
) -> anyhow::Result<()> {
    match command {
        ProjectCommand::List => {
            let listed = client
                .list_projects()
                .await
                .context("projects.list failed")?;
            if listed.projects.is_empty() {
                return io.write_line("No projects.");
            }
            for project in &listed.projects {
                io.write_line(&project_line(project))?;
            }
            Ok(())
        }
        ProjectCommand::Create { name, emoji } => {
            let created = client
                .create_project(ProjectCreateParams {
                    name,
                    emoji,
                    defaults: None,
                })
                .await
                .map_err(anyhow::Error::new)?;
            io.write_line(&format!("Created project {}.", created.project.name))
        }
        ProjectCommand::Rename { old, new } => {
            let project = find_project(client, &old).await?;
            let mut params = ProjectUpdateParams::new(project.id, project.revision);
            params.name = Some(new);
            let renamed = client
                .update_project(params)
                .await
                .map_err(anyhow::Error::new)?;
            io.write_line(&format!(
                "Renamed project {old} to {}; its memories are project://{}/….",
                renamed.project.name, renamed.project.name
            ))
        }
        ProjectCommand::Delete { name, yes } => {
            let project = find_project(client, &name).await?;
            let question = format!(
                "Delete project {name} with {} and {}? [y/N]",
                count(project.sessions, "session"),
                count(project.memories, "memory")
            );
            if !yes && !io.confirm(&question, false)? {
                return io.write_line(&format!("Project {name} was not deleted."));
            }
            let deleted = client
                .delete_project(project.id, project.revision)
                .await
                .map_err(anyhow::Error::new)?;
            io.write_line(&format!(
                "Deleted project {name}, {} and {}.",
                count(deleted.deleted_sessions, "session"),
                count(deleted.deleted_memories, "memory")
            ))
        }
        ProjectCommand::Edit { name } => edit(client, &name, io).await,
        ProjectCommand::Defaults { name, from_session } => {
            let project = find_project(client, &name).await?;
            let session = client
                .get_session(cookie_agent_protocol::SessionGetParams {
                    session_id: from_session,
                })
                .await
                .map_err(anyhow::Error::new)?;
            let defaults = ProjectDefaults {
                preset: session.session.creation_selection.preset.clone(),
                agent: Some(session.session.creation_selection.agent.clone()),
                global: session
                    .runtimes
                    .iter()
                    .any(|attachment| attachment.runtime_id.as_str() == RuntimeId::GLOBAL),
                runtimes: session
                    .runtimes
                    .iter()
                    .filter(|attachment| attachment.runtime_id.as_str() != RuntimeId::GLOBAL)
                    .map(|attachment| ProjectRuntime {
                        runtime_id: attachment.runtime_id.clone(),
                        capability: Some(attachment.capability.clone()),
                        workspace: attachment.workspace.clone(),
                    })
                    .collect(),
            };
            let mut params = ProjectUpdateParams::new(project.id, project.revision);
            params.defaults = Some(Some(defaults));
            let updated = client
                .update_project(params)
                .await
                .map_err(anyhow::Error::new)?;
            io.write_line(&format!("Set the defaults of project {name}."))?;
            io.write_line(&project_line(&updated.project))
        }
    }
}

fn count(value: u64, noun: &str) -> String {
    match (value, noun) {
        (1, noun) => format!("1 {noun}"),
        (value, "memory") => format!("{value} memories"),
        (value, noun) => format!("{value} {noun}s"),
    }
}

/// `cookie project list`: one line per project.
pub(crate) fn project_line(project: &ProjectInfo) -> String {
    let mut line = match &project.emoji {
        Some(emoji) => format!("{emoji} {}", project.name),
        None => project.name.to_string(),
    };
    line.push_str(&format!(
        "  {}, {}",
        count(project.sessions, "session"),
        count(project.memories, "memory")
    ));
    if project.has_agents_md {
        line.push_str(", AGENTS.md");
    }
    if let Some(defaults) = &project.defaults {
        let mut parts = Vec::new();
        if let Some(agent) = &defaults.agent {
            parts.push(match &defaults.preset {
                Some(preset) => format!("agent {preset}/{agent}"),
                None => format!("agent {agent}"),
            });
        } else if let Some(preset) = &defaults.preset {
            parts.push(format!("preset {preset}"));
        }
        if !defaults.global {
            parts.push("no global".to_owned());
        }
        parts.extend(
            defaults
                .runtimes
                .iter()
                .map(|runtime| match &runtime.workspace {
                    Some(workspace) => format!("{} {workspace}", runtime.runtime_id),
                    None => runtime.runtime_id.to_string(),
                }),
        );
        if !parts.is_empty() {
            line.push_str(&format!("  [{}]", parts.join(", ")));
        }
    }
    line
}

/// Opens the project's `AGENTS.md` in the editor and saves it at the
/// revision read; a project changed meanwhile can be reloaded and edited
/// again.
async fn edit<I: MemoryIo>(client: &Client, name: &ProjectName, io: &mut I) -> anyhow::Result<()> {
    let label = format!("{name}/AGENTS.md");
    let load = || async {
        let project = find_project(client, name).await?;
        let got = client
            .get_project(project.id)
            .await
            .map_err(anyhow::Error::new)?;
        anyhow::Ok((
            got.project,
            got.agents_md.map(|agents| agents.text).unwrap_or_default(),
        ))
    };
    let (mut project, mut original) = load().await?;
    let mut text = original.clone();
    loop {
        text = io.edit(&label, &text)?;
        if text == original {
            return io.write_line("No changes.");
        }
        let mut params = ProjectUpdateParams::new(project.id.clone(), project.revision.clone());
        params.agents_md = Some(text.clone());
        match client.update_project(params).await {
            Ok(_) if text.is_empty() => {
                return io.write_line(&format!("Removed the AGENTS.md of project {name}."));
            }
            Ok(_) => return io.write_line(&format!("Saved the AGENTS.md of project {name}.")),
            Err(error) if error.is(&errors::PROJECT_CHANGED) => {
                io.write_line(&format!(
                    "Not saved: project {name} changed since it was opened."
                ))?;
                if !io.confirm(
                    "Reload it and edit the current version? Your edit is dropped. [Y/n]",
                    true,
                )? {
                    anyhow::bail!("the AGENTS.md of project {name} was not changed");
                }
                (project, original) = load().await?;
                text = original.clone();
            }
            Err(error) => return Err(daemon_error(error)),
        }
    }
}

fn daemon_error(error: ClientError) -> anyhow::Error {
    anyhow::Error::new(error)
}
