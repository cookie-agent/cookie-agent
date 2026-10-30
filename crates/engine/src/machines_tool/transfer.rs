//! `transfer`: copies a file or folder to a machine of the session, from
//! another of its machines or from an artifact of its tree
//! (`docs/site/specs/machines-on-demand.md` section 6). The daemon relays
//! the bytes a chunk at a time and keeps none of them: the call's output is
//! a summary, and its progress is display only.

use cookie_agent_protocol::{
    ApprovalResourceSource, PermissionAction, PersistedToolResult as ToolResult,
    PreparedBindingLifetime, ToolCallId, human_bytes,
    runtime_protocol::{FsDirection, FsKind, FsPrepareResult, FsStreamSummary, MAX_FS_CHUNK_BYTES},
};
use cookie_agent_runtime_api::{
    PreparedExecutor, PreparedTool, ToolCall, ToolCompletion, ToolConcurrency, ToolError,
    ToolExecutionContext, ToolPreparationContext, ToolProgress, ToolResultTruncationPolicy,
    ToolSpec,
    authoring::{parse_args, prepared_operation, prepared_resource, safe_title, schema},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Engine, runtimes::RuntimeClient};

pub(crate) const TRANSFER: &str = "transfer";

const DESCRIPTION: &str = "Copy a file or folder to a machine of this session: from another of its machines, or from an artifact of this session, such as a file the user sent, whose label names it (`[file report.zip, 3.2 MB, artifact 5f2c81d0]`). `from` is `{\"machine\": \"labpc\", \"path\": \"~/data\"}` or `{\"artifact\": \"5f2c81d0\"}` (with `stream` for one stream of retained tool output); `to` is `{\"machine\": \"laptop\", \"path\": \"~/Downloads/data\"}`, the path the copy will have, not a folder to put it in. Machines are named as the machines block and list_runtimes name them; a relative path resolves against that machine's working directory, and `~` against its home. A folder is copied with everything below it. An existing destination is an error unless `overwrite` is true, which replaces a file or merges into a folder. The daemon relays the bytes, so the machines need not reach each other. The source is checked as `read` on its machine and the destination as `write` on its own. The result gives the bytes and files copied and, for a file, its SHA-256.";

/// `transfer`'s arguments.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct TransferArgs {
    /// Where the bytes come from: `{"machine", "path"}` for a file or folder
    /// on a machine, or `{"artifact"}` (a file label's digest, a longer
    /// prefix, or the full digest) with an optional `stream`
    from: Source,
    /// Where they go: `{"machine", "path"}`, the path the copy will have
    to: MachinePath,
    /// Replace an existing file, or merge into an existing folder. Defaults
    /// to false: an existing destination is an error
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(untagged)]
enum Source {
    Machine(MachinePath),
    Artifact(ArtifactSource),
}

/// A path on one of the session's machines.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct MachinePath {
    /// A machine of this session
    machine: String,
    /// Relative to the machine's working directory, absolute, or under `~`
    path: String,
}

/// An artifact of this session.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactSource {
    /// Its digest, or the first 8 or more hex digits of it
    artifact: String,
    /// One stream of retained tool output, such as `stdout`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream: Option<String>,
}

pub(super) fn spec() -> ToolSpec {
    ToolSpec {
        background: true,
        background_default: false,
        collision: Default::default(),
        output: Default::default(),
        concurrency: ToolConcurrency::Parallel,
        result_truncation: ToolResultTruncationPolicy::OptOut,
        name: TRANSFER.into(),
        // Visible to a run that may write; each end is checked on its own
        // machine.
        permission_name: Some(PermissionAction::WRITE.as_str().to_owned()),
        description: DESCRIPTION.into(),
        parameters: schema::<TransferArgs>(),
    }
}

/// `machine:path` as a call shows its ends.
fn shown_args(arguments: &serde_json::Value) -> Result<(String, String), ToolError> {
    let args: TransferArgs = parse_args(TRANSFER, arguments.clone())?;
    let from = match &args.from {
        Source::Machine(from) => format!("{}:{}", from.machine, from.path),
        Source::Artifact(from) => match &from.stream {
            Some(stream) => format!("artifact {}/{stream}", short(&from.artifact)),
            None => format!("artifact {}", short(&from.artifact)),
        },
    };
    Ok((from, format!("{}:{}", args.to.machine, args.to.path)))
}

fn short(digest: &str) -> &str {
    &digest[..digest.len().min(12)]
}

pub(super) fn display_argument(arguments: &serde_json::Value) -> Result<String, ToolError> {
    let (from, to) = shown_args(arguments)?;
    Ok(format!("{from} → {to}"))
}

/// The permission resource of the destination, as the write resource of
/// the call.
pub(super) fn permission_resource(arguments: &serde_json::Value) -> Result<String, ToolError> {
    let args: TransferArgs = parse_args(TRANSFER, arguments.clone())?;
    Ok(args.to.path)
}

pub(super) fn presentation(call: &ToolCall) -> cookie_agent_protocol::PreparedPresentation {
    match shown_args(&call.arguments) {
        Ok((from, to)) => crate::base_tools::base_presentation(
            "Transfer",
            cookie_agent_protocol::ToolHint::GENERIC,
            &format!("{from} → {to}"),
            vec![("from", from), ("to", to)],
        ),
        Err(_) => cookie_agent_runtime_api::tool_title_only(&call.name),
    }
}

/// A route or handle on a runtime, closed when it is dropped unspent.
struct Held {
    client: RuntimeClient,
    id: u64,
    armed: bool,
}

impl Held {
    fn new(client: RuntimeClient, id: u64) -> Self {
        Self {
            client,
            id,
            armed: true,
        }
    }

    /// The runtime spent it: nothing to close.
    fn spend(&mut self) -> u64 {
        self.armed = false;
        self.id
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            let id = self.id;
            handle.spawn(async move { client.fs_close(id).await });
        }
    }
}

/// A prepared end on a machine.
struct MachineEnd {
    route: Held,
    machine: String,
    prepared: FsPrepareResult,
}

impl MachineEnd {
    fn shown(&self) -> String {
        format!("{} on {}", self.prepared.path, self.machine)
    }
}

enum SourceEnd {
    Machine(MachineEnd),
    Artifact { digest: String, size: u64 },
}

pub(super) async fn prepare(
    engine: Engine,
    ctx: ToolPreparationContext,
    call: ToolCall,
) -> Result<PreparedTool, ToolError> {
    let args: TransferArgs = parse_args(TRANSFER, call.arguments)?;
    let failed = ToolError::execution;
    let limit = engine.transfer_limit();
    let too_large = |size: u64| {
        limit.filter(|limit| size > *limit).map(|limit| {
            ToolError::resource_limit(format!(
                "the source holds {} ({size} bytes), more than the {} `[transfer] max_bytes` allows",
                human_bytes(size),
                human_bytes(limit)
            ))
        })
    };
    let destination_endpoint = engine
        .transfer_endpoint(ctx.session, &args.to.machine)
        .map_err(failed)?;
    let mut resources = Vec::new();
    let mut labels = Vec::new();
    let mut scopes = Vec::new();
    let mut capabilities = Vec::new();
    let source = match &args.from {
        Source::Machine(from) => {
            let endpoint = engine
                .transfer_endpoint(ctx.session, &from.machine)
                .map_err(failed)?;
            let prepared = endpoint
                .connection
                .fs_prepare(&from.path, FsDirection::Read)
                .await
                .map_err(failed)?;
            let end = MachineEnd {
                route: Held::new(endpoint.connection.client.clone(), prepared.route),
                machine: endpoint.machine,
                prepared,
            };
            if let Some(error) = end.prepared.size.and_then(too_large) {
                return Err(error);
            }
            resources.push(path_resource(PermissionAction::READ, &end.prepared)?);
            labels.push(end.prepared.label.clone());
            scopes.push(endpoint.scope);
            capabilities.push((PermissionAction::READ, "read"));
            SourceEnd::Machine(end)
        }
        Source::Artifact(from) => {
            let (digest, size) = engine
                .transfer_artifact(ctx.session, from.artifact.clone(), from.stream.clone())
                .await?;
            if let Some(error) = too_large(size) {
                return Err(error);
            }
            SourceEnd::Artifact { digest, size }
        }
    };
    let prepared = destination_endpoint
        .connection
        .fs_prepare(&args.to.path, FsDirection::Write)
        .await
        .map_err(failed)?;
    let destination = MachineEnd {
        route: Held::new(
            destination_endpoint.connection.client.clone(),
            prepared.route,
        ),
        machine: destination_endpoint.machine,
        prepared,
    };
    let source_kind = match &source {
        SourceEnd::Machine(end) => end.prepared.kind.unwrap_or(FsKind::File),
        SourceEnd::Artifact { .. } => FsKind::File,
    };
    // Refused before anyone is asked to approve it.
    match destination.prepared.kind {
        Some(existing) if !args.overwrite => {
            return Err(failed(format!(
                "{} exists already ({}); pass overwrite: true to replace it",
                destination.shown(),
                existing.as_str()
            )));
        }
        Some(existing) if existing != source_kind => {
            return Err(failed(format!(
                "{} is a {}, and the source is a {}",
                destination.shown(),
                existing.as_str(),
                source_kind.as_str()
            )));
        }
        _ => {}
    }
    resources.push(path_resource(
        PermissionAction::WRITE,
        &destination.prepared,
    )?);
    labels.push(destination.prepared.label.clone());
    scopes.push(destination_endpoint.scope);
    capabilities.push((PermissionAction::WRITE, "write"));
    let normalized = serde_json::to_value(&args).map_err(|error| failed(error.to_string()))?;
    let operation = prepared_operation(
        TRANSFER,
        &normalized,
        capabilities,
        resources,
        &ctx.context_bytes(),
    )?;
    let same_runtime = match &source {
        SourceEnd::Machine(end) => {
            end.route.client.runtime_id() == destination.route.client.runtime_id()
        }
        SourceEnd::Artifact { .. } => false,
    };
    let executor = TransferExecutor {
        engine,
        session: ctx.session,
        call_id: call.id,
        source,
        destination,
        overwrite: args.overwrite,
        same_runtime,
        limit,
    };
    PreparedTool::new(operation, normalized, None, Box::new(executor))?
        .with_policy_labels(labels)?
        .with_resource_scopes(scopes)
}

/// A path end's permission resource: its label, bound to where its route
/// led, which a restart forgets.
fn path_resource(
    action: PermissionAction,
    prepared: &FsPrepareResult,
) -> Result<cookie_agent_protocol::PreparedApprovalResource, ToolError> {
    prepared_resource(
        action,
        "path",
        prepared.label.as_bytes(),
        prepared.binding.as_bytes(),
        PreparedBindingLifetime::ProcessLocal,
        ApprovalResourceSource::PrimaryOperation,
    )
}

struct TransferExecutor {
    engine: Engine,
    session: cookie_agent_protocol::SessionId,
    call_id: ToolCallId,
    source: SourceEnd,
    destination: MachineEnd,
    overwrite: bool,
    /// Both ends on one runtime: it copies on its own.
    same_runtime: bool,
    limit: Option<u64>,
}

/// The bytes a base64 text decodes to.
fn decoded_length(data: &str) -> u64 {
    let padding = data.bytes().rev().take_while(|byte| *byte == b'=').count();
    (data.len() / 4 * 3).saturating_sub(padding) as u64
}

/// How often a transfer reports how far it got.
const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// What moved, for the result: complete, or as far as a failure got.
#[derive(Default)]
struct Moved {
    bytes: u64,
}

impl TransferExecutor {
    async fn progress(&self, context: &ToolExecutionContext, moved: u64, size: Option<u64>) {
        let message = match size {
            Some(size) => format!("{} of {}", human_bytes(moved), human_bytes(size)),
            None => format!("{} so far", human_bytes(moved)),
        };
        let _ = context
            .progress
            .send(ToolProgress {
                tool_call_id: self.call_id,
                message,
                display: None,
                output: Vec::new(),
            })
            .await;
    }

    /// The next chunk of the source: base64 data, and the summary at its
    /// end.
    async fn read(
        &self,
        reader: Option<&Held>,
        offset: u64,
    ) -> Result<(String, Option<FsStreamSummary>), String> {
        use base64::Engine as _;

        match (&self.source, reader) {
            (SourceEnd::Machine(end), Some(reader)) => {
                let chunk = end
                    .route
                    .client
                    .fs_read_chunk(reader.id, MAX_FS_CHUNK_BYTES)
                    .await?;
                Ok((chunk.data, chunk.summary))
            }
            (SourceEnd::Artifact { digest, size }, _) => {
                let length = MAX_FS_CHUNK_BYTES.min(size.saturating_sub(offset));
                let data = if length == 0 {
                    Vec::new()
                } else {
                    self.engine
                        .transfer_artifact_chunk(self.session, digest.clone(), offset, length)
                        .await
                        .map_err(|error| error.to_string())?
                };
                let end = offset + data.len() as u64 >= *size;
                Ok((
                    base64::engine::general_purpose::STANDARD.encode(&data),
                    end.then(|| FsStreamSummary {
                        // The artifact is named by its content's SHA-256.
                        sha256: digest.clone(),
                        bytes: *size,
                        files: 1,
                        skipped: 0,
                    }),
                ))
            }
            (SourceEnd::Machine(_), None) => Err("the source is not open".into()),
        }
    }
}

impl TransferExecutor {
    /// Relays the source to the destination through the daemon.
    async fn relay(
        &mut self,
        context: &ToolExecutionContext,
        moved: &mut Moved,
    ) -> Result<cookie_agent_protocol::runtime_protocol::FsWritten, String> {
        let (reader, kind, size) = match &mut self.source {
            SourceEnd::Machine(end) => {
                let route = end.route.spend();
                let opened = end.route.client.fs_read_open(route).await?;
                (
                    Some(Held::new(end.route.client.clone(), opened.handle)),
                    opened.kind,
                    opened.size,
                )
            }
            SourceEnd::Artifact { size, .. } => (None, FsKind::File, Some(*size)),
        };
        if let (Some(limit), Some(size)) = (self.limit, size)
            && size > limit
        {
            return Err(format!(
                "the source holds {}, more than the {} `[transfer] max_bytes` allows",
                human_bytes(size),
                human_bytes(limit)
            ));
        }
        let client = self.destination.route.client.clone();
        let route = self.destination.route.spend();
        let mut writer = Held::new(
            client.clone(),
            client
                .fs_write_open(route, kind, self.overwrite, size)
                .await?,
        );
        let mut reported = std::time::Instant::now();
        let mut offset = 0;
        // One chunk is read while the one before it is written.
        let mut next = self.read(reader.as_ref(), 0).await?;
        let summary = loop {
            let (data, summary) = next;
            let length = decoded_length(&data);
            if let Some(limit) = self.limit
                && offset + length > limit
            {
                return Err(format!(
                    "the folder holds more than the {} `[transfer] max_bytes` allows",
                    human_bytes(limit)
                ));
            }
            if let Some(summary) = summary {
                if length > 0 {
                    client.fs_write_chunk(writer.id, offset, data).await?;
                    offset += length;
                }
                break summary;
            }
            let write = client.fs_write_chunk(writer.id, offset, data);
            let read = self.read(reader.as_ref(), offset + length);
            let (written, read) = tokio::join!(write, read);
            written?;
            offset += length;
            moved.bytes = offset;
            next = read?;
            if reported.elapsed() >= PROGRESS_EVERY {
                reported = std::time::Instant::now();
                self.progress(context, offset, size).await;
            }
        };
        moved.bytes = offset;
        let id = writer.spend();
        let mut written = client.fs_write_commit(id, summary.sha256).await?;
        written.skipped += summary.skipped;
        if kind == FsKind::Folder {
            written.bytes = summary.bytes;
        }
        drop(reader);
        Ok(written)
    }

    /// Copies on the one runtime both ends are on.
    async fn copy(
        &mut self,
        context: &ToolExecutionContext,
    ) -> Result<cookie_agent_protocol::runtime_protocol::FsWritten, String> {
        let SourceEnd::Machine(source) = &mut self.source else {
            return Err("an artifact is never copied on a runtime".into());
        };
        let from = source.route.spend();
        let client = self.destination.route.client.clone();
        let to = self.destination.route.id;
        let copied = tokio::select! {
            copied = client.fs_copy(from, to, self.overwrite) => copied,
            () = context.cancellation.cancelled() => {
                // Closing the destination's route stops the copy and removes
                // its partial copy.
                client.fs_close(to).await;
                Err("the transfer was stopped".into())
            }
        };
        self.destination.route.spend();
        copied
    }
}

#[async_trait::async_trait]
impl PreparedExecutor for TransferExecutor {
    async fn execute(
        mut self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<ToolCompletion, ToolError> {
        if context.session != self.session {
            return Err(ToolError::operation_changed("session changed"));
        }
        context.detachable(None);
        let from = match &self.source {
            SourceEnd::Machine(end) => end.shown(),
            SourceEnd::Artifact { digest, .. } => format!("artifact {}", short(digest)),
        };
        let to = self.destination.shown();
        let folder_limited = self.limit.is_some()
            && matches!(&self.source, SourceEnd::Machine(end) if end.prepared.kind == Some(FsKind::Folder));
        let mut moved = Moved::default();
        let result = if self.same_runtime && !folder_limited {
            self.copy(&context).await
        } else {
            let cancellation = context.cancellation.clone();
            tokio::select! {
                relayed = self.relay(&context, &mut moved) => relayed,
                () = cancellation.cancelled() => Err("the transfer was stopped".into()),
            }
        };
        let written = result.map_err(|error| {
            ToolError::execution(if moved.bytes > 0 {
                format!(
                    "transferring {from} to {to} failed after {}: {error}",
                    human_bytes(moved.bytes)
                )
            } else {
                format!("transferring {from} to {to} failed: {error}")
            })
        })?;
        let mut output = if written.files == 1
            && !matches!(&self.source, SourceEnd::Machine(end) if end.prepared.kind == Some(FsKind::Folder))
        {
            let size = if written.bytes < 1024 {
                format!("{} bytes", written.bytes)
            } else {
                format!("{} ({} bytes)", human_bytes(written.bytes), written.bytes)
            };
            format!(
                "Copied {from} to {} on {}: {size}, SHA-256 {}.",
                written.path, self.destination.machine, written.sha256
            )
        } else {
            format!(
                "Copied the folder {from} to {} on {}: {} file{}, {}.",
                written.path,
                self.destination.machine,
                written.files,
                if written.files == 1 { "" } else { "s" },
                human_bytes(written.bytes)
            )
        };
        if written.skipped > 0 {
            output.push_str(&format!(
                " Left out {} entr{}: devices, sockets, FIFOs, or links that lead outside the folder.",
                written.skipped,
                if written.skipped == 1 { "y" } else { "ies" }
            ));
        }
        Ok(ToolCompletion::single(ToolResult {
            title: safe_title(format!("Transferred {from} → {to}")),
            display: crate::base_tools::markdown_display(&output),
            output,
            background: None,
            retained_output: None,
            metadata: serde_json::json!({
                "path": written.path,
                "machine": self.destination.machine,
                "bytes": written.bytes,
                "files": written.files,
                "sha256": written.sha256,
                "skipped": written.skipped,
            }),
            attachments: Vec::new(),
            additional_messages: Vec::new(),
            loaded_tools: Vec::new(),
        }))
    }
}
