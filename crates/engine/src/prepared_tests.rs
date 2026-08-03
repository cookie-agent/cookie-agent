use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    ActionKind, ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation,
    ApprovalResourceSource, ApprovalTrigger, Effect, PreparedApprovalResource,
    PreparedBindingLifetime, PreparedCapabilityOperation, PreparedOperationIdentity,
    PreparedResourceDigest, PreparedResourceIdentity, Sha256Digest,
};

use crate::{PreparedExecutor, PreparedTool, ToolError, ToolExecutionContext, ToolResult};

struct DropExecutor(Arc<AtomicUsize>);

impl Drop for DropExecutor {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl PreparedExecutor for DropExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(self: Box<Self>, _: ToolExecutionContext) -> Result<ToolResult, ToolError> {
        unreachable!("drop-only executor")
    }
}

fn operation(lifetime: PreparedBindingLifetime) -> PreparedOperationIdentity {
    PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"normalized"),
        vec![ApprovalCapability {
            action: ActionKind::Bash,
            operation: PreparedCapabilityOperation::new("bash:execute").expect("operation"),
        }],
        vec![PreparedApprovalResource {
            capability: ActionKind::Bash,
            canonical: PreparedResourceIdentity::new("command:test").expect("identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"binding"),
            binding_lifetime: lifetime,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(b"cwd-identity"),
    )
    .expect("prepared operation")
}

fn evaluation(operation: &PreparedOperationIdentity) -> Vec<ApprovalEvaluation> {
    operation
        .resources()
        .iter()
        .map(|resource| ApprovalEvaluation {
            resource_digest: resource.binding_digest.clone(),
            effect: Effect::Ask,
            trace: cookie_agent_protocol::DecisionTrace {
                action: resource.capability,
                normalized_resource: resource.canonical.as_str().into(),
                candidates: Vec::new(),
                effect: Effect::Ask,
                precedence_reason: "test".into(),
            },
        })
        .collect()
}

#[test]
fn denied_prepared_tool_drops_its_single_use_executor() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let prepared = PreparedTool::new(
        operation(PreparedBindingLifetime::ProcessLocal),
        None,
        Box::new(DropExecutor(dropped.clone())),
    );
    drop(prepared);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn policy_label_must_match_immutable_resource_identity() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let result = PreparedTool::new(
        operation(PreparedBindingLifetime::ProcessLocal),
        None,
        Box::new(DropExecutor(dropped.clone())),
    )
    .with_policy_labels(vec!["bash -lc safe".into()]);
    assert!(matches!(result, Err(ToolError::Failed(_))));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[test]
fn model_and_policy_approval_share_the_exact_prepared_fingerprint() {
    let operation = operation(PreparedBindingLifetime::ProcessLocal);
    let constraints = ApprovalConstraints {
        allow_once: true,
        allow_tree_grant: false,
        cancellable: true,
        expires_at: None,
    };
    let model = cookie_agent_protocol::ApprovalRequest::new(
        cookie_agent_protocol::ApprovalId::new_v7(),
        1,
        ApprovalTrigger::ModelToolApproval,
        operation.clone(),
        evaluation(&operation),
        constraints.clone(),
    )
    .expect("model approval");
    let policy = cookie_agent_protocol::ApprovalRequest::new(
        cookie_agent_protocol::ApprovalId::new_v7(),
        1,
        ApprovalTrigger::PermissionPolicy,
        operation,
        evaluation(model.operation()),
        constraints,
    )
    .expect("policy approval");
    assert_eq!(
        model.operation_fingerprint(),
        policy.operation_fingerprint()
    );
    assert_eq!(model.operation(), policy.operation());
}

#[test]
fn process_local_filesystem_style_binding_cannot_enable_tree_grants() {
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"args"),
        vec![ApprovalCapability {
            action: ActionKind::Read,
            operation: PreparedCapabilityOperation::new("read:read").expect("operation"),
        }],
        vec![PreparedApprovalResource {
            capability: ActionKind::Read,
            canonical: PreparedResourceIdentity::new("file:test").expect("identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"inode"),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(b"cwd"),
    )
    .expect("operation");
    assert!(
        cookie_agent_protocol::ApprovalRequest::new(
            cookie_agent_protocol::ApprovalId::new_v7(),
            1,
            ApprovalTrigger::PermissionPolicy,
            operation.clone(),
            evaluation(&operation),
            ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: true,
                cancellable: true,
                expires_at: None,
            },
        )
        .is_err()
    );
}
