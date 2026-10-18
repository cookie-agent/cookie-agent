//! Open enums, as specified in `docs/site/specs/compatibility-v1.md`
//! section 4.2: wire enums whose readers accept values they do not know.
//!
//! - **Tagged enums with data** are read through [`Open<T>`]: either
//!   [`Open::Known`] or [`Open::Unknown`], which keeps the tag and the raw
//!   JSON so the value can be passed on or written back unchanged.
//!   [`open_enum!`](crate::open_enum) implements [`OpenTagged`] for the enum.
//! - **Unit enums** gain an `Unknown` variant through the unit form of
//!   [`open_enum!`](crate::open_enum).
//!
//! Every schema of an open enum carries `"x-open": true`, which the
//! compatibility checker reads: growing an open enum is a minor change,
//! growing a closed one a major change.
//!
//! **A known tag with a broken body is an error, not `Unknown`.** `Unknown`
//! means "a value from a newer peer"; a malformed value of a known kind is a
//! bug in its writer. A tolerant reader that must survive malformed data
//! (such as the session history loader) handles that error itself, at the
//! level where it knows what to skip.

use std::{borrow::Cow, collections::BTreeSet};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

/// The schema member that marks an enum open.
pub const OPEN_SCHEMA_KEY: &str = "x-open";

/// An internally tagged enum that [`Open`] can read: implemented by
/// [`open_enum!`](crate::open_enum).
pub trait OpenTagged: Serialize + DeserializeOwned + JsonSchema {
    /// The member that carries the tag, such as `type`.
    const TAG: &'static str;

    /// Every tag this build knows, read once from the enum's schema.
    fn known_tags() -> &'static BTreeSet<String>;

    /// The tag of a known value.
    fn tag(&self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|value| value.get(Self::TAG)?.as_str().map(str::to_owned))
            .unwrap_or_default()
    }
}

/// A value of an open tagged enum: a variant this build knows, or one from a
/// newer peer kept as raw JSON.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Open<T> {
    Known(T),
    /// A tag this build does not know. `raw` is the whole value, the tag
    /// included, exactly as read.
    Unknown {
        tag: String,
        raw: Value,
    },
}

impl<T> Open<T> {
    #[must_use]
    pub const fn known(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unknown { .. } => None,
        }
    }

    #[must_use]
    pub fn into_known(self) -> Option<T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unknown { .. } => None,
        }
    }

    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }
}

impl<T: OpenTagged> Open<T> {
    /// The value's tag, known or not.
    #[must_use]
    pub fn tag(&self) -> Cow<'_, str> {
        match self {
            Self::Known(value) => Cow::Owned(value.tag()),
            Self::Unknown { tag, .. } => Cow::Borrowed(tag),
        }
    }
}

impl<T> From<T> for Open<T> {
    fn from(value: T) -> Self {
        Self::Known(value)
    }
}

impl<T: Serialize> Serialize for Open<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Known(value) => value.serialize(serializer),
            Self::Unknown { raw, .. } => raw.serialize(serializer),
        }
    }
}

impl<'de, T: OpenTagged> Deserialize<'de> for Open<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Value::deserialize(deserializer)?;
        let tag = match raw.get(T::TAG) {
            Some(Value::String(tag)) => tag.clone(),
            Some(_) => {
                return Err(serde::de::Error::custom(format_args!(
                    "the `{}` member must be a string",
                    T::TAG
                )));
            }
            None => return Err(serde::de::Error::missing_field(T::TAG)),
        };
        if T::known_tags().contains(&tag) {
            T::deserialize(raw)
                .map(Self::Known)
                .map_err(serde::de::Error::custom)
        } else {
            Ok(Self::Unknown { tag, raw })
        }
    }
}

impl<T: OpenTagged> JsonSchema for Open<T> {
    fn inline_schema() -> bool {
        T::inline_schema()
    }

    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }

    fn schema_id() -> Cow<'static, str> {
        Cow::Owned(format!("Open<{}>", T::schema_id()))
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = T::json_schema(generator);
        mark_open(&mut schema);
        schema
    }
}

/// Marks `schema` as the schema of an open enum.
pub fn mark_open(schema: &mut Schema) {
    schema
        .ensure_object()
        .insert(OPEN_SCHEMA_KEY.into(), Value::Bool(true));
}

/// The tags an internally tagged enum's schema declares: the `const` (or
/// single `enum` value) of the tag member of each `oneOf`/`anyOf` variant.
#[must_use]
pub fn schema_tags<T: JsonSchema>(tag: &str) -> BTreeSet<String> {
    let schema = schemars::schema_for!(T);
    let mut tags = BTreeSet::new();
    collect_tags(schema.as_value(), tag, &mut tags);
    tags
}

fn collect_tags(schema: &Value, tag: &str, tags: &mut BTreeSet<String>) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(value) = object
        .get("properties")
        .and_then(|properties| properties.get(tag))
        .and_then(tag_value)
    {
        tags.insert(value.to_owned());
    }
    for key in ["oneOf", "anyOf", "allOf"] {
        for variant in object
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            collect_tags(variant, tag, tags);
        }
    }
}

fn tag_value(schema: &Value) -> Option<&str> {
    if let Some(value) = schema.get("const") {
        return value.as_str();
    }
    match schema.get("enum")?.as_array()?.as_slice() {
        [value] => value.as_str(),
        _ => None,
    }
}

/// Derives the glue that makes an enum open.
///
/// **Tagged enums with data** get an [`OpenTagged`] impl; read and write
/// them as [`Open<T>`]. The known tags come from the enum's own schema, so
/// they never drift from its serde attributes:
///
/// ```
/// # use cookie_agent_protocol::{Open, open_enum};
/// #[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
/// #[serde(tag = "type", rename_all = "snake_case")]
/// enum Shape {
///     Circle { radius: u32 },
///     Square { side: u32 },
/// }
/// open_enum!(tagged Shape, tag = "type");
///
/// let future: Open<Shape> =
///     serde_json::from_str(r#"{"type":"hexagon","side":2}"#).unwrap();
/// assert!(future.is_unknown());
/// ```
///
/// **Unit enums** are declared inside the macro, which adds an `Unknown`
/// variant read for any value this build does not know
/// (`#[serde(other)]`) and marks the schema `x-open`. `Unknown` is left out
/// of the schema and serializes as `"unknown"`; writers never send it. The
/// enum must derive `Deserialize`, `Serialize`, and `JsonSchema`, and
/// document what `Unknown` means where it is read:
///
/// ```
/// # use cookie_agent_protocol::open_enum;
/// open_enum! {
///     #[derive(Clone, Copy, Debug, Eq, PartialEq)]
///     #[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
///     #[serde(rename_all = "snake_case")]
///     pub enum Weather {
///         Sunny,
///         Rainy,
///     }
/// }
/// let future: Weather = serde_json::from_str(r#""foggy""#).unwrap();
/// assert_eq!(future, Weather::Unknown);
/// ```
#[macro_export]
macro_rules! open_enum {
    (tagged $name:ty, tag = $tag:literal) => {
        impl $crate::OpenTagged for $name {
            const TAG: &'static str = $tag;

            fn known_tags() -> &'static ::std::collections::BTreeSet<::std::string::String> {
                static TAGS: ::std::sync::OnceLock<
                    ::std::collections::BTreeSet<::std::string::String>,
                > = ::std::sync::OnceLock::new();
                TAGS.get_or_init(|| $crate::schema_tags::<$name>($tag))
            }
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[schemars(extend("x-open" = true))]
        $vis enum $name {
            $($(#[$variant_meta])* $variant,)+
            /// A value this build does not know, from a newer peer.
            #[serde(other)]
            #[schemars(skip)]
            Unknown,
        }
    };
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
    enum Block {
        Text {
            text: String,
        },
        /// A documented variant, which schemars renders differently.
        Rule,
        Pair(Pair),
    }

    #[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    struct Pair {
        left: u32,
        right: u32,
    }

    open_enum!(tagged Block, tag = "type");

    open_enum! {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
        #[serde(rename_all = "snake_case")]
        enum State {
            /// Running.
            Running,
            Stopped,
        }
    }

    #[test]
    fn known_tags_come_from_the_schema() {
        let tags = Block::known_tags();
        assert_eq!(
            tags.iter().map(String::as_str).collect::<Vec<_>>(),
            ["pair", "rule", "text"]
        );
    }

    #[test]
    fn a_known_value_round_trips() {
        for value in [
            json!({"type": "text", "text": "hello"}),
            json!({"type": "rule"}),
            json!({"type": "pair", "left": 1, "right": 2}),
        ] {
            let open: Open<Block> = serde_json::from_value(value.clone()).unwrap();
            assert!(!open.is_unknown());
            assert_eq!(serde_json::to_value(&open).unwrap(), value);
        }
        let open: Open<Block> = serde_json::from_value(json!({"type": "rule"})).unwrap();
        assert_eq!(open.known(), Some(&Block::Rule));
        assert_eq!(open.tag(), "rule");
    }

    #[test]
    fn an_unknown_tag_is_kept_raw_and_written_back_unchanged() {
        let value = json!({
            "type": "chart",
            "series": [{"x": 1, "y": [1.5, null]}],
            "nested": {"z": "a", "a": true}
        });
        let open: Open<Block> = serde_json::from_value(value.clone()).unwrap();
        let Open::Unknown { tag, raw } = &open else {
            panic!("expected unknown: {open:?}");
        };
        assert_eq!(tag, "chart");
        assert_eq!(raw, &value);
        assert_eq!(open.tag(), "chart");
        let written = serde_json::to_string(&open).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&written).unwrap(), value);
        assert_eq!(written, serde_json::to_string(&value).unwrap());
    }

    #[test]
    fn a_known_tag_with_a_broken_body_is_an_error() {
        for value in [
            json!({"type": "text"}),
            json!({"type": "text", "text": 7}),
            json!({"type": "text", "text": "x", "extra": 1}),
            json!({"type": "pair", "left": 1}),
        ] {
            assert!(
                serde_json::from_value::<Open<Block>>(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn a_value_without_a_string_tag_is_an_error() {
        for value in [
            json!({"text": "x"}),
            json!({"type": 3}),
            json!("text"),
            json!(null),
        ] {
            assert!(
                serde_json::from_value::<Open<Block>>(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn open_schemas_are_marked() {
        let schema = schemars::schema_for!(Open<Block>);
        assert_eq!(schema.get(OPEN_SCHEMA_KEY), Some(&json!(true)));
        let closed = schemars::schema_for!(Block);
        assert_eq!(closed.get(OPEN_SCHEMA_KEY), None);

        let unit = schemars::schema_for!(State);
        assert_eq!(unit.get(OPEN_SCHEMA_KEY), Some(&json!(true)));
        assert!(!unit.as_value().to_string().contains("unknown"), "{unit:?}");
    }

    #[test]
    fn open_unit_enums_read_unknown_values() {
        assert_eq!(
            serde_json::from_value::<State>(json!("running")).unwrap(),
            State::Running
        );
        assert_eq!(
            serde_json::from_value::<State>(json!("paused")).unwrap(),
            State::Unknown
        );
        assert_eq!(
            serde_json::to_value(State::Stopped).unwrap(),
            json!("stopped")
        );
        assert!(serde_json::from_value::<State>(json!(1)).is_err());
    }
}
