//! Strict decoding of commands (`docs/site/specs/compatibility-v1.md`
//! section 4.1).
//!
//! Information types ignore members they do not know, so a type that is
//! also part of a command (a [`crate::RunSelection`] in `run.start`, a
//! [`crate::PermissionRule`] in `session.create`) cannot reject them itself.
//! A command's receiver therefore checks the params against the command's
//! published JSON Schema first: every member of an object must be one of the
//! properties the schema lists for it, wherever it is in the params. Serde
//! then decodes the params as usual.

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
    Invalid(serde_json::Error),
}

/// Decodes `value` as the params of a command of type `T`, refusing any
/// member `T`'s schema does not list.
pub fn decode_command<T>(value: Value) -> Result<T, CommandDecodeError>
where
    T: DeserializeOwned + JsonSchema + 'static,
{
    let schema = command_schema::<T>();
    if let Some(field) = unknown_member(&schema, &value) {
        return Err(CommandDecodeError::UnknownField { field });
    }
    serde_json::from_value(value).map_err(CommandDecodeError::Invalid)
}

/// The JSON path of the first member of `value` that `schema` does not list,
/// or `None` when every member is listed. Members are checked only where the
/// schema lists properties; a free-form object (one that lists none) takes any.
#[must_use]
pub fn unknown_member(schema: &Schema, value: &Value) -> Option<String> {
    let root = schema.as_value();
    let definitions = root.get("$defs").and_then(Value::as_object);
    let mut path = String::new();
    Walker { definitions }.unknown(root, value, &mut path)
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

    fn unknown(&self, schema: &'a Value, value: &Value, path: &mut String) -> Option<String> {
        let schema = self.resolve(schema);
        let branches = ["oneOf", "anyOf"]
            .iter()
            .filter_map(|keyword| schema.get(*keyword).and_then(Value::as_array))
            .flatten()
            .map(|branch| self.resolve(branch))
            .filter(|branch| admits(branch, value))
            .collect::<Vec<_>>();
        if branches.is_empty() {
            return self.unknown_in(&[schema], value, path);
        }
        // One branch the value fits is enough; otherwise report the first.
        let mut first = None;
        for branch in branches {
            let field = self.unknown_in(&[branch, schema], value, path)?;
            first.get_or_insert(field);
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
    ) -> Option<String> {
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
                        (None, None) => Some(path.clone()),
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
            _ => None,
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

/// Whether a branch of a `oneOf` or `anyOf` can describe `value` at all: its
/// type allows the value's, and an object has every required member, with
/// each `const` member (a tag) equal to the branch's.
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
    struct Command {
        shared: Shared,
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
            Err(CommandDecodeError::Invalid(error)) => panic!("invalid params: {error}"),
            Ok(_) => None,
        }
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
