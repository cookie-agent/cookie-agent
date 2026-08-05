use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::*;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegationReservation {
    pub invocation_id: InvocationId,
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    pub parent_tool_call_id: ToolCallId,
    pub child_session_id: SessionId,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegateRequestPayload {
    pub task: String,
    #[schemars(length(max = 256))]
    pub context: Vec<Value>,
    #[schemars(length(max = 256))]
    pub success_criteria: Vec<String>,
    pub expected_output: Value,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
pub enum DelegationJournalRecord {
    DelegationStarted {
        reservation: DelegationReservation,
        child_agent: Box<AgentSnapshot>,
        #[ts(type = "ModelSnapshotRevision")]
        manifest_revision: ModelSnapshotRevision,
        #[ts(type = "RuntimeRevision")]
        runtime_revision: RuntimeRevision,
        #[ts(type = "CatalogRevision")]
        catalog_revision: CatalogRevision,
        #[ts(type = "ProviderStateRevision")]
        provider_state_revision: ProviderStateRevision,
        #[ts(type = "ModelRevision")]
        model_revision: ModelRevision,
        #[ts(type = "AgentRevision")]
        agent_revision: AgentRevision,
        #[ts(type = "RecipeRegistryRevision")]
        recipe_registry_revision: RecipeRegistryRevision,
        #[schemars(length(min = 1, max = 256))]
        selected_suffix: Vec<FrozenModelBinding>,
        request_fingerprint: Sha256Digest,
        task: String,
        request: DelegateRequestPayload,
    },
    DelegationLinked {
        invocation_id: InvocationId,
    },
    DelegationRunStarted {
        invocation_id: InvocationId,
        child_run_id: RunId,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct StoredDelegationJournalRecord {
    pub delegation_journal_schema_version: DelegationJournalSchemaVersion,
    pub record: DelegationJournalRecord,
}
