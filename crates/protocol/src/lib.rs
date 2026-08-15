//! Exact cookie-agent protocol 9, event schemas 15-17, and session metadata schema 9.
//!
//! This crate intentionally contains no compatibility aliases or decoders.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub(crate) struct NullableSchema<T>(std::marker::PhantomData<T>);

impl<T: JsonSchema> JsonSchema for NullableSchema<T> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Owned(format!("Nullable{}", T::schema_name()))
    }
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        generator.subschema_for::<Option<T>>()
    }
}

pub(crate) fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequiredModelSelectionWire {
    model: ModelKey,
    #[serde(deserialize_with = "deserialize_required_option")]
    variant: Option<VariantId>,
}

pub(crate) fn deserialize_required_model_selection<'de, D>(
    deserializer: D,
) -> Result<ModelSelection, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let wire = RequiredModelSelectionWire::deserialize(deserializer)?;
    Ok(ModelSelection {
        model: wire.model,
        variant: wire.variant,
    })
}

pub(crate) fn deserialize_required_model_selections<'de, D>(
    deserializer: D,
) -> Result<Vec<ModelSelection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::<RequiredModelSelectionWire>::deserialize(deserializer).map(|values| {
        values
            .into_iter()
            .map(|wire| ModelSelection {
                model: wire.model,
                variant: wire.variant,
            })
            .collect()
    })
}

pub(crate) struct RequiredModelSelectionSchema;

impl JsonSchema for RequiredModelSelectionSchema {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        ModelSelection::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = ModelSelection::json_schema(generator);
        schema
            .ensure_object()
            .insert("required".into(), serde_json::json!(["model", "variant"]));
        schema
    }
}

mod agent;
mod approval;
mod bindings;
mod event;
mod identity;
mod journal;
mod manifest;
mod model;
mod provider;
mod rpc;
mod runtime;
mod session;
mod setup_value;

pub use agent::*;
pub use approval::*;
pub use bindings::*;
pub use cookie_agent_identity::{
    AdapterId, AgentId, AgentRevision, AuthFieldName, AuthMethodId, AuthParameterId, AuthRecipeId,
    CacheEntryId, CacheRevision, CanonicalModelId, CatalogRevision, IdentityError, ManifestEntryId,
    ManifestRevision, ModelKey, ModelRevision, ModelSelection, ModelSnapshotRevision,
    ProtocolRecipeId, ProviderId, ProviderModelId, ProviderRecipeId, ProviderSetupRecipeId,
    ProviderStateRevision, ProviderStoreRevision, RecipeCompilerVersion, RecipeRegistryRevision,
    RuntimeRevision, SafeCode, SetupFieldId, StoreEntryId, VariantId, WildcardPattern,
};
pub use event::*;
pub use identity::*;
pub use journal::*;
pub use manifest::*;
pub use model::*;
pub use provider::*;
pub use rpc::*;
pub use runtime::*;
pub use session::*;
pub use setup_value::*;

/// The only protocol version supported by this build.
pub const PROTOCOL_VERSION: u32 = 9;
/// The current durable event/session-JSONL schema written by this build.
pub const EVENT_SCHEMA_VERSION: u32 = 17;
/// The only session metadata schema supported by this build.
pub const SESSION_META_SCHEMA_VERSION: u32 = 9;
/// The current delegation-journal schema written by this build.
pub const DELEGATION_JOURNAL_SCHEMA_VERSION: u32 = 14;
/// The only coherent runtime snapshot schema supported by this build.
pub const RUNTIME_SNAPSHOT_SCHEMA_VERSION: u32 = 4;

/// Returns the TypeScript generation configuration required by this JSON wire.
#[must_use]
pub fn typescript_config() -> ts_rs::Config {
    ts_rs::Config::default().with_large_int("number")
}

macro_rules! exact_numeric_wire_type {
    ($name:ident, $value:expr, $ts:literal, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, TS)]
        #[ts(type = $ts)]
        pub struct $name(());

        impl $name {
            #[must_use]
            pub const fn current() -> Self {
                Self(())
            }

            #[must_use]
            pub const fn value(self) -> u32 {
                $value
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_u32($value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = u32::deserialize(deserializer)?;
                if value == $value {
                    Ok(Self::current())
                } else {
                    Err(serde::de::Error::custom(format!(
                        "unsupported exact wire version {value}; expected {}",
                        $value
                    )))
                }
            }
        }

        impl JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }

            fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
                json_schema!({"type": "integer", "const": $value})
            }
        }
    };
}

exact_numeric_wire_type!(ProtocolVersion, 9, "9", "The exact protocol wire version.");
/// An event/session-JSONL schema accepted by this build.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, TS)]
#[ts(type = "15 | 16 | 17")]
pub struct EventSchemaVersion(u32);

impl EventSchemaVersion {
    #[must_use]
    pub const fn current() -> Self {
        Self(EVENT_SCHEMA_VERSION)
    }

    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl Default for EventSchemaVersion {
    fn default() -> Self {
        Self::current()
    }
}

impl Serialize for EventSchemaVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for EventSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        if matches!(value, 15..=17) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported event schema version {value}; expected 15, 16, or 17"
            )))
        }
    }
}

impl JsonSchema for EventSchemaVersion {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("EventSchemaVersion")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "enum": [15, 16, 17]})
    }
}
exact_numeric_wire_type!(
    SessionMetaSchemaVersion,
    9,
    "9",
    "The exact session metadata schema version."
);
/// A delegation-journal schema accepted by this build.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, TS)]
#[ts(type = "11 | 12 | 13 | 14")]
pub struct DelegationJournalSchemaVersion(u32);

impl DelegationJournalSchemaVersion {
    #[must_use]
    pub const fn current() -> Self {
        Self(DELEGATION_JOURNAL_SCHEMA_VERSION)
    }

    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl Default for DelegationJournalSchemaVersion {
    fn default() -> Self {
        Self::current()
    }
}

impl Serialize for DelegationJournalSchemaVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(self.0)
    }
}

impl<'de> Deserialize<'de> for DelegationJournalSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        if matches!(value, 11..=14) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported delegation-journal version {value}; expected 11, 12, 13, or 14"
            )))
        }
    }
}

impl JsonSchema for DelegationJournalSchemaVersion {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("DelegationJournalSchemaVersion")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "enum": [11, 12, 13, 14]})
    }
}
exact_numeric_wire_type!(
    RuntimeSnapshotSchemaVersion,
    4,
    "4",
    "The exact coherent runtime snapshot schema version."
);

#[cfg(test)]
mod tests;
