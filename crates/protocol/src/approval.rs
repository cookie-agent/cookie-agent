use std::{borrow::Cow, fmt};

use jiff::Timestamp;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use crate::{
    ApprovalId, PermissionAction, PermissionEffect, RuntimeId, SafeCode, SafeErrorMessage,
    SessionId, Sha256Digest, ToolCallId, TreeApprovalGrantId, WildcardPattern,
};

#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct OperationFingerprint {
    digest: Sha256Digest,
}
impl OperationFingerprint {
    const DOMAIN: &'static [u8] = b"cookie-agent.operation-fingerprint.v7\0";
    #[must_use]
    pub fn from_prepared_operation(operation: &PreparedOperationIdentity) -> Self {
        Self {
            digest: Sha256Digest::hash_domain_separated(
                Self::DOMAIN,
                &operation.canonical_fingerprint_bytes(),
            ),
        }
    }
    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}
impl<'de> Deserialize<'de> for OperationFingerprint {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            digest: Sha256Digest,
        }
        Ok(Self {
            digest: Wire::deserialize(d)?.digest,
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct PreparedResourceDigest {
    digest: Sha256Digest,
}
impl PreparedResourceDigest {
    const DOMAIN: &'static [u8] = b"cookie-agent.prepared-resource-digest.v7\0";
    #[must_use]
    pub fn from_canonical_binding_bytes(bytes: &[u8]) -> Self {
        Self {
            digest: Sha256Digest::hash_domain_separated(Self::DOMAIN, bytes),
        }
    }
    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}
impl<'de> Deserialize<'de> for PreparedResourceDigest {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            digest: Sha256Digest,
        }
        Ok(Self {
            digest: Wire::deserialize(d)?.digest,
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PreparedResourceIdentity(String);
impl PreparedResourceIdentity {
    pub const MAX_BYTES: usize = 512;
    pub fn new(value: impl Into<String>) -> Result<Self, ApprovalSchemaError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > Self::MAX_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
            || value.starts_with("fd:")
            || value.starts_with("tmp:")
            || value.starts_with("temp:")
        {
            return Err(ApprovalSchemaError::InvalidPreparedIdentity);
        }
        Ok(Self(value))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for PreparedResourceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Serialize for PreparedResourceIdentity {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for PreparedResourceIdentity {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for PreparedResourceIdentity {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PreparedResourceIdentity")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":512,"pattern":"^[a-z0-9][a-z0-9._:-]*$"})
    }
}

pub type PreparedCapabilityOperation = PreparedResourceIdentity;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalBoundary {
    Exact,
    CommandPrefix { prefix: String },
    DelegationTree { root_session_id: SessionId },
}

/// A value this build does not know reads as `primary_operation`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ApprovalResourceSource {
    SecondaryOperation,
    ModelRequest,
    DoomLoopGuard,
    #[serde(other)]
    PrimaryOperation,
}
/// A value this build does not know reads as `process_local`: a binding that is
/// never restart-stable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum PreparedBindingLifetime {
    RestartStable,
    #[serde(other)]
    ProcessLocal,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PreparedApprovalResource {
    pub capability: PermissionAction,
    pub canonical: PreparedResourceIdentity,
    pub binding_digest: PreparedResourceDigest,
    pub binding_lifetime: PreparedBindingLifetime,
    pub boundary: ApprovalBoundary,
    pub source: ApprovalResourceSource,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MatchedPermissionRule {
    pub source_layer: SafeCode,
    pub action: PermissionAction,
    /// The rule's runtime selector, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeId>,
    pub resource: WildcardPattern,
    pub effect: PermissionEffect,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DecisionTrace {
    pub action: PermissionAction,
    pub normalized_resource: String,
    pub candidates: Vec<MatchedPermissionRule>,
    pub effect: PermissionEffect,
    pub precedence_reason: String,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ApprovalEvaluation {
    pub resource_digest: PreparedResourceDigest,
    pub effect: PermissionEffect,
    pub trace: DecisionTrace,
}

/// A value this build does not know reads as `permission_policy`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ApprovalTrigger {
    ModelToolApproval,
    InternalAgent,
    DoomLoop,
    #[serde(other)]
    PermissionPolicy,
}

/// A value this build does not know reads as `ask`: asking.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum PermissionMode {
    #[default]
    AutoApprove,
    AutoApproveN,
    AutoApproveY,
    Yolo,
    #[serde(other)]
    Ask,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ApprovalCapability {
    pub action: PermissionAction,
    pub operation: PreparedCapabilityOperation,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparedCapabilityLifetime {
    ProcessLocal,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PreparedOperationIdentity {
    normalized_arguments_digest: Sha256Digest,
    #[schemars(length(min = 1, max = 256))]
    capabilities: Vec<ApprovalCapability>,
    #[schemars(length(min = 1, max = 256))]
    resources: Vec<PreparedApprovalResource>,
    execution_context_digest: Sha256Digest,
    capability_lifetime: PreparedCapabilityLifetime,
}
impl PreparedOperationIdentity {
    pub fn new(
        normalized_arguments_digest: Sha256Digest,
        capabilities: Vec<ApprovalCapability>,
        resources: Vec<PreparedApprovalResource>,
        execution_context_digest: Sha256Digest,
    ) -> Result<Self, ApprovalSchemaError> {
        if capabilities.is_empty() || capabilities.len() > 256 {
            return Err(ApprovalSchemaError::MissingCapability);
        }
        if resources.is_empty() {
            return Err(ApprovalSchemaError::MissingResource);
        }
        if resources.len() > 256 {
            return Err(ApprovalSchemaError::TooManyResources);
        }
        if resources.iter().any(|resource| {
            !capabilities
                .iter()
                .any(|capability| capability.action == resource.capability)
        }) {
            return Err(ApprovalSchemaError::UnboundResourceCapability);
        }
        let mut digests = resources
            .iter()
            .map(|resource| resource.binding_digest.digest().as_str())
            .collect::<Vec<_>>();
        digests.sort_unstable();
        if digests.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ApprovalSchemaError::DuplicateResourceBinding);
        }
        Ok(Self {
            normalized_arguments_digest,
            capabilities,
            resources,
            execution_context_digest,
            capability_lifetime: PreparedCapabilityLifetime::ProcessLocal,
        })
    }
    #[must_use]
    pub fn capabilities(&self) -> &[ApprovalCapability] {
        &self.capabilities
    }
    #[must_use]
    pub fn resources(&self) -> &[PreparedApprovalResource] {
        &self.resources
    }
    fn canonical_fingerprint_bytes(&self) -> Vec<u8> {
        let mut value =
            serde_json::to_value(self).expect("protocol identity serialization cannot fail");
        if let Some(resources) = value
            .get_mut("resources")
            .and_then(serde_json::Value::as_array_mut)
        {
            for resource in &mut *resources {
                if let Some(object) = resource.as_object_mut() {
                    object.remove("boundary");
                    object.remove("source");
                }
            }
            resources.sort_by_key(serde_json::Value::to_string);
        }
        if let Some(capabilities) = value
            .get_mut("capabilities")
            .and_then(serde_json::Value::as_array_mut)
        {
            capabilities.sort_by_key(serde_json::Value::to_string);
        }
        serde_json::to_vec(&value).expect("protocol identity serialization cannot fail")
    }
}
impl<'de> Deserialize<'de> for PreparedOperationIdentity {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            normalized_arguments_digest: Sha256Digest,
            capabilities: Vec<ApprovalCapability>,
            resources: Vec<PreparedApprovalResource>,
            execution_context_digest: Sha256Digest,
            capability_lifetime: PreparedCapabilityLifetime,
        }
        let w = Wire::deserialize(d)?;
        let value = Self::new(
            w.normalized_arguments_digest,
            w.capabilities,
            w.resources,
            w.execution_context_digest,
        )
        .map_err(serde::de::Error::custom)?;
        if w.capability_lifetime != PreparedCapabilityLifetime::ProcessLocal {
            return Err(serde::de::Error::custom(
                "prepared capability lifetime must be process_local",
            ));
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ApprovalConstraints {
    pub allow_once: bool,
    pub allow_tree_grant: bool,
    pub cancellable: bool,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<Timestamp>", required)]
    pub expires_at: Option<Timestamp>,
}

/// A value this build does not know reads as `system_error`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ApprovalReasonCode {
    PolicyAllowed,
    PolicyDenied,
    PolicyRequiresApproval,
    ModelRequested,
    InternalAgentAllowed,
    InternalAgentDenied,
    AutoApproveNRejected,
    AutoApproveYApproved,
    YoloApproved,
    TreeGrantMatched,
    UserApprovedOnce,
    UserApprovedTree,
    UserRejected,
    UserCancelled,
    RequestCancelled,
    DoomLoopDetected,
    ConstraintViolation,
    OperationChanged,
    PreparedCapabilityLost,
    UnsupportedPlatform,
    ApprovalExpired,
    Escalated,
    Unattended,
    #[serde(other)]
    SystemError,
}
/// A value this build does not know reads as `system`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ApprovalDecisionSource {
    Policy,
    Model,
    InternalAgent,
    PermissionMode,
    TreeGrant,
    User,
    DoomLoopGuard,
    #[serde(other)]
    System,
}
/// A value this build does not know reads as `cancelled`: a cancelled approval,
/// which takes no answer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ApprovalStatus {
    Pending,
    Escalated,
    Approved,
    Rejected,
    Expired,
    #[serde(other)]
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ApprovalFeedback {
    pub message: SafeErrorMessage,
}
/// A value this build does not know reads as `ask`: asking the user.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ApprovalInternalDecisionKind {
    Allow,
    Deny,
    Escalate,
    #[serde(other)]
    Ask,
}
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ApprovalInternalDecision {
    pub decision: ApprovalInternalDecisionKind,
    pub source: ApprovalDecisionSource,
    pub reason_code: ApprovalReasonCode,
    #[schemars(length(min = 1, max = 256))]
    pub evaluations: Vec<ApprovalEvaluation>,
}
impl ApprovalInternalDecision {
    pub fn validate(&self) -> Result<(), ApprovalSchemaError> {
        if self.evaluations.is_empty() {
            return Err(ApprovalSchemaError::MissingEvaluation);
        }
        if self.evaluations.len() > 256 {
            return Err(ApprovalSchemaError::TooManyEvaluations);
        }
        let coherent = match self.decision {
            ApprovalInternalDecisionKind::Allow => matches!(
                self.reason_code,
                ApprovalReasonCode::PolicyAllowed
                    | ApprovalReasonCode::InternalAgentAllowed
                    | ApprovalReasonCode::YoloApproved
                    | ApprovalReasonCode::TreeGrantMatched
            ),
            ApprovalInternalDecisionKind::Deny => matches!(
                self.reason_code,
                ApprovalReasonCode::PolicyDenied
                    | ApprovalReasonCode::InternalAgentDenied
                    | ApprovalReasonCode::DoomLoopDetected
                    | ApprovalReasonCode::ConstraintViolation
                    | ApprovalReasonCode::OperationChanged
                    | ApprovalReasonCode::PreparedCapabilityLost
                    | ApprovalReasonCode::UnsupportedPlatform
                    | ApprovalReasonCode::Unattended
                    | ApprovalReasonCode::SystemError
            ),
            ApprovalInternalDecisionKind::Ask => matches!(
                self.reason_code,
                ApprovalReasonCode::PolicyRequiresApproval | ApprovalReasonCode::ModelRequested
            ),
            ApprovalInternalDecisionKind::Escalate => {
                self.reason_code == ApprovalReasonCode::Escalated
            }
        };
        let source_coherent = match self.reason_code {
            ApprovalReasonCode::PolicyAllowed
            | ApprovalReasonCode::PolicyDenied
            | ApprovalReasonCode::PolicyRequiresApproval
            | ApprovalReasonCode::YoloApproved => self.source == ApprovalDecisionSource::Policy,
            ApprovalReasonCode::ModelRequested => self.source == ApprovalDecisionSource::Model,
            ApprovalReasonCode::InternalAgentAllowed | ApprovalReasonCode::InternalAgentDenied => {
                self.source == ApprovalDecisionSource::InternalAgent
            }
            ApprovalReasonCode::TreeGrantMatched => {
                self.source == ApprovalDecisionSource::TreeGrant
            }
            ApprovalReasonCode::DoomLoopDetected => {
                self.source == ApprovalDecisionSource::DoomLoopGuard
            }
            ApprovalReasonCode::Escalated => !matches!(
                self.source,
                ApprovalDecisionSource::TreeGrant
                    | ApprovalDecisionSource::User
                    | ApprovalDecisionSource::PermissionMode
                    | ApprovalDecisionSource::DoomLoopGuard
            ),
            _ => self.source == ApprovalDecisionSource::System,
        };
        if coherent && source_coherent {
            Ok(())
        } else {
            Err(ApprovalSchemaError::ContradictoryDecision)
        }
    }
}
impl<'de> Deserialize<'de> for ApprovalInternalDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            decision: ApprovalInternalDecisionKind,
            source: ApprovalDecisionSource,
            reason_code: ApprovalReasonCode,
            evaluations: Vec<ApprovalEvaluation>,
        }
        let raw = serde_json::Value::deserialize(deserializer)?;
        let wire = Wire::deserialize(&raw).map_err(serde::de::Error::custom)?;
        let value = Self {
            decision: wire.decision,
            source: wire.source,
            reason_code: wire.reason_code,
            evaluations: wire.evaluations,
        };
        if reads_as_known(&raw, &value, DECISION_MEMBERS) {
            value.validate().map_err(serde::de::Error::custom)?;
        }
        Ok(value)
    }
}
/// A value this build does not know reads as `cancel`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ApprovalUserDecision {
    ApproveOnce,
    ApproveTree,
    Reject,
    #[serde(other)]
    Cancel,
}
/// A value this build does not know reads as `cancelled`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ApprovalFinalOutcome {
    Approved,
    Rejected,
    Expired,
    #[serde(other)]
    Cancelled,
}
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ApprovalFinalDecision {
    pub outcome: ApprovalFinalOutcome,
    pub source: ApprovalDecisionSource,
    pub reason_code: ApprovalReasonCode,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ApprovalFeedback>", required)]
    pub feedback: Option<ApprovalFeedback>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<TreeApprovalGrantId>", required)]
    pub tree_grant_id: Option<TreeApprovalGrantId>,
}
impl ApprovalFinalDecision {
    pub fn validate(&self) -> Result<(), ApprovalSchemaError> {
        let outcome_coherent = match self.outcome {
            ApprovalFinalOutcome::Approved => matches!(
                self.reason_code,
                ApprovalReasonCode::PolicyAllowed
                    | ApprovalReasonCode::InternalAgentAllowed
                    | ApprovalReasonCode::AutoApproveYApproved
                    | ApprovalReasonCode::YoloApproved
                    | ApprovalReasonCode::TreeGrantMatched
                    | ApprovalReasonCode::UserApprovedOnce
                    | ApprovalReasonCode::UserApprovedTree
            ),
            ApprovalFinalOutcome::Rejected => matches!(
                self.reason_code,
                ApprovalReasonCode::PolicyDenied
                    | ApprovalReasonCode::InternalAgentDenied
                    | ApprovalReasonCode::AutoApproveNRejected
                    | ApprovalReasonCode::UserRejected
                    | ApprovalReasonCode::DoomLoopDetected
                    | ApprovalReasonCode::ConstraintViolation
                    | ApprovalReasonCode::UnsupportedPlatform
                    | ApprovalReasonCode::Unattended
                    | ApprovalReasonCode::SystemError
            ),
            ApprovalFinalOutcome::Cancelled => matches!(
                self.reason_code,
                ApprovalReasonCode::UserCancelled
                    | ApprovalReasonCode::RequestCancelled
                    | ApprovalReasonCode::OperationChanged
                    | ApprovalReasonCode::PreparedCapabilityLost
            ),
            ApprovalFinalOutcome::Expired => {
                self.reason_code == ApprovalReasonCode::ApprovalExpired
            }
        };
        let source_coherent = match self.reason_code {
            ApprovalReasonCode::UserApprovedOnce
            | ApprovalReasonCode::UserApprovedTree
            | ApprovalReasonCode::UserRejected
            | ApprovalReasonCode::UserCancelled => self.source == ApprovalDecisionSource::User,
            ApprovalReasonCode::PolicyAllowed
            | ApprovalReasonCode::PolicyDenied
            | ApprovalReasonCode::YoloApproved => self.source == ApprovalDecisionSource::Policy,
            ApprovalReasonCode::InternalAgentAllowed | ApprovalReasonCode::InternalAgentDenied => {
                self.source == ApprovalDecisionSource::InternalAgent
            }
            ApprovalReasonCode::AutoApproveNRejected | ApprovalReasonCode::AutoApproveYApproved => {
                self.source == ApprovalDecisionSource::PermissionMode
            }
            ApprovalReasonCode::TreeGrantMatched => {
                self.source == ApprovalDecisionSource::TreeGrant
            }
            ApprovalReasonCode::DoomLoopDetected => {
                self.source == ApprovalDecisionSource::DoomLoopGuard
            }
            _ => self.source == ApprovalDecisionSource::System,
        };
        let grant_coherent = self.tree_grant_id.is_some()
            == matches!(
                self.reason_code,
                ApprovalReasonCode::UserApprovedTree | ApprovalReasonCode::TreeGrantMatched
            );
        if outcome_coherent && source_coherent && grant_coherent {
            Ok(())
        } else {
            Err(ApprovalSchemaError::ContradictoryDecision)
        }
    }
}
impl<'de> Deserialize<'de> for ApprovalFinalDecision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            outcome: ApprovalFinalOutcome,
            source: ApprovalDecisionSource,
            reason_code: ApprovalReasonCode,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            feedback: Option<ApprovalFeedback>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            tree_grant_id: Option<TreeApprovalGrantId>,
        }
        let raw = serde_json::Value::deserialize(deserializer)?;
        let wire = Wire::deserialize(&raw).map_err(serde::de::Error::custom)?;
        let value = Self {
            outcome: wire.outcome,
            source: wire.source,
            reason_code: wire.reason_code,
            feedback: wire.feedback,
            tree_grant_id: wire.tree_grant_id,
        };
        if reads_as_known(&raw, &value, FINAL_DECISION_MEMBERS) {
            value.validate().map_err(serde::de::Error::custom)?;
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ApprovalRequest {
    approval_id: ApprovalId,
    #[schemars(range(min = 1))]
    revision: u64,
    /// The tool call the request is for, whose `tool_call_started` carries
    /// the call's presentation.
    tool_call_id: ToolCallId,
    trigger: ApprovalTrigger,
    operation_fingerprint: OperationFingerprint,
    operation: PreparedOperationIdentity,
    #[schemars(length(min = 1, max = 256))]
    evaluations: Vec<ApprovalEvaluation>,
    constraints: ApprovalConstraints,
    /// Runtime ID of the tool the request is for; `null` for the base
    /// layer.
    #[schemars(with = "crate::NullableSchema<RuntimeId>", required)]
    runtime_id: Option<RuntimeId>,
}
impl ApprovalRequest {
    /// A request for `operation` of call `tool_call_id`, whose fingerprint
    /// it computes.
    pub fn new(
        approval_id: ApprovalId,
        revision: u64,
        tool_call_id: ToolCallId,
        trigger: ApprovalTrigger,
        operation: PreparedOperationIdentity,
        evaluations: Vec<ApprovalEvaluation>,
        constraints: ApprovalConstraints,
    ) -> Result<Self, ApprovalSchemaError> {
        let operation_fingerprint = OperationFingerprint::from_prepared_operation(&operation);
        Self::with_fingerprint(
            approval_id,
            revision,
            tool_call_id,
            trigger,
            operation_fingerprint,
            operation,
            evaluations,
            constraints,
        )
    }

    /// A request carrying an operation fingerprint computed when it was
    /// written, which a reader trusts rather than computing it again.
    #[allow(clippy::too_many_arguments)]
    fn with_fingerprint(
        approval_id: ApprovalId,
        revision: u64,
        tool_call_id: ToolCallId,
        trigger: ApprovalTrigger,
        operation_fingerprint: OperationFingerprint,
        operation: PreparedOperationIdentity,
        evaluations: Vec<ApprovalEvaluation>,
        constraints: ApprovalConstraints,
    ) -> Result<Self, ApprovalSchemaError> {
        if revision == 0 {
            return Err(ApprovalSchemaError::ZeroRevision);
        }
        if evaluations.is_empty() {
            return Err(ApprovalSchemaError::MissingEvaluation);
        }
        if evaluations.len() > 256 {
            return Err(ApprovalSchemaError::TooManyEvaluations);
        }
        let mut resources = operation
            .resources()
            .iter()
            .map(|resource| resource.binding_digest.digest().as_str())
            .collect::<Vec<_>>();
        resources.sort_unstable();
        let mut evaluated = evaluations
            .iter()
            .map(|evaluation| evaluation.resource_digest.digest().as_str())
            .collect::<Vec<_>>();
        evaluated.sort_unstable();
        if resources != evaluated {
            return Err(ApprovalSchemaError::IncompleteEvaluations);
        }
        if constraints.allow_tree_grant
            && operation.resources().iter().any(|resource| {
                resource.binding_lifetime == PreparedBindingLifetime::ProcessLocal
                    || resource.capability.is_path()
            })
        {
            return Err(ApprovalSchemaError::InvalidTreeGrantConstraint);
        }
        Ok(Self {
            approval_id,
            revision,
            tool_call_id,
            trigger,
            operation_fingerprint,
            operation,
            evaluations,
            constraints,
            runtime_id: None,
        })
    }
    /// The request for a tool of the runtime with ID `runtime_id` (`None`
    /// for the base layer).
    #[must_use]
    pub fn with_runtime(mut self, runtime_id: Option<RuntimeId>) -> Self {
        self.runtime_id = runtime_id;
        self
    }
    /// Runtime ID of the tool the request is for; `None` for the base layer.
    #[must_use]
    pub const fn runtime_id(&self) -> Option<&RuntimeId> {
        self.runtime_id.as_ref()
    }
    #[must_use]
    pub const fn approval_id(&self) -> ApprovalId {
        self.approval_id
    }
    /// The tool call the request is for.
    #[must_use]
    pub const fn tool_call_id(&self) -> ToolCallId {
        self.tool_call_id
    }
    #[must_use]
    pub const fn operation_fingerprint(&self) -> &OperationFingerprint {
        &self.operation_fingerprint
    }
    #[must_use]
    pub const fn operation(&self) -> &PreparedOperationIdentity {
        &self.operation
    }
    #[must_use]
    pub fn evaluations(&self) -> &[ApprovalEvaluation] {
        &self.evaluations
    }
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
    #[must_use]
    pub const fn constraints(&self) -> &ApprovalConstraints {
        &self.constraints
    }
}
impl<'de> Deserialize<'de> for ApprovalRequest {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            approval_id: ApprovalId,
            revision: u64,
            tool_call_id: ToolCallId,
            trigger: ApprovalTrigger,
            operation_fingerprint: OperationFingerprint,
            operation: PreparedOperationIdentity,
            evaluations: Vec<ApprovalEvaluation>,
            constraints: ApprovalConstraints,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            runtime_id: Option<RuntimeId>,
        }
        let w = Wire::deserialize(d)?;
        Ok(Self::with_fingerprint(
            w.approval_id,
            w.revision,
            w.tool_call_id,
            w.trigger,
            w.operation_fingerprint,
            w.operation,
            w.evaluations,
            w.constraints,
        )
        .map_err(serde::de::Error::custom)?
        .with_runtime(w.runtime_id))
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct TreeApprovalGrant {
    pub grant_id: TreeApprovalGrantId,
    pub root_session_id: SessionId,
    pub approval_id: ApprovalId,
    pub operation_fingerprint: OperationFingerprint,
    #[schemars(length(min = 1, max = 256))]
    pub capabilities: Vec<ApprovalCapability>,
    #[schemars(length(min = 1, max = 256))]
    pub resources: Vec<PreparedApprovalResource>,
    pub created_at: Timestamp,
    /// Runtime ID the grant applies to; `null` for base-layer tools.
    #[schemars(with = "crate::NullableSchema<RuntimeId>", required)]
    pub runtime_id: Option<RuntimeId>,
}
impl TreeApprovalGrant {
    pub fn validate(&self) -> Result<(), ApprovalSchemaError> {
        if self.capabilities.is_empty() || self.capabilities.len() > 256 {
            return Err(ApprovalSchemaError::MissingCapability);
        }
        if self.resources.is_empty() {
            return Err(ApprovalSchemaError::MissingResource);
        }
        if self.resources.len() > 256 {
            return Err(ApprovalSchemaError::TooManyResources);
        }
        if self
            .resources
            .iter()
            .any(|resource| resource.binding_lifetime != PreparedBindingLifetime::RestartStable)
        {
            return Err(ApprovalSchemaError::ProcessLocalTreeGrant);
        }
        if self
            .resources
            .iter()
            .any(|resource| resource.capability.is_path())
        {
            return Err(ApprovalSchemaError::FilesystemTreeGrant);
        }
        if self.resources.iter().any(|resource| {
            !self
                .capabilities
                .iter()
                .any(|capability| capability.action == resource.capability)
        }) {
            return Err(ApprovalSchemaError::UnboundResourceCapability);
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for TreeApprovalGrant {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            grant_id: TreeApprovalGrantId,
            root_session_id: SessionId,
            approval_id: ApprovalId,
            operation_fingerprint: OperationFingerprint,
            capabilities: Vec<ApprovalCapability>,
            resources: Vec<PreparedApprovalResource>,
            created_at: Timestamp,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            runtime_id: Option<RuntimeId>,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            grant_id: w.grant_id,
            root_session_id: w.root_session_id,
            approval_id: w.approval_id,
            operation_fingerprint: w.operation_fingerprint,
            capabilities: w.capabilities,
            resources: w.resources,
            created_at: w.created_at,
            runtime_id: w.runtime_id,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ApprovalRecord {
    pub session_id: SessionId,
    pub request: ApprovalRequest,
    pub status: ApprovalStatus,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ApprovalInternalDecision>", required)]
    pub internal_decision: Option<ApprovalInternalDecision>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ApprovalUserDecision>", required)]
    pub user_decision: Option<ApprovalUserDecision>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ApprovalFinalDecision>", required)]
    pub final_decision: Option<ApprovalFinalDecision>,
}
impl ApprovalRecord {
    pub fn validate(&self) -> Result<(), ApprovalSchemaError> {
        if let Some(internal) = &self.internal_decision {
            internal.validate()?;
        }
        if let Some(final_decision) = &self.final_decision {
            final_decision.validate()?;
        }
        let coherent = match self.status {
            ApprovalStatus::Pending => {
                self.user_decision.is_none()
                    && self.final_decision.is_none()
                    && self.internal_decision.as_ref().is_none_or(|decision| {
                        decision.decision == ApprovalInternalDecisionKind::Ask
                    })
            }
            ApprovalStatus::Escalated => {
                self.internal_decision.as_ref().is_some_and(|decision| {
                    decision.decision == ApprovalInternalDecisionKind::Escalate
                }) && self.user_decision.is_none()
                    && self.final_decision.is_none()
            }
            ApprovalStatus::Approved => self
                .final_decision
                .as_ref()
                .is_some_and(|decision| decision.outcome == ApprovalFinalOutcome::Approved),
            ApprovalStatus::Rejected => self
                .final_decision
                .as_ref()
                .is_some_and(|decision| decision.outcome == ApprovalFinalOutcome::Rejected),
            ApprovalStatus::Cancelled => self
                .final_decision
                .as_ref()
                .is_some_and(|decision| decision.outcome == ApprovalFinalOutcome::Cancelled),
            ApprovalStatus::Expired => self
                .final_decision
                .as_ref()
                .is_some_and(|decision| decision.outcome == ApprovalFinalOutcome::Expired),
        };
        let decisions_coherent = match &self.final_decision {
            None => self.user_decision.is_none(),
            Some(final_decision) if final_decision.source == ApprovalDecisionSource::User => {
                self.internal_decision.as_ref().is_some_and(|decision| {
                    decision.decision == ApprovalInternalDecisionKind::Escalate
                }) && self.user_decision.is_some_and(|user| {
                    matches!(
                        (user, final_decision.outcome, final_decision.reason_code),
                        (
                            ApprovalUserDecision::ApproveOnce,
                            ApprovalFinalOutcome::Approved,
                            ApprovalReasonCode::UserApprovedOnce
                        ) | (
                            ApprovalUserDecision::ApproveTree,
                            ApprovalFinalOutcome::Approved,
                            ApprovalReasonCode::UserApprovedTree
                        ) | (
                            ApprovalUserDecision::Reject,
                            ApprovalFinalOutcome::Rejected,
                            ApprovalReasonCode::UserRejected
                        ) | (
                            ApprovalUserDecision::Cancel,
                            ApprovalFinalOutcome::Cancelled,
                            ApprovalReasonCode::UserCancelled
                        )
                    )
                })
            }
            Some(final_decision)
                if final_decision.source == ApprovalDecisionSource::PermissionMode =>
            {
                self.user_decision.is_none()
                    && self.internal_decision.as_ref().is_some_and(|internal| {
                        internal.decision == ApprovalInternalDecisionKind::Escalate
                            && internal.source == ApprovalDecisionSource::InternalAgent
                            && internal.reason_code == ApprovalReasonCode::Escalated
                    })
            }
            Some(final_decision)
                if matches!(
                    final_decision.outcome,
                    ApprovalFinalOutcome::Approved | ApprovalFinalOutcome::Rejected
                ) =>
            {
                self.user_decision.is_none()
                    && self.internal_decision.as_ref().is_some_and(|internal| {
                        internal.source == final_decision.source
                            && internal.reason_code == final_decision.reason_code
                            && matches!(
                                (internal.decision, final_decision.outcome),
                                (
                                    ApprovalInternalDecisionKind::Allow,
                                    ApprovalFinalOutcome::Approved
                                ) | (
                                    ApprovalInternalDecisionKind::Deny,
                                    ApprovalFinalOutcome::Rejected
                                )
                            )
                    })
            }
            Some(_) => self.user_decision.is_none() && self.internal_decision.is_none(),
        };
        if coherent && decisions_coherent {
            Ok(())
        } else {
            Err(ApprovalSchemaError::ContradictoryLifecycle)
        }
    }
}
impl<'de> Deserialize<'de> for ApprovalRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            session_id: SessionId,
            request: ApprovalRequest,
            status: ApprovalStatus,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            internal_decision: Option<ApprovalInternalDecision>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            user_decision: Option<ApprovalUserDecision>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            final_decision: Option<ApprovalFinalDecision>,
        }
        let raw = serde_json::Value::deserialize(deserializer)?;
        let wire = Wire::deserialize(&raw).map_err(serde::de::Error::custom)?;
        let value = Self {
            session_id: wire.session_id,
            request: wire.request,
            status: wire.status,
            internal_decision: wire.internal_decision,
            user_decision: wire.user_decision,
            final_decision: wire.final_decision,
        };
        if reads_as_known(&raw, &value, RECORD_MEMBERS) {
            value.validate().map_err(serde::de::Error::custom)?;
        }
        Ok(value)
    }
}

/// The open-enum members whose combination an internal decision's
/// validation checks.
const DECISION_MEMBERS: &[&str] = &["/decision", "/source", "/reason_code"];
/// The same for a final decision.
const FINAL_DECISION_MEMBERS: &[&str] = &["/outcome", "/source", "/reason_code"];
/// The same for an approval record and the decisions it holds.
const RECORD_MEMBERS: &[&str] = &[
    "/status",
    "/user_decision",
    "/internal_decision/decision",
    "/internal_decision/source",
    "/internal_decision/reason_code",
    "/final_decision/outcome",
    "/final_decision/source",
    "/final_decision/reason_code",
];

/// Whether every member at `pointers` of `raw` reads as itself in `value`.
/// A value a newer build added reads as its fallback, and a combination that
/// is coherent there may not be among this build's: the writer keeps
/// coherence (`docs/site/specs/compatibility-v1.md` section 4.1), so a reader
/// checks it only for values it knows.
fn reads_as_known(raw: &serde_json::Value, value: &impl Serialize, pointers: &[&str]) -> bool {
    let Ok(read) = serde_json::to_value(value) else {
        return false;
    };
    pointers
        .iter()
        .all(|pointer| raw.pointer(pointer) == read.pointer(pointer))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalSchemaError {
    InvalidPreparedIdentity,
    MissingCapability,
    MissingResource,
    TooManyResources,
    UnboundResourceCapability,
    DuplicateResourceBinding,
    ZeroRevision,
    MissingEvaluation,
    IncompleteEvaluations,
    InvalidTreeGrantConstraint,
    ProcessLocalTreeGrant,
    FilesystemTreeGrant,
    TooManyEvaluations,
    ContradictoryDecision,
    ContradictoryLifecycle,
}
impl fmt::Display for ApprovalSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPreparedIdentity => "invalid stable prepared resource identity",
            Self::MissingCapability => "prepared operation requires at least one capability",
            Self::MissingResource => "prepared operation requires at least one resource",
            Self::TooManyResources => "prepared operation exceeds 256 resources",
            Self::UnboundResourceCapability => {
                "prepared resource capability is not present in operation capabilities"
            }
            Self::DuplicateResourceBinding => "prepared resource binding digests must be unique",
            Self::ZeroRevision => "approval revision must be positive",
            Self::MissingEvaluation => "approval requires at least one evaluation",
            Self::IncompleteEvaluations => {
                "approval evaluations must exactly cover prepared resources"
            }
            Self::InvalidTreeGrantConstraint => {
                "process-local or filesystem resources cannot allow tree grants"
            }
            Self::ProcessLocalTreeGrant => "tree grants require restart-stable resources",
            Self::FilesystemTreeGrant => "filesystem resources cannot be durable tree grants",
            Self::TooManyEvaluations => "approval evaluations exceed 256 entries",
            Self::ContradictoryDecision => {
                "approval decision source, reason, and outcome contradict"
            }
            Self::ContradictoryLifecycle => "approval lifecycle status and decisions contradict",
        })
    }
}
impl std::error::Error for ApprovalSchemaError {}
