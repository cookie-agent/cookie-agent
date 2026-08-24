//! Exact cookie-agent protocol 12 with versionless session events.
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
mod extension;
mod identity;
mod manifest;
mod model;
pub mod paths;
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
pub use extension::*;
pub use identity::*;
pub use manifest::*;
pub use model::*;
pub use provider::*;
pub use rpc::*;
pub use runtime::*;
pub use session::*;
pub use setup_value::*;

/// The only protocol version supported by this build.
pub const PROTOCOL_VERSION: u32 = 12;
/// The only coherent runtime snapshot schema supported by this build.
pub const RUNTIME_SNAPSHOT_SCHEMA_VERSION: u32 = 5;

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

exact_numeric_wire_type!(
    ProtocolVersion,
    12,
    "12",
    "The exact protocol wire version."
);
exact_numeric_wire_type!(
    RuntimeSnapshotSchemaVersion,
    5,
    "5",
    "The exact coherent runtime snapshot schema version."
);

#[cfg(test)]
mod tests;
