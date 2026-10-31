//! Where a `transfer` reads and writes: one of the session's attachments on
//! the machine it names (`docs/site/specs/machines-on-demand.md` section 6).

use cookie_agent_protocol::{RuntimeId, SessionId};
use cookie_agent_runtime_api::ResourceScope;

use crate::runtime::Engine;
use crate::runtimes::InstanceConnection;

/// One end of a transfer on a machine: the attachment's runtime connection,
/// and where its path permission is checked.
pub(crate) struct TransferEndpoint {
    pub(crate) connection: InstanceConnection,
    /// The machine as the session's machines block names it.
    pub(crate) machine: String,
    pub(crate) scope: ResourceScope,
}

impl Engine {
    /// The end of a transfer on `machine`: the session's attachment there
    /// whose runtime takes transfers.
    pub(crate) fn transfer_endpoint(
        &self,
        session: SessionId,
        machine: &str,
    ) -> Result<TransferEndpoint, String> {
        let attachments = self
            .session_runtimes(session)
            .map_err(|error| error.to_string())?;
        let mut machines = std::collections::BTreeSet::new();
        let mut refusals = Vec::new();
        for attachment in attachments {
            if attachment.runtime_id.as_str() == RuntimeId::GLOBAL {
                continue;
            }
            let named = self
                .runtime_machine(attachment.runtime_id.as_str())
                .map_or_else(
                    || attachment.runtime_id.to_string(),
                    |name| name.to_string(),
                );
            machines.insert(named.clone());
            if named != machine {
                continue;
            }
            let Some(instance) = self.inner.runtimes.instance(&attachment) else {
                refusals.push(format!(
                    "runtime `{}` is not connected",
                    attachment.runtime_id
                ));
                continue;
            };
            let Some(connection) = instance.connection() else {
                refusals.push(format!(
                    "runtime `{}` runs inside the daemon without the runtime protocol",
                    attachment.runtime_id
                ));
                continue;
            };
            if !connection.client.files() {
                refusals.push(format!(
                    "runtime `{}` speaks runtime protocol {}, which has no transfers (1.13.0 \
                     and later have them): update it",
                    attachment.runtime_id,
                    connection
                        .client
                        .protocol_version()
                        .map_or_else(|| "unknown".to_owned(), |version| version.to_string())
                ));
                continue;
            }
            let info = instance.info();
            return Ok(TransferEndpoint {
                scope: ResourceScope {
                    runtime: attachment.runtime_id.to_string(),
                    workspace: info
                        .workspace()
                        .map(|workspace| workspace.root().to_owned()),
                    home: info.home().map(str::to_owned),
                    platform: info
                        .workspace()
                        .and_then(|workspace| workspace.platform().map(str::to_owned)),
                },
                connection,
                machine: named,
            });
        }
        if refusals.is_empty() {
            let known = if machines.is_empty() {
                "none".to_owned()
            } else {
                machines
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            return Err(format!(
                "machine `{machine}` is not attached to this session (its machines: {known}); \
                 attach it with attach_machine first"
            ));
        }
        Err(format!(
            "machine `{machine}` takes no transfers: {}",
            refusals.join("; ")
        ))
    }

    /// Whether `session` has a machine attached whose runtime takes
    /// transfers, which `transfer` is offered for.
    pub(crate) fn session_takes_transfers(&self, session: SessionId) -> bool {
        self.session_runtimes(session)
            .unwrap_or_default()
            .iter()
            .filter(|attachment| attachment.runtime_id.as_str() != RuntimeId::GLOBAL)
            .any(|attachment| {
                self.inner
                    .runtimes
                    .instance(attachment)
                    .and_then(|instance| instance.connection())
                    .is_some_and(|connection| connection.client.files())
            })
    }

    /// The most bytes one transfer moves, when `[transfer] max_bytes` sets
    /// a limit.
    pub(crate) fn transfer_limit(&self) -> Option<u64> {
        self.inner.config.load().runtime.transfer.limit()
    }
}

impl Engine {
    /// The artifact a transfer's `from` names in `session`'s tree, and its
    /// length; see [`crate::runtime::artifacts::reads::transfer_source`].
    pub(crate) async fn transfer_artifact(
        &self,
        session: SessionId,
        artifact: String,
        stream: Option<String>,
    ) -> Result<(String, u64), crate::ToolError> {
        let store = std::sync::Arc::clone(&self.inner.artifacts);
        crate::runtime::blocking_io::run(move || {
            crate::runtime::artifacts::reads::transfer_source(
                &store,
                session,
                &artifact,
                stream.as_deref(),
            )
        })
        .await?
    }

    /// `length` bytes of blob `digest` of `session`'s tree from `offset`.
    pub(crate) async fn transfer_artifact_chunk(
        &self,
        session: SessionId,
        digest: String,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, crate::ToolError> {
        let store = std::sync::Arc::clone(&self.inner.artifacts);
        crate::runtime::blocking_io::run(move || {
            crate::runtime::artifacts::reads::read_transfer_chunk(
                &store, session, &digest, offset, length,
            )
        })
        .await?
    }
}
