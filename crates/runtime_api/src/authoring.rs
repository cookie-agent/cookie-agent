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

/// The JSON Schema of a tool's parameters type, as a model reads it: without
/// the meta-schema, the type's title, and the doc comments of the type and
/// of the types it refers to (written for whoever reads the code: the tool's
/// description says what it takes, and each member's doc comment what that
/// member is); without `x-` extension keywords; an optional member as its
/// type alone, not that type or `null`; and descriptions with the line
/// breaks of wrapped doc comments joined, paragraphs kept. Generated once
/// per type: providers publish their tools at every model step.
#[must_use]
pub fn schema<T: JsonSchema + 'static>() -> serde_json::Value {
    memoized::<T>(false, || {
        let mut schema =
            serde_json::to_value(schemars::schema_for!(T)).expect("tool schemas serialize");
        if let Some(root) = schema.as_object_mut() {
            for keyword in ["$schema", "title", "description"] {
                root.remove(keyword);
            }
            if let Some(definitions) = root
                .get_mut("$defs")
                .and_then(serde_json::Value::as_object_mut)
            {
                for definition in definitions.values_mut() {
                    if let Some(definition) = definition.as_object_mut() {
                        definition.remove("description");
                    }
                }
            }
        }
        tidy(&mut schema);
        schema
    })
}

/// Tidies `schema` and every schema below it as [`schema`] describes.
fn tidy(schema: &mut serde_json::Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    object.retain(|keyword, _| !keyword.starts_with("x-"));
    if let Some(serde_json::Value::String(description)) = object.get_mut("description") {
        *description = join_wrapped_lines(description);
    }
    let required: Vec<String> = object
        .get("required")
        .and_then(serde_json::Value::as_array)
        .map(|required| {
            required
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if let Some(properties) = object
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    {
        for (name, property) in properties.iter_mut() {
            if !required.contains(name) {
                drop_null(property);
            }
        }
    }
    for (keyword, value) in object.iter_mut() {
        match keyword.as_str() {
            "properties" | "$defs" | "definitions" => {
                if let Some(schemas) = value.as_object_mut() {
                    schemas.values_mut().for_each(tidy);
                }
            }
            "oneOf" | "anyOf" | "allOf" | "prefixItems" => {
                if let Some(schemas) = value.as_array_mut() {
                    schemas.iter_mut().for_each(tidy);
                }
            }
            "items" | "additionalProperties" | "not" => tidy(value),
            _ => {}
        }
    }
}

/// An optional member's schema without the `null` an `Option` adds: a
/// `["T", "null"]` type becomes `"T"`, an `anyOf` of one schema and
/// `{"type": "null"}` becomes that schema, keeping the member's own keywords,
/// and a `null` default goes. Leaving the member out means the same, and
/// serde still takes `null`.
fn drop_null(property: &mut serde_json::Value) {
    let Some(object) = property.as_object_mut() else {
        return;
    };
    if object.get("default") == Some(&serde_json::Value::Null) {
        object.remove("default");
    }
    if let Some(serde_json::Value::Array(types)) = object.get("type") {
        let other: Vec<&serde_json::Value> = types.iter().filter(|kind| *kind != "null").collect();
        if other.len() == 1 && types.len() == 2 {
            let kind = other[0].clone();
            object.insert("type".into(), kind);
        }
        return;
    }
    let Some(serde_json::Value::Array(branches)) = object.get("anyOf") else {
        return;
    };
    let is_null = |branch: &serde_json::Value| *branch == serde_json::json!({"type": "null"});
    if branches.len() != 2 || branches.iter().filter(|branch| is_null(branch)).count() != 1 {
        return;
    }
    let Some(serde_json::Value::Object(kept)) = branches.iter().find(|branch| !is_null(branch))
    else {
        return;
    };
    let kept = kept.clone();
    object.remove("anyOf");
    for (keyword, value) in kept {
        object.entry(keyword).or_insert(value);
    }
}

/// `text` with each single line break, as a wrapped doc comment leaves,
/// joined into a space; a blank line between paragraphs stays.
fn join_wrapped_lines(text: &str) -> String {
    text.split("\n\n")
        .map(|paragraph| paragraph.replace('\n', " "))
        .collect::<Vec<_>>()
        .join("\n\n")
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

#[cfg(test)]
mod tests {
    use schemars::JsonSchema;
    use serde::Deserialize;

    /// The arguments' own doc comment.
    #[derive(Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct Args {
        /// A wrapped
        /// doc comment.
        ///
        /// Second paragraph.
        name: String,
        /// Optional text.
        #[serde(default)]
        note: Option<String>,
        /// Optional with a serde default.
        #[serde(default)]
        #[schemars(extend("default" = null))]
        defaulted: Option<String>,
        /// Optional enum.
        #[serde(default)]
        kind: Option<Kind>,
    }

    /// A doc comment for whoever reads the code.
    #[derive(Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    #[schemars(extend("x-open" = true))]
    #[allow(dead_code)]
    enum Kind {
        One,
        Two,
    }

    #[test]
    fn schema_drops_meta_and_null_and_joins_wrapped_lines() {
        let schema = super::schema::<Args>();
        let root = schema.as_object().unwrap();
        for keyword in ["$schema", "title", "description"] {
            assert!(!root.contains_key(keyword), "{keyword} kept");
        }
        let properties = &schema["properties"];
        assert_eq!(
            properties["name"]["description"],
            "A wrapped doc comment.\n\nSecond paragraph."
        );
        assert_eq!(properties["note"]["type"], "string");
        assert!(properties["defaulted"].get("default").is_none());
        assert_eq!(properties["kind"]["$ref"], "#/$defs/Kind");
        assert_eq!(properties["kind"]["description"], "Optional enum.");
        assert!(properties["kind"].get("anyOf").is_none());
        let kind = &schema["$defs"]["Kind"];
        assert!(kind.get("description").is_none());
        assert!(kind.get("x-open").is_none());
    }
}
