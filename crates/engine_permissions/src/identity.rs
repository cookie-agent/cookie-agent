//! The approval identity of a call a provider only declares: a runtime's or
//! a plugin's tool, or an MCP server's. The provider states the call's
//! permission, resource, and normalized arguments; the daemon binds them to
//! the tool it knows the call goes to (`docs/site/specs/compatibility-v1.md`
//! section 9.4), so a peer can never state another tool's identity, and no
//! peer has to reproduce a digest byte for byte.

use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, PermissionAction,
    PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
    PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity, Sha256Digest,
    ToolIdentity,
};
use serde_json::Value;

use cookie_agent_runtime_api::ToolError;

/// The action a declared call of a tool that needs no permission is
/// identified under. No rule is evaluated for such a call; the identity only
/// tells repeated calls apart.
const PERMISSION_FREE_ACTION: &str = "tool";

/// The longest tool-name part of a capability operation.
const MAX_OPERATION_NAME_BYTES: usize = 128;

/// The approval identity of a declared call of `tool`, prepared under
/// `origin_context` (its origin's [`cookie_agent_runtime_api::ToolOrigin::context_bytes`]):
///
/// - its normalized arguments' digest, over their canonical JSON (object
///   members in key order);
/// - one capability, `permission` (or `tool` for a tool that needs none),
///   operation `<tool name>:call`;
/// - one resource of that action: canonical `<action>:<SHA-256 of the
///   resource>` (`<action>:unscoped` without one), bound to the tool's
///   identity and the resource. A shell command approves by command prefix,
///   anything else exactly. A command's or a path's binding is process-local,
///   since the runtime resolves it against its own machine's state at
///   preparation; every other binding is restart-stable, the tool's identity
///   and the resource being all it names;
/// - the execution context: the tool's identity and its origin's context.
///
/// The tool's identity is the source the daemon resolved (the runtime
/// attachment, plugin, or MCP server) and the name the source knows the tool
/// by, never anything the provider reports.
pub fn declared_operation(
    tool: &ToolIdentity,
    origin_context: &[u8],
    permission: Option<&PermissionAction>,
    resource: Option<&str>,
    normalized_arguments: &Value,
) -> Result<PreparedOperationIdentity, ToolError> {
    let error = |error: &dyn std::fmt::Display| ToolError::execution(error.to_string());
    let action = match permission {
        Some(action) => action.clone(),
        None => PermissionAction::new(PERMISSION_FREE_ACTION).map_err(|e| error(&e))?,
    };
    let tool_key = tool_key(tool);
    let mut binding = tool_key.clone();
    binding.push(0);
    match resource {
        Some(resource) => {
            binding.push(b'r');
            binding.extend_from_slice(resource.as_bytes());
        }
        None => binding.push(b'-'),
    }
    let canonical = match resource {
        Some(resource) => format!(
            "{}:{}",
            action.as_str(),
            Sha256Digest::of_bytes(resource.as_bytes()).as_str()
        ),
        None => format!("{}:unscoped", action.as_str()),
    };
    let boundary = match resource {
        Some(resource) if action.is_command() => ApprovalBoundary::CommandPrefix {
            prefix: resource.to_owned(),
        },
        _ => ApprovalBoundary::Exact,
    };
    let binding_lifetime = if action.is_command() || action.is_path() {
        PreparedBindingLifetime::ProcessLocal
    } else {
        PreparedBindingLifetime::RestartStable
    };
    let mut context = tool_key;
    context.push(0);
    context.extend_from_slice(origin_context);
    PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(&canonical_json(normalized_arguments)),
        vec![ApprovalCapability {
            action: action.clone(),
            operation: PreparedCapabilityOperation::new(format!(
                "{}:call",
                operation_name(&tool.name)
            ))
            .map_err(|e| error(&e))?,
        }],
        vec![PreparedApprovalResource {
            capability: action,
            canonical: PreparedResourceIdentity::new(canonical).map_err(|e| error(&e))?,
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(&binding),
            binding_lifetime,
            boundary,
            source: ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(&context),
    )
    .map_err(|e| error(&e))
}

/// The bytes that name `tool`: its resolved source and the name its source
/// knows it by.
fn tool_key(tool: &ToolIdentity) -> Vec<u8> {
    let source = tool.source.as_ref().map_or(Value::Null, |source| {
        serde_json::to_value(source).expect("tool sources serialize")
    });
    let mut key = canonical_json(&source);
    key.push(0);
    key.extend_from_slice(tool.name.as_bytes());
    key
}

/// `name` as a capability operation part: lowercase ASCII letters, digits,
/// `.`, `-`, and `_`, starting with a letter or digit.
fn operation_name(name: &str) -> String {
    let mut part = String::with_capacity(name.len().min(MAX_OPERATION_NAME_BYTES));
    for character in name.chars() {
        if part.len() == MAX_OPERATION_NAME_BYTES {
            break;
        }
        let character = character.to_ascii_lowercase();
        part.push(
            if character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '-' | '_')
            {
                character
            } else {
                '_'
            },
        );
    }
    if !part.starts_with(|character: char| character.is_ascii_alphanumeric()) {
        part.insert_str(0, "tool");
    }
    part
}

/// `value` as JSON text with every object's members in key order, so equal
/// values have equal digests whatever order a peer sent them in.
fn canonical_json(value: &Value) -> Vec<u8> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(members) => {
                let mut entries = members.iter().collect::<Vec<_>>();
                entries.sort_by(|left, right| left.0.cmp(right.0));
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(key, value)| (key.clone(), sorted(value)))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_vec(&sorted(value)).expect("JSON values serialize")
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
