//! `cookie machine`: list and regroup the machines a daemon's runtimes run
//! on with the `machines.*` methods, so it works against a remote daemon.

use anyhow::Context as _;
use clap::Subcommand;
use cookie_agent_protocol::{
    MachineAssignment, MachineInfo, MachineName, MachinesListResult, RuntimeId,
};
use cookie_agent_protocol_transport::Client;

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum MachineCommand {
    /// List the machines: their machine IDs, pinned, connected, and
    /// remembered offline runtimes, platform, and memories.
    List,
    /// Rename a machine; its memory folder moves with it.
    Rename {
        machine: MachineName,
        name: MachineName,
    },
    /// Pin a runtime ID to a machine, created when the name is new.
    Assign {
        runtime: RuntimeId,
        machine: MachineName,
    },
    /// Remove a runtime's pin, so that it follows the machine ID it reports.
    Unassign { runtime: RuntimeId },
    /// Move one machine's IDs, pins, and memories into another and remove
    /// it.
    Merge {
        from: MachineName,
        into: MachineName,
    },
    /// Remove a machine that has no connected runtimes and no memories.
    Forget { machine: MachineName },
    /// Forget where an offline runtime ran, so it is no longer listed on
    /// its machine.
    ForgetRuntime { runtime: RuntimeId },
}

pub(crate) async fn run_machine(
    url: &str,
    token: &str,
    command: MachineCommand,
) -> anyhow::Result<()> {
    let client = Client::connect_websocket_with_token(url, token).await?;
    client
        .handshake_as(super::client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    for line in run_machine_with(&client, command).await? {
        println!("{line}");
    }
    Ok(())
}

/// Runs `command` against `client`; the lines to print.
pub(crate) async fn run_machine_with(
    client: &Client,
    command: MachineCommand,
) -> anyhow::Result<Vec<String>> {
    match command {
        MachineCommand::List => {
            let listed = client
                .list_machines()
                .await
                .context("machines.list failed")?;
            Ok(machine_lines(&listed))
        }
        MachineCommand::Rename { machine, name } => {
            client
                .rename_machine(machine.clone(), name.clone())
                .await
                .context("machines.rename failed")?;
            Ok(vec![format!("Renamed machine {machine} to {name}.")])
        }
        MachineCommand::Assign { runtime, machine } => {
            client
                .assign_machine(runtime.clone(), machine.clone())
                .await
                .context("machines.assign failed")?;
            Ok(vec![format!(
                "Pinned runtime {runtime} to machine {machine}."
            )])
        }
        MachineCommand::Unassign { runtime } => {
            client
                .unassign_machine(runtime.clone())
                .await
                .context("machines.unassign failed")?;
            Ok(vec![format!(
                "Unpinned runtime {runtime}; it follows the machine ID it reports."
            )])
        }
        MachineCommand::Merge { from, into } => {
            client
                .merge_machines(from.clone(), into.clone())
                .await
                .context("machines.merge failed")?;
            Ok(vec![format!("Merged machine {from} into {into}.")])
        }
        MachineCommand::Forget { machine } => {
            client
                .forget_machine(machine.clone())
                .await
                .context("machines.forget failed")?;
            Ok(vec![format!("Forgot machine {machine}.")])
        }
        MachineCommand::ForgetRuntime { runtime } => {
            client
                .forget_machine_runtime(runtime.clone())
                .await
                .context("machines.forget_runtime failed")?;
            Ok(vec![format!("Forgot where runtime {runtime} ran.")])
        }
    }
}

/// `cookie machine list`: one heading line per machine, then its runtimes
/// and machine IDs, indented.
pub(crate) fn machine_lines(listed: &MachinesListResult) -> Vec<String> {
    if listed.machines.is_empty() {
        return vec!["No machines yet.".to_owned()];
    }
    listed.machines.iter().flat_map(machine_block).collect()
}

fn machine_block(machine: &MachineInfo) -> Vec<String> {
    let mut heading = machine.name.to_string();
    if let Some(platform) = &machine.platform {
        heading.push_str(&format!(" ({platform})"));
    }
    heading.push_str(&match machine.memories {
        1 => ", 1 memory".to_owned(),
        count => format!(", {count} memories"),
    });
    let mut lines = vec![heading];
    let connected = machine
        .connected
        .iter()
        .map(|runtime| {
            let why = match runtime.assigned_by {
                MachineAssignment::Pinned => "pinned",
                MachineAssignment::MachineId => "by machine ID",
                MachineAssignment::LastSeen => "last seen here",
                MachineAssignment::Unknown => "for a reason this build does not know",
            };
            format!("{} ({why})", runtime.runtime_id)
        })
        .collect::<Vec<_>>();
    if !connected.is_empty() {
        lines.push(format!("  connected: {}", connected.join(", ")));
    }
    let offline = machine
        .remembered
        .iter()
        .map(|runtime| {
            let seen = runtime.last_seen.strftime("%Y-%m-%d %H:%M UTC");
            match runtime.assigned_by {
                MachineAssignment::Pinned => {
                    format!("{} (pinned, last seen {seen})", runtime.runtime_id)
                }
                _ => format!("{} (last seen {seen})", runtime.runtime_id),
            }
        })
        .collect::<Vec<_>>();
    if !offline.is_empty() {
        lines.push(format!("  offline: {}", offline.join(", ")));
    }
    if !machine.pinned.is_empty() {
        let pinned = machine
            .pinned
            .iter()
            .map(RuntimeId::to_string)
            .collect::<Vec<_>>();
        lines.push(format!("  pinned: {}", pinned.join(", ")));
    }
    if !machine.ids.is_empty() {
        lines.push(format!("  machine IDs: {}", machine.ids.join(", ")));
    }
    lines
}
