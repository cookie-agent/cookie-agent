use std::{fmt, fs, path::Path};

use schemars::schema_for;

use crate::*;

#[derive(Debug)]
pub enum BindingExportError {
    Arguments(String),
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for BindingExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(error) => {
                write!(formatter, "invalid binding generator arguments: {error}")
            }
            Self::Io(error) => write!(formatter, "binding output failed: {error}"),
            Self::Json(error) => write!(formatter, "schema encoding failed: {error}"),
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
macro_rules! protocol_roots {
    ($macro:ident) => {
        $macro!(
            ClientHello,
            ServerHello,
            Request,
            Response,
            Notification,
            ExtensionInitializeParams,
            ExtensionInitializeResult,
            ExtensionProducerRegisterParams,
            ExtensionProducerRegisterResult,
            ExtensionProducerSendParams,
            ExtensionProducerSendResult,
            ExtensionProducerUnregisterParams,
            ExtensionProducerUnregisterResult,
            ExtensionProducerDiscardParams,
            ExtensionProducerDiscardResult,
            ExtensionRecoveryStartParams,
            ExtensionRecoveryCompleteParams,
            ExtensionRecoveryCompleteResult,
            ExtensionPingParams,
            ExtensionPingResult,
            ExtensionShutdownParams,
            ExtensionToolCallParams,
            ExtensionToolCallResult,
            ExtensionEventParams,
            ExtensionBusEventParams,
            ExtensionEmitParams,
            ExtensionEmitResultParams,
            ExtensionToolBeforeCallParams,
            ExtensionToolBeforeCallResult,
            ExtensionToolAfterResultParams,
            ExtensionToolAfterResultResult,
            ExtensionAgentBeforeStartParams,
            ExtensionAgentBeforeStartResult,
            ExtensionSessionBeforeCompactParams,
            ExtensionSessionBeforeCompactResult,
            ExtensionUserBeforeInputParams,
            ExtensionUserBeforeInputResult,
            ExtensionModelBeforeRequestParams,
            ExtensionModelBeforeRequestResult,
            ExtensionProviderBeforeHeadersParams,
            ExtensionProviderBeforeHeadersResult,
            ExtensionProviderBeforeRequestParams,
            ExtensionProviderBeforeRequestResult,
            ExtensionProviderAfterResponseParams,
            ExtensionProviderAfterResponseResult,
            ExtensionMessageEndParams,
            ExtensionMessageEndResult,
            ExtensionModelBeforeSelectParams,
            ExtensionAllowBlockResult,
            ExtensionSessionBeforeForkParams,
            ExtensionSessionBeforeRevertParams,
            ExtensionSessionBeforeRevertResult,
            SessionMeta,
            StoredEvent,
            EventPayload,
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
            SessionGoalGetParams,
            SessionGoalGetResult,
            SessionGoalSetParams,
            SessionGoalSetResult,
            SessionGoalLifecycleParams,
            SessionGoalLifecycleResult,
            SessionProducersParams,
            SessionProducersResult,
            GoalGetParams,
            GoalGetResult,
            GoalUpdateParams,
            GoalUpdateResult,
            SessionUsageParams,
            SessionUsageResult,
            SessionTreeUsageResult,
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
            SkillsListParams,
            SkillsListResult,
            SkillsGetParams,
            SkillsGetResult,
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
            ModelSnapshotManifestV1
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
            "schemas": filenames,
        }))?,
    )?;
    Ok(())
}
