//! Helpers shared by tool providers: argument parsing, parameter schemas,
//! result titles, and prepared-operation identities.

use std::{
    any::TypeId,
    collections::HashMap,
    sync::{OnceLock, RwLock},
};

use cookie_agent_protocol::{
    ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, PermissionAction,
    PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
    PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity, SafeDisplayText,
    Sha256Digest,
};
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};

use crate::ToolError;

fn tool_error(error: impl std::fmt::Display) -> ToolError {
    ToolError::execution(error.to_string())
}

/// The JSON Schema of a tool's parameters type. Generated once per type:
/// providers publish their tools at every model step.
#[must_use]
pub fn schema<T: JsonSchema + 'static>() -> serde_json::Value {
    memoized::<T>(false, || {
        serde_json::to_value(schemars::schema_for!(T)).expect("tool schemas serialize")
    })
}

/// The schema of arguments [`parse_args_strict`] reads: every object that
/// lists its properties refuses others, including types history also stores,
/// which ignore them there.
#[must_use]
pub fn strict_schema<T: JsonSchema + 'static>() -> serde_json::Value {
    memoized::<T>(true, || {
        let mut schema = schema::<T>();
        close_objects(&mut schema);
        schema
    })
}

/// The schema `build` makes for `T`, `strict` or not, built on first use and
/// cloned from then on.
fn memoized<T: 'static>(
    strict: bool,
    build: impl FnOnce() -> serde_json::Value,
) -> serde_json::Value {
    type Schemas = HashMap<(TypeId, bool), serde_json::Value>;
    static SCHEMAS: OnceLock<RwLock<Schemas>> = OnceLock::new();
    let schemas = SCHEMAS.get_or_init(RwLock::default);
    let key = (TypeId::of::<T>(), strict);
    if let Some(schema) = schemas
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
    {
        return schema.clone();
    }
    // Built outside the lock; two first uses may both build it.
    let schema = build();
    schemas
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(key)
        .or_insert(schema)
        .clone()
}
fn close_objects(schema: &mut serde_json::Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if object.contains_key("properties") && !object.contains_key("additionalProperties") {
        object.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    for (keyword, value) in object {
        match keyword.as_str() {
            "properties" | "$defs" | "definitions" => {
                if let Some(schemas) = value.as_object_mut() {
                    schemas.values_mut().for_each(close_objects);
                }
            }
            "oneOf" | "anyOf" | "allOf" | "prefixItems" => {
                if let Some(schemas) = value.as_array_mut() {
                    schemas.iter_mut().for_each(close_objects);
                }
            }
            "items" | "additionalProperties" | "not" => close_objects(value),
            _ => {}
        }
    }
}

/// A tool result title: control characters replaced, cut to the display
/// bound, `Tool result` when nothing is left.
#[must_use]
pub fn safe_title(value: impl AsRef<str>) -> SafeDisplayText {
    let mut safe = String::new();
    for character in value.as_ref().chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if safe.len() + character.len_utf8() > SafeDisplayText::MAX_BYTES {
            break;
        }
        safe.push(character);
    }
    SafeDisplayText::new(if safe.is_empty() {
        "Tool result".to_owned()
    } else {
        safe
    })
    .expect("sanitized tool title")
}

/// Deserializes a call's arguments, with an error the model can act on.
pub fn parse_args<T: DeserializeOwned>(
    tool: &str,
    value: serde_json::Value,
) -> Result<T, ToolError> {
    serde_json::from_value(value).map_err(|error| {
        tool_error(format!(
            "The {tool} tool was called with invalid arguments: {error}.\nPlease rewrite the input so it satisfies the expected schema."
        ))
    })
}

/// Like [`parse_args`], for arguments that embed a type history also stores:
/// such a type ignores unknown members, so they are refused here against the
/// arguments' schema instead.
pub fn parse_args_strict<T>(tool: &str, value: serde_json::Value) -> Result<T, ToolError>
where
    T: DeserializeOwned + schemars::JsonSchema + 'static,
{
    match cookie_agent_protocol::strict::decode_command(value) {
        Ok(arguments) => Ok(arguments),
        Err(error) => Err(tool_error(format!(
            "The {tool} tool was called with invalid arguments: {error}.\nPlease rewrite the input so it satisfies the expected schema."
        ))),
    }
}

/// The identity of a prepared operation of `tool`: its normalized
/// arguments, capabilities, resources, and execution context.
pub fn prepared_operation<T: Serialize>(
    tool: &str,
    normalized_arguments: &T,
    capabilities: Vec<(PermissionAction, &str)>,
    resources: Vec<PreparedApprovalResource>,
    execution_context_bytes: &[u8],
) -> Result<PreparedOperationIdentity, ToolError> {
    let arguments = serde_json::to_vec(normalized_arguments).map_err(tool_error)?;
    let capabilities = capabilities
        .into_iter()
        .map(|(action, operation)| {
            Ok(ApprovalCapability {
                action,
                operation: PreparedCapabilityOperation::new(format!("{tool}:{operation}"))
                    .map_err(tool_error)?,
            })
        })
        .collect::<Result<Vec<_>, ToolError>>()?;
    PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(&arguments),
        capabilities,
        resources,
        Sha256Digest::of_bytes(execution_context_bytes),
    )
    .map_err(tool_error)
}

/// One approval resource of a prepared operation. `bash` resources approve
/// by command prefix; every other action approves the exact resource.
pub fn prepared_resource(
    action: PermissionAction,
    logical_kind: &str,
    stable_label_bytes: &[u8],
    binding_bytes: &[u8],
    lifetime: PreparedBindingLifetime,
    source: ApprovalResourceSource,
) -> Result<PreparedApprovalResource, ToolError> {
    let label = Sha256Digest::of_bytes(stable_label_bytes);
    let mut complete_binding = Vec::new();
    complete_binding.extend_from_slice(logical_kind.as_bytes());
    complete_binding.push(0);
    complete_binding.extend_from_slice(stable_label_bytes);
    complete_binding.push(0);
    complete_binding.extend_from_slice(binding_bytes);
    let boundary = if action == PermissionAction::BASH {
        ApprovalBoundary::CommandPrefix {
            prefix: String::from_utf8_lossy(stable_label_bytes).into_owned(),
        }
    } else {
        ApprovalBoundary::Exact
    };
    Ok(PreparedApprovalResource {
        capability: action,
        canonical: PreparedResourceIdentity::new(format!("{logical_kind}:{}", label.as_str()))
            .map_err(tool_error)?,
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(&complete_binding),
        binding_lifetime: lifetime,
        boundary,
        source,
    })
}
