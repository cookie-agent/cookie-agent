use std::{fmt, fs, path::Path};

use schemars::schema_for;
use ts_rs::{Config, TS};

use crate::*;

#[derive(Debug)]
pub enum BindingExportError {
    Arguments(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    TypeScript(ts_rs::ExportError),
}

impl fmt::Display for BindingExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(error) => {
                write!(formatter, "invalid binding generator arguments: {error}")
            }
            Self::Io(error) => write!(formatter, "binding output failed: {error}"),
            Self::Json(error) => write!(formatter, "schema encoding failed: {error}"),
            Self::TypeScript(error) => write!(formatter, "TypeScript export failed: {error}"),
        }
    }
}

impl std::error::Error for BindingExportError {}

impl From<std::io::Error> for BindingExportError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<serde_json::Error> for BindingExportError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}
impl From<ts_rs::ExportError> for BindingExportError {
    fn from(value: ts_rs::ExportError) -> Self {
        Self::TypeScript(value)
    }
}

macro_rules! protocol_roots {
    ($macro:ident) => {
        $macro!(
            ClientHello,
            ServerHello,
            Request,
            Response,
            Notification,
            SessionMeta,
            StoredEvent,
            EventSubscriptionMessage,
            OutputDelta,
            OutputGap,
            OutputSnapshotEnvelope,
            SessionCreateParams,
            SessionCreateResult,
            SessionListParams,
            SessionListResult,
            SessionGetParams,
            SessionGetResult,
            SessionUsageParams,
            SessionUsageResult,
            AgentUsageParams,
            AgentUsageResult,
            GlobalUsageParams,
            GlobalUsageResult,
            ModelUsageRollup,
            SessionChildrenParams,
            SessionChildrenResult,
            SessionTreeParams,
            SessionTreeResult,
            SessionResumeParams,
            SessionResumeResult,
            SessionSetPermissionModeParams,
            SessionSetPermissionModeResult,
            SessionPermissionGetParams,
            SessionPermissionGetResult,
            SessionPermissionSetParams,
            SessionPermissionClearParams,
            SessionPermissionMutationResult,
            SessionCompactParams,
            SessionCompactResult,
            SessionRevertParams,
            SessionRevertResult,
            SessionForkParams,
            SessionForkResult,
            RunStartParams,
            RunStartResult,
            RunStartConflict,
            RunSteerParams,
            RunSteerResult,
            RunRecallSteerParams,
            RunRecallSteerResult,
            RunCancelParams,
            RunCancelResult,
            RunToolStdinParams,
            RunToolStdinResult,
            SessionRenameParams,
            SessionRenameResult,
            SessionRenameError,
            EventsSubscribeParams,
            EventsSubscribeResult,
            ApprovalRespondParams,
            ApprovalRespondResult,
            ApprovalRespondError,
            ApprovalListParams,
            ApprovalListResult,
            McpAuthBeginParams,
            McpAuthBeginResult,
            McpAuthCancelParams,
            McpAuthCancelResult,
            McpServerListParams,
            McpServerListResult,
            McpServerAddParams,
            McpServerEditParams,
            McpServerNameParams,
            McpServerSetEnabledParams,
            McpServerPersistParams,
            McpServerMutationResult,
            RuntimeSnapshotGetParams,
            RuntimeSnapshotResult,
            RuntimeChangedNotification,
            ProviderConnectResult,
            ProviderConnectError,
            ProviderDisconnectParams,
            ProviderDisconnectResult,
            ProviderDisconnectError,
            ModelSnapshotManifestV1,
            StoredDelegationJournalRecord
        )
    };
}

#[must_use]
pub fn json_schema_documents() -> Vec<(&'static str, schemars::Schema)> {
    macro_rules! collect {
        ($($type:ty),+ $(,)?) => {{
            vec![$((concat!(stringify!($type), ".schema.json"), schema_for!($type))),+]
        }};
    }
    protocol_roots!(collect)
}

pub fn export_json_schema_set(output: &Path) -> Result<(), BindingExportError> {
    if output.exists() {
        fs::remove_dir_all(output)?;
    }
    fs::create_dir_all(output)?;
    let documents = json_schema_documents();
    let filenames = documents
        .iter()
        .map(|(filename, _)| *filename)
        .collect::<Vec<_>>();
    for (filename, schema) in documents {
        let bytes = serde_json::to_vec_pretty(&schema)?;
        fs::write(output.join(filename), bytes)?;
    }
    fs::write(
        output.join("manifest.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "event_schema_version": EVENT_SCHEMA_VERSION,
            "session_jsonl_schema_version": EVENT_SCHEMA_VERSION,
            "session_meta_schema_version": SESSION_META_SCHEMA_VERSION,
            "delegation_journal_schema_version": DELEGATION_JOURNAL_SCHEMA_VERSION,
            "schemas": filenames,
        }))?,
    )?;
    Ok(())
}

pub fn export_typescript_binding_set(output: &Path) -> Result<(), BindingExportError> {
    if output.exists() {
        fs::remove_dir_all(output)?;
    }
    fs::create_dir_all(output)?;
    let config = Config::new()
        .with_out_dir(output)
        .with_large_int("number")
        .with_import_extension(Some("js"));
    macro_rules! export {
        ($($type:ty),+ $(,)?) => {{
            $(<$type>::export_all(&config)?;)+
        }};
    }
    protocol_roots!(export);
    fs::write(output.join("globals.d.ts"), TYPESCRIPT_GLOBALS)?;
    fs::write(output.join("index.ts"), typescript_index(output)?)?;
    fs::write(
        output.join("compile-fixture.ts"),
        TYPESCRIPT_COMPILE_FIXTURE,
    )?;
    fs::write(output.join("tsconfig.json"), TYPESCRIPT_CONFIG)?;
    Ok(())
}

fn typescript_index(output: &Path) -> Result<String, std::io::Error> {
    let mut modules = fs::read_dir(output)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            (name.ends_with(".ts") && !name.ends_with(".d.ts"))
                .then(|| name.trim_end_matches(".ts").to_owned())
        })
        .collect::<Vec<_>>();
    modules.sort();
    modules.dedup();
    let exports = modules
        .into_iter()
        .map(|module| format!("export * from \"./{module}.js\";\n"))
        .collect::<String>();
    Ok(format!(
        "/// <reference path=\"./globals.d.ts\" />\n{exports}"
    ))
}

pub const TYPESCRIPT_GLOBALS: &str = r#"declare global {
  type AgentId = string;
  type ProviderId = string;
  type ProviderModelId = string;
  type ModelKey = string;
  type VariantId = string;
  type SetupFieldId = string;
  type AuthFieldName = string;
  type AuthMethodId = string;
  type ProviderRecipeId = string;
  type ProtocolRecipeId = string;
  type ProviderSetupRecipeId = string;
  type RecipeCompilerVersion = string;
  type CatalogRevision = string;
  type RecipeRegistryRevision = string;
  type ProviderStoreRevision = string;
  type ProviderStateRevision = string;
  type ModelRevision = string;
  type AgentRevision = string;
  type RuntimeRevision = string;
  type ModelSnapshotRevision = string;
  type ModelSelection = { model: ModelKey; variant: VariantId | null };
  type LanguageModelDescriptor = {
    identity: { provider_id: string; model_id: string };
    adapter_id: string;
    capabilities: {
      features: Array<"tool_calling" | "parallel_tools" | "tool_input_deltas" | "reasoning" | "structured_output" | "temperature" | "top_p" | "max_output_tokens" | "prompt_caching" | "usage" | "provider_tools" | "sources">;
      limits: { context: number | null; input: number | null; output: number | null };
      modalities: { input: Array<string>; output: Array<string> };
      media: { input: Record<string, { media_types: Array<string>; sources: Array<"inline_bytes" | "inline_text" | "url" | "provider_reference"> }> };
      cancellation: "local_only" | "remote_best_effort" | "unsupported";
      compaction: "unsupported";
      replay: { policy: "never" | "if_valid" | "always"; capability: "required" | "optional" | "unsupported"; reasoning: boolean };
    };
    provider_metadata: Record<string, unknown>;
  };
}
export {};
"#;

pub const TYPESCRIPT_COMPILE_FIXTURE: &str = r#"/// <reference path="./globals.d.ts" />
import type {
  ClientHello,
  ModelSnapshotManifestV1,
  ProviderConnectResult,
  ProviderDisconnectParams,
  Request,
  RuntimeChangedNotification,
  RuntimeSnapshotResult,
  RunStartParams,
  SessionCreateParams,
  StoredEvent,
} from "./index.js";

const roots: [
  ClientHello,
  Request,
  SessionCreateParams,
  RunStartParams,
  StoredEvent,
  RuntimeSnapshotResult,
  RuntimeChangedNotification,
  ProviderConnectResult,
  ProviderDisconnectParams,
  ModelSnapshotManifestV1,
] | null = null;

export { roots };
"#;

pub const TYPESCRIPT_CONFIG: &str = r#"{
  "compilerOptions": {
    "strict": true,
    "noEmit": true,
    "target": "ES2022",
    "module": "NodeNext",
    "moduleResolution": "NodeNext",
    "skipLibCheck": false
  },
  "include": ["./**/*.ts", "./globals.d.ts"]
}
"#;
