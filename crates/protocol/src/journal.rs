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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegateRequestPayloadV2 {
    pub description: String,
    pub prompt: String,
    pub title: SessionTitle,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DelegatedContextRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegatedContextTurn {
    pub role: DelegatedContextRole,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegateRequestPayloadV3 {
    pub description: String,
    pub prompt: String,
    pub title: SessionTitle,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SessionId>", required)]
    pub resume_session_id: Option<SessionId>,
    pub inherit_context: bool,
    #[schemars(length(max = 65536))]
    pub seeded_context: Vec<DelegatedContextTurn>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegateRequestPayloadV4 {
    pub description: String,
    pub prompt: String,
    pub title: SessionTitle,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SessionId>", required)]
    pub resume_session_id: Option<SessionId>,
    pub inherit_context: bool,
    #[schemars(length(max = 65536))]
    pub seeded_context: Vec<DelegatedContextTurn>,
    pub background: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum StagedSkillProvenance {
    SkillFork,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct StagedSkillPayload {
    pub provenance: StagedSkillProvenance,
    pub name: String,
    pub args: String,
    pub rendered_body: String,
    pub source_path: String,
    pub base_dir: String,
    #[schemars(length(max = 10))]
    pub supporting_files: Vec<String>,
    #[schemars(length(max = 256))]
    pub grants: Vec<PermissionRule>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ModelKey>", required)]
    #[ts(type = "ModelKey | null")]
    pub model: Option<ModelKey>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DelegateRequestPayloadV5 {
    pub description: String,
    pub prompt: String,
    pub title: SessionTitle,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SessionId>", required)]
    pub resume_session_id: Option<SessionId>,
    pub inherit_context: bool,
    #[schemars(length(max = 65536))]
    pub seeded_context: Vec<DelegatedContextTurn>,
    pub background: bool,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<StagedSkillPayload>", required)]
    pub staged_skill: Option<StagedSkillPayload>,
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
    DelegationStartedV2 {
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
        prompt: String,
        request: DelegateRequestPayloadV2,
    },
    DelegationStartedV3 {
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
        prompt: String,
        request: DelegateRequestPayloadV3,
    },
    DelegationStartedV4 {
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
        prompt: String,
        request: DelegateRequestPayloadV4,
    },
    DelegationStartedV5 {
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
        prompt: String,
        request: DelegateRequestPayloadV5,
    },
    DelegationLinked {
        invocation_id: InvocationId,
    },
    DelegationRunStarted {
        invocation_id: InvocationId,
        child_run_id: RunId,
    },
    DelegationRunAttached {
        invocation_id: InvocationId,
        child_run_id: RunId,
    },
    DelegationTerminated {
        invocation_id: InvocationId,
        status: SessionStatus,
    },
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct StoredDelegationJournalRecord {
    pub delegation_journal_schema_version: DelegationJournalSchemaVersion,
    pub record: DelegationJournalRecord,
}
