//! Strict decoding of commands (`docs/site/specs/compatibility-v1.md`
//! section 4.1).
//!
//! Information types ignore members they do not know, so a type that is
//! also part of a command (a [`crate::RunSelection`] in `run.start`, a
//! [`crate::PermissionRule`] in `session.create`) cannot reject them itself.
//! A command's receiver therefore checks the params against the command's
//! published JSON Schema first: every member of an object must be one of the
//! properties the schema lists for it, and every enum value or tag one the
//! schema lists, wherever it is in the params. Serde then decodes the params
//! as usual. Open enums are no exception: a command never carries a value its
//! receiver does not know, since its sender gates new values on the
//! receiver's version.

use std::{
    any::TypeId,
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use schemars::{JsonSchema, Schema, schema_for};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Why a command's params were refused.
#[derive(Debug)]
pub enum CommandDecodeError {
    /// A member the command's schema does not list, at this JSON path
    /// (`selection.model.extra`, `permission_rules[0].note`).
    UnknownField {
        field: String,
    },
    /// An enum value or tag the command's schema does not list, at this JSON
    /// path: the value itself for a plain enum (`mode`), or its tag member for
    /// a tagged one (`outcome.status`).
    UnknownVariant {
        field: String,
        value: String,
    },
    Invalid(serde_json::Error),
}

impl CommandDecodeError {
    /// The error a receiver answers with: `-32602` with the registry's
    /// `unknown_field` or `unknown_variant` data naming the path, or plain
    /// `invalid_params` with serde's message as its detail.
    #[must_use]
    pub fn to_rpc_error(&self) -> crate::JsonRpcError {
        match self {
            Self::UnknownField { field } => crate::errors::UNKNOWN_FIELD
                .error()
                .field("field", field)
                .build(),
            Self::UnknownVariant { field, value } => crate::errors::UNKNOWN_VARIANT
                .error()
                .field("field", field)
                .field("value", value)
                .build(),
            Self::Invalid(error) => crate::errors::BAD_PARAMS
                .error()
                .detail(error.to_string())
                .build(),
        }
    }
}

impl std::fmt::Display for CommandDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownField { field } => write!(formatter, "unknown field `{field}`"),
            Self::UnknownVariant { field, value } => {
                write!(formatter, "unknown value `{value}` at `{field}`")
            }
            Self::Invalid(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CommandDecodeError {}

/// Decodes `value` as the params of a command of type `T`, refusing any
/// member or enum value `T`'s schema does not list.
pub fn decode_command<T>(value: Value) -> Result<T, CommandDecodeError>
where
    T: DeserializeOwned + JsonSchema + 'static,
{
    check_command(&command_schema::<T>(), &value)?;
    serde_json::from_value(value).map_err(CommandDecodeError::Invalid)
}

/// Checks `value` against the schema of the params of a command of type
/// `T`, as [`decode_command`] does before decoding, without decoding it.
pub fn check_command_params<T>(value: &Value) -> Result<(), CommandDecodeError>
where
    T: JsonSchema + 'static,
{
    check_command(&command_schema::<T>(), value)
}

/// Checks `value` against a command's `schema`: the first member the schema
/// does not list, or enum value it does not list, is refused with its JSON
/// path. Members are checked only where the schema lists properties; a
/// free-form object (one that lists none) takes any.
pub fn check_command(schema: &Schema, value: &Value) -> Result<(), CommandDecodeError> {
    let root = schema.as_value();
    let definitions = root.get("$defs").and_then(Value::as_object);
    let mut path = String::new();
    match (Walker { definitions }).unknown(root, value, &mut path) {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    }
}

fn command_schema<T: JsonSchema + 'static>() -> Arc<Schema> {
    static SCHEMAS: OnceLock<Mutex<HashMap<TypeId, Arc<Schema>>>> = OnceLock::new();
    let schemas = SCHEMAS.get_or_init(Mutex::default);
    if let Some(schema) = schemas
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&TypeId::of::<T>())
    {
        return Arc::clone(schema);
    }
    // Built outside the lock; two first decodes may both build it.
    let schema = Arc::new(schema_for!(T));
    schemas
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .entry(TypeId::of::<T>())
        .or_insert(schema)
        .clone()
}

/// The deepest `$ref` chain followed before a schema is taken as free-form.
const MAX_REFERENCE_DEPTH: usize = 32;

struct Walker<'a> {
    definitions: Option<&'a Map<String, Value>>,
}

impl<'a> Walker<'a> {
    fn resolve(&self, mut schema: &'a Value) -> &'a Value {
        for _ in 0..MAX_REFERENCE_DEPTH {
            let Some(name) = schema
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix("#/$defs/"))
            else {
                return schema;
            };
            match self
                .definitions
                .and_then(|definitions| definitions.get(name))
            {
                Some(definition) => schema = definition,
                None => return &Value::Bool(true),
            }
        }
        &Value::Bool(true)
    }

    fn unknown(
        &self,
        schema: &'a Value,
        value: &Value,
        path: &mut String,
    ) -> Option<CommandDecodeError> {
        let schema = self.resolve(schema);
        let alternatives = alternatives(schema)
            .map(|branch| self.resolve(branch))
            .collect::<Vec<_>>();
        if alternatives.is_empty() {
            return self.unknown_in(&[schema], value, path);
        }
        let branches = alternatives
            .iter()
            .copied()
            .filter(|branch| admits(branch, value))
            .collect::<Vec<_>>();
        if branches.is_empty() {
            return unknown_variant(&alternatives, value, path)
                .or_else(|| self.unknown_in(&[schema], value, path));
        }
        // One branch the value fits is enough; otherwise report the first.
        let mut first = None;
        for branch in branches {
            // A branch that is itself a choice, such as the enum of an
            // optional tagged value, is checked as one.
            let found = if alternatives_of(branch) {
                self.unknown(branch, value, path)
            } else {
                self.unknown_in(&[branch, schema], value, path)
            }?;
            first.get_or_insert(found);
        }
        first
    }

    /// The check against the keywords of `schemas` together (a branch and the
    /// schema around it), nested branches aside.
    fn unknown_in(
        &self,
        schemas: &[&'a Value],
        value: &Value,
        path: &mut String,
    ) -> Option<CommandDecodeError> {
        let keyword = |name: &str| schemas.iter().find_map(|schema| schema.get(name));
        match value {
            Value::Object(members) => {
                let mut properties = Map::new();
                for schema in schemas {
                    self.collect_properties(schema, &mut properties);
                }
                let additional = keyword("additionalProperties").filter(|extra| extra.is_object());
                let free_form = properties.is_empty() || keyword("patternProperties").is_some();
                for (name, member) in members {
                    let length = path.len();
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(name);
                    let found = match (properties.get(name), additional) {
                        (Some(property), _) => self.unknown(property, member, path),
                        (None, Some(additional)) => self.unknown(additional, member, path),
                        (None, None) if free_form => None,
                        (None, None) => Some(CommandDecodeError::UnknownField {
                            field: path.clone(),
                        }),
                    };
                    path.truncate(length);
                    if found.is_some() {
                        return found;
                    }
                }
                None
            }
            Value::Array(items) => {
                let prefix = keyword("prefixItems").and_then(Value::as_array);
                let rest = keyword("items").filter(|items| items.is_object());
                for (index, item) in items.iter().enumerate() {
                    let Some(item_schema) = prefix.and_then(|prefix| prefix.get(index)).or(rest)
                    else {
                        continue;
                    };
                    let length = path.len();
                    path.push_str(&format!("[{index}]"));
                    let found = self.unknown(item_schema, item, path);
                    path.truncate(length);
                    if found.is_some() {
                        return found;
                    }
                }
                None
            }
            scalar => {
                let listed = keyword("enum").and_then(Value::as_array)?;
                (!listed.contains(scalar)).then(|| CommandDecodeError::UnknownVariant {
                    field: path.clone(),
                    value: variant_text(scalar),
                })
            }
        }
    }

    /// `schema`'s properties, with those of every `allOf` part.
    fn collect_properties(&self, schema: &'a Value, properties: &mut Map<String, Value>) {
        if let Some(own) = schema.get("properties").and_then(Value::as_object) {
            for (name, property) in own {
                properties
                    .entry(name.clone())
                    .or_insert_with(|| property.clone());
            }
        }
        for part in schema
            .get("allOf")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.collect_properties(self.resolve(part), properties);
        }
    }
}

/// The `oneOf` and `anyOf` branches of `schema`.
fn alternatives(schema: &Value) -> impl Iterator<Item = &Value> {
    ["oneOf", "anyOf"]
        .into_iter()
        .filter_map(|keyword| schema.get(keyword).and_then(Value::as_array))
        .flatten()
}

fn alternatives_of(schema: &Value) -> bool {
    alternatives(schema).next().is_some()
}

/// The refusal of a value that fits none of a choice's `alternatives`, when
/// the choice is a closed set of values: a string none of its `const` or
/// `enum` branches lists, or an object whose tag (a member every object
/// branch fixes with a `const`) names none of them. `None` when the value is
/// simply malformed, which serde reports.
fn unknown_variant(
    alternatives: &[&Value],
    value: &Value,
    path: &str,
) -> Option<CommandDecodeError> {
    let is_null = |branch: &&Value| branch.get("type").and_then(Value::as_str) == Some("null");
    match value {
        Value::String(text) => alternatives
            .iter()
            .all(|branch| {
                branch.get("const").is_some() || branch.get("enum").is_some() || is_null(branch)
            })
            .then(|| CommandDecodeError::UnknownVariant {
                field: path.to_owned(),
                value: text.clone(),
            }),
        Value::Object(members) => {
            let objects = alternatives
                .iter()
                .filter(|branch| !is_null(branch))
                .collect::<Vec<_>>();
            let tag_of = |branch: &Value, name: &str| {
                branch
                    .get("properties")
                    .and_then(|properties| properties.get(name))
                    .and_then(|property| property.get("const"))
                    .cloned()
            };
            let first = objects.first()?;
            let tag = first
                .get("properties")
                .and_then(Value::as_object)?
                .keys()
                .find(|name| objects.iter().all(|branch| tag_of(branch, name).is_some()))?;
            let found = members.get(tag)?;
            if !found.is_string()
                || objects
                    .iter()
                    .any(|branch| tag_of(branch, tag).as_ref() == Some(found))
            {
                return None;
            }
            Some(CommandDecodeError::UnknownVariant {
                field: if path.is_empty() {
                    tag.clone()
                } else {
                    format!("{path}.{tag}")
                },
                value: variant_text(found),
            })
        }
        _ => None,
    }
}

fn variant_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Whether a branch of a `oneOf` or `anyOf` can describe `value` at all: its
/// type allows the value's, a `const` or `enum` lists it, and an object has
/// every required member, with each `const` member (a tag) equal to the
/// branch's.
fn admits(branch: &Value, value: &Value) -> bool {
    if let Some(types) = branch.get("type") {
        let name = match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        let allowed = |allowed: &str| allowed == name || (allowed == "number" && name == "integer");
        let fits = match types {
            Value::String(single) => allowed(single),
            Value::Array(several) => several.iter().filter_map(Value::as_str).any(allowed),
            _ => true,
        };
        if !fits {
            return false;
        }
    }
    if branch
        .get("const")
        .is_some_and(|constant| constant != value)
        || branch
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|listed| !listed.contains(value))
    {
        return false;
    }
    let Value::Object(members) = value else {
        return true;
    };
    let required = branch
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .all(|name| members.contains_key(name));
    let tags_match = branch
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, property)| Some((name, property.get("const")?)))
        .all(|(name, tag)| members.get(name).is_none_or(|member| member == tag));
    required && tags_match
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    // Only the decoding matters here, not the decoded values.
    #[allow(dead_code)]
    #[derive(Debug, Deserialize, JsonSchema)]
    struct Shared {
        name: String,
        #[serde(default)]
        note: Option<String>,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum Part {
        Text { text: String },
        Shared { shared: Shared },
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    enum Mode {
        Steer,
        Queue,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    enum Documented {
        /// First.
        First,
        /// Second.
        Second,
    }

    #[allow(dead_code)]
    #[derive(Debug, Deserialize, JsonSchema)]
    struct Command {
        shared: Shared,
        #[serde(default)]
        mode: Option<Mode>,
        #[serde(default)]
        documented: Vec<Documented>,
        #[serde(default)]
        part: Option<Part>,
        #[serde(default)]
        optional: Option<Shared>,
        #[serde(default)]
        parts: Vec<Part>,
        #[serde(default)]
        labels: HashMap<String, Shared>,
        #[serde(default)]
        free: Option<Value>,
    }

    fn unknown(value: Value) -> Option<String> {
        match decode_command::<Command>(value) {
            Err(CommandDecodeError::UnknownField { field }) => Some(field),
            Err(error) => panic!("not an unknown field: {error}"),
            Ok(_) => None,
        }
    }

    fn unknown_variant(value: Value) -> Option<(String, String)> {
        match decode_command::<Command>(value) {
            Err(CommandDecodeError::UnknownVariant { field, value }) => Some((field, value)),
            Err(error) => panic!("not an unknown variant: {error}"),
            Ok(_) => None,
        }
    }

    #[test]
    fn commands_name_the_path_of_an_unknown_enum_value_or_tag() {
        let shared = json!({"name": "a"});
        assert_eq!(
            unknown_variant(json!({"shared": shared, "mode": "queue", "documented": ["second"]})),
            None
        );
        assert_eq!(
            unknown_variant(json!({"shared": shared, "mode": "later"})),
            Some(("mode".into(), "later".into()))
        );
        assert_eq!(
            unknown_variant(json!({"shared": shared, "documented": ["first", "third"]})),
            Some(("documented[1]".into(), "third".into()))
        );
        assert_eq!(
            unknown_variant(json!({"shared": shared, "parts": [{"type": "chart"}]})),
            Some(("parts[0].type".into(), "chart".into()))
        );
        assert_eq!(
            unknown_variant(json!({"shared": shared, "part": {"type": "chart", "x": 1}})),
            Some(("part.type".into(), "chart".into()))
        );
        // A known tag with a missing member is malformed, not unknown.
        assert!(matches!(
            decode_command::<Command>(json!({"shared": shared, "parts": [{"type": "text"}]})),
            Err(CommandDecodeError::Invalid(_))
        ));
        // An unknown member inside an optional tagged value is still found.
        assert_eq!(
            unknown(json!({"shared": shared, "part": {"type": "text", "text": "t", "x": 1}}))
                .as_deref(),
            Some("part.x")
        );
        let error = decode_command::<Command>(json!({"shared": shared, "mode": "later"}))
            .unwrap_err()
            .to_rpc_error();
        assert_eq!(error.code, crate::errors::INVALID_PARAMS);
        assert_eq!(
            error.data,
            Some(json!({
                "code": "unknown_variant",
                "hint": "Update the older of the two programs.",
                "field": "mode",
                "value": "later"
            }))
        );
    }

    #[test]
    fn commands_name_the_path_of_an_unknown_member_wherever_it_is() {
        assert_eq!(unknown(json!({"shared": {"name": "a"}})), None);
        assert_eq!(
            unknown(json!({"shared": {"name": "a"}, "extra": 1})).as_deref(),
            Some("extra")
        );
        assert_eq!(
            unknown(json!({"shared": {"name": "a", "extra": 1}})).as_deref(),
            Some("shared.extra")
        );
        assert_eq!(
            unknown(json!({"shared": {"name": "a"}, "optional": {"name": "b", "extra": 1}}))
                .as_deref(),
            Some("optional.extra")
        );
        assert_eq!(
            unknown(json!({"shared": {"name": "a"}, "optional": null})),
            None
        );
        assert_eq!(
            unknown(json!({
                "shared": {"name": "a"},
                "parts": [{"type": "text", "text": "t"}, {"type": "shared", "shared": {"name": "b", "x": 1}}],
            }))
            .as_deref(),
            Some("parts[1].shared.x")
        );
        assert_eq!(
            unknown(
                json!({"shared": {"name": "a"}, "parts": [{"type": "text", "text": "t", "x": 1}]})
            )
            .as_deref(),
            Some("parts[0].x")
        );
        assert_eq!(
            unknown(json!({"shared": {"name": "a"}, "labels": {"k": {"name": "b", "x": 1}}}))
                .as_deref(),
            Some("labels.k.x")
        );
        assert_eq!(
            unknown(json!({"shared": {"name": "a"}, "free": {"anything": {"goes": 1}}})),
            None
        );
    }
}
