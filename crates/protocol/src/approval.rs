use std::{borrow::Cow, fmt};

use jiff::Timestamp;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{
    ApprovalId, PermissionAction, PermissionEffect, SafeCode, SafeErrorMessage, SessionId,
    Sha256Digest, TreeApprovalGrantId,
};

#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
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
        #[serde(deny_unknown_fields)]
        struct Wire {
            digest: Sha256Digest,
        }
        Ok(Self {
            digest: Wire::deserialize(d)?.digest,
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
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
        #[serde(deny_unknown_fields)]
        struct Wire {
            digest: Sha256Digest,
        }
        Ok(Self {
            digest: Wire::deserialize(d)?.digest,
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalBoundary {
    Exact,
    CommandPrefix { prefix: String },
    DelegationTree { root_session_id: SessionId },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResourceSource {
    PrimaryOperation,
    SecondaryOperation,
    ExternalDirectoryGuard,
    ModelRequest,
    DoomLoopGuard,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PreparedBindingLifetime {
    ProcessLocal,
    RestartStable,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PreparedApprovalResource {
    pub capability: PermissionAction,
    pub canonical: PreparedResourceIdentity,
    pub binding_digest: PreparedResourceDigest,
    pub binding_lifetime: PreparedBindingLifetime,
    pub boundary: ApprovalBoundary,
    pub source: ApprovalResourceSource,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MatchedPermissionRule {
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeCode>", required)]
    pub rule_id: Option<SafeCode>,
    pub source_layer: SafeCode,
    pub effect: PermissionEffect,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DecisionTrace {
    pub action: PermissionAction,
    pub normalized_resource: String,
    pub candidates: Vec<MatchedPermissionRule>,
    pub effect: PermissionEffect,
    pub precedence_reason: String,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalEvaluation {
    pub resource_digest: PreparedResourceDigest,
    pub effect: PermissionEffect,
    pub trace: DecisionTrace,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTrigger {
    PermissionPolicy,
    ModelToolApproval,
    InternalAgent,
    DoomLoop,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalCapability {
    pub action: PermissionAction,
    pub operation: PreparedCapabilityOperation,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PreparedCapabilityLifetime {
    ProcessLocal,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PreparedOperationIdentity {
    normalized_arguments_digest: Sha256Digest,
    #[schemars(length(min = 1, max = 256))]
    capabilities: Vec<ApprovalCapability>,
    #[schemars(length(max = 256))]
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
        #[serde(deny_unknown_fields)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalConstraints {
    pub allow_once: bool,
    pub allow_tree_grant: bool,
    pub cancellable: bool,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<Timestamp>", required)]
    pub expires_at: Option<Timestamp>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReasonCode {
    PolicyAllowed,
    PolicyDenied,
    PolicyRequiresApproval,
    ModelRequested,
    InternalAgentAllowed,
    InternalAgentDenied,
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
    SystemError,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecisionSource {
    Policy,
    Model,
    InternalAgent,
    TreeGrant,
    User,
    DoomLoopGuard,
    System,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Escalated,
    Approved,
    Rejected,
    Cancelled,
    Expired,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalFeedback {
    pub message: SafeErrorMessage,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalInternalDecisionKind {
    Allow,
    Deny,
    Ask,
    Escalate,
}
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalInternalDecision {
    pub decision: ApprovalInternalDecisionKind,
    pub source: ApprovalDecisionSource,
    pub reason_code: ApprovalReasonCode,
    #[schemars(length(max = 256))]
    pub evaluations: Vec<ApprovalEvaluation>,
}
impl ApprovalInternalDecision {
    pub fn validate(&self) -> Result<(), ApprovalSchemaError> {
        if self.evaluations.len() > 256 {
            return Err(ApprovalSchemaError::TooManyEvaluations);
        }
        let coherent = match self.decision {
            ApprovalInternalDecisionKind::Allow => matches!(
                self.reason_code,
                ApprovalReasonCode::PolicyAllowed
                    | ApprovalReasonCode::InternalAgentAllowed
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
            | ApprovalReasonCode::PolicyRequiresApproval => {
                self.source == ApprovalDecisionSource::Policy
            }
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
        #[serde(deny_unknown_fields)]
        struct Wire {
            decision: ApprovalInternalDecisionKind,
            source: ApprovalDecisionSource,
            reason_code: ApprovalReasonCode,
            evaluations: Vec<ApprovalEvaluation>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            decision: wire.decision,
            source: wire.source,
            reason_code: wire.reason_code,
            evaluations: wire.evaluations,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalUserDecision {
    ApproveOnce,
    ApproveTree,
    Reject,
    Cancel,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalFinalOutcome {
    Approved,
    Rejected,
    Cancelled,
    Expired,
}
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
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
                    | ApprovalReasonCode::TreeGrantMatched
                    | ApprovalReasonCode::UserApprovedOnce
                    | ApprovalReasonCode::UserApprovedTree
            ),
            ApprovalFinalOutcome::Rejected => matches!(
                self.reason_code,
                ApprovalReasonCode::PolicyDenied
                    | ApprovalReasonCode::InternalAgentDenied
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
            ApprovalReasonCode::PolicyAllowed | ApprovalReasonCode::PolicyDenied => {
                self.source == ApprovalDecisionSource::Policy
            }
            ApprovalReasonCode::InternalAgentAllowed | ApprovalReasonCode::InternalAgentDenied => {
                self.source == ApprovalDecisionSource::InternalAgent
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
        #[serde(deny_unknown_fields)]
        struct Wire {
            outcome: ApprovalFinalOutcome,
            source: ApprovalDecisionSource,
            reason_code: ApprovalReasonCode,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            feedback: Option<ApprovalFeedback>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            tree_grant_id: Option<TreeApprovalGrantId>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            outcome: wire.outcome,
            source: wire.source,
            reason_code: wire.reason_code,
            feedback: wire.feedback,
            tree_grant_id: wire.tree_grant_id,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequest {
    approval_id: ApprovalId,
    #[schemars(range(min = 1))]
    revision: u64,
    trigger: ApprovalTrigger,
    operation_fingerprint: OperationFingerprint,
    operation: PreparedOperationIdentity,
    #[schemars(length(max = 256))]
    evaluations: Vec<ApprovalEvaluation>,
    constraints: ApprovalConstraints,
}
impl ApprovalRequest {
    pub fn new(
        approval_id: ApprovalId,
        revision: u64,
        trigger: ApprovalTrigger,
        operation: PreparedOperationIdentity,
        evaluations: Vec<ApprovalEvaluation>,
        constraints: ApprovalConstraints,
    ) -> Result<Self, ApprovalSchemaError> {
        if revision == 0 {
            return Err(ApprovalSchemaError::ZeroRevision);
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
                    || matches!(
                        resource.capability,
                        PermissionAction::Read
                            | PermissionAction::Write
                            | PermissionAction::Grep
                            | PermissionAction::Glob
                            | PermissionAction::ExternalDirectory
                    )
            })
        {
            return Err(ApprovalSchemaError::InvalidTreeGrantConstraint);
        }
        let operation_fingerprint = OperationFingerprint::from_prepared_operation(&operation);
        Ok(Self {
            approval_id,
            revision,
            trigger,
            operation_fingerprint,
            operation,
            evaluations,
            constraints,
        })
    }
    #[must_use]
    pub const fn approval_id(&self) -> ApprovalId {
        self.approval_id
    }
    #[must_use]
    pub const fn operation_fingerprint(&self) -> &OperationFingerprint {
        &self.operation_fingerprint
    }
    #[must_use]
    pub const fn operation(&self) -> &PreparedOperationIdentity {
        &self.operation
    }
}
impl<'de> Deserialize<'de> for ApprovalRequest {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            approval_id: ApprovalId,
            revision: u64,
            trigger: ApprovalTrigger,
            operation_fingerprint: OperationFingerprint,
            operation: PreparedOperationIdentity,
            evaluations: Vec<ApprovalEvaluation>,
            constraints: ApprovalConstraints,
        }
        let w = Wire::deserialize(d)?;
        let fingerprint = w.operation_fingerprint.clone();
        let value = Self::new(
            w.approval_id,
            w.revision,
            w.trigger,
            w.operation,
            w.evaluations,
            w.constraints,
        )
        .map_err(serde::de::Error::custom)?;
        if value.operation_fingerprint != fingerprint {
            return Err(serde::de::Error::custom(
                "approval operation fingerprint mismatch",
            ));
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct TreeApprovalGrant {
    pub grant_id: TreeApprovalGrantId,
    pub root_session_id: SessionId,
    pub approval_id: ApprovalId,
    pub operation_fingerprint: OperationFingerprint,
    #[schemars(length(min = 1, max = 256))]
    pub capabilities: Vec<ApprovalCapability>,
    #[schemars(length(max = 256))]
    pub resources: Vec<PreparedApprovalResource>,
    pub created_at: Timestamp,
}
impl TreeApprovalGrant {
    pub fn validate(&self) -> Result<(), ApprovalSchemaError> {
        if self.capabilities.is_empty() || self.capabilities.len() > 256 {
            return Err(ApprovalSchemaError::MissingCapability);
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
        if self.resources.iter().any(|resource| {
            matches!(
                resource.capability,
                PermissionAction::Read
                    | PermissionAction::Write
                    | PermissionAction::Grep
                    | PermissionAction::Glob
                    | PermissionAction::ExternalDirectory
            )
        }) {
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
        #[serde(deny_unknown_fields)]
        struct Wire {
            grant_id: TreeApprovalGrantId,
            root_session_id: SessionId,
            approval_id: ApprovalId,
            operation_fingerprint: OperationFingerprint,
            capabilities: Vec<ApprovalCapability>,
            resources: Vec<PreparedApprovalResource>,
            created_at: Timestamp,
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
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
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
        #[serde(deny_unknown_fields)]
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
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            session_id: wire.session_id,
            request: wire.request,
            status: wire.status,
            internal_decision: wire.internal_decision,
            user_decision: wire.user_decision,
            final_decision: wire.final_decision,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalSchemaError {
    InvalidPreparedIdentity,
    MissingCapability,
    TooManyResources,
    UnboundResourceCapability,
    DuplicateResourceBinding,
    ZeroRevision,
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
            Self::TooManyResources => "prepared operation exceeds 256 resources",
            Self::UnboundResourceCapability => {
                "prepared resource capability is not present in operation capabilities"
            }
            Self::DuplicateResourceBinding => "prepared resource binding digests must be unique",
            Self::ZeroRevision => "approval revision must be positive",
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
