//! cookie-agent client protocol 1.21, plugin and runtime protocols, and
//! session events.
//!
//! This crate intentionally contains no compatibility aliases or decoders, except
//! the few that keep versionless session event history readable.
//!
//! It also builds for `wasm32-unknown-unknown`, as the wire types and the
//! [`Client`] only: the server, the stdio peer, [`paths`], and the other
//! native-only parts are left out there. With the `websocket` feature a page
//! connects with `Client::connect_browser_websocket` and authenticates with
//! [`Client::handshake_with_token`].

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::Deserialize;

mod tool_output;
pub use tool_output::*;

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
        let object = schema.ensure_object();
        object.insert("required".into(), serde_json::json!(["model", "variant"]));
        // Stored events and results carry it as information, which readers
        // decode without rejecting unknown members.
        object.remove("additionalProperties");
        schema
    }
}

/// An open snake-case label: a lowercase ASCII letter followed by up to 31
/// lowercase letters, digits, or underscores. Readers accept any such label,
/// including ones this build does not document.
macro_rules! snake_label {
    ($(#[$meta:meta])* $name:ident, $what:literal, $description:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);
        impl $name {
            pub const MAX_LEN: usize = 32;

            pub fn new(value: impl Into<String>) -> Result<Self, $crate::WireStringError> {
                let value = value.into();
                let valid = !value.is_empty()
                    && value.len() <= Self::MAX_LEN
                    && value.bytes().enumerate().all(|(index, byte)| {
                        byte.is_ascii_lowercase()
                            || (index > 0 && (byte.is_ascii_digit() || byte == b'_'))
                    });
                if valid {
                    Ok(Self(value))
                } else {
                    Err($crate::WireStringError::Invalid(concat!(
                        "must be a snake_case ",
                        $what,
                        " of at most 32 bytes"
                    )))
                }
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl ::std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
        impl ::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: ::serde::Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }
        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(::serde::de::Error::custom)
            }
        }
        impl ::schemars::JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }
            fn schema_name() -> ::std::borrow::Cow<'static, str> {
                ::std::borrow::Cow::Borrowed(stringify!($name))
            }
            fn json_schema(_: &mut ::schemars::SchemaGenerator) -> ::schemars::Schema {
                ::schemars::json_schema!({
                    "type": "string",
                    "pattern": "^[a-z][a-z0-9_]{0,31}$",
                    "description": $description
                })
            }
        }
    };
}
pub(crate) use snake_label;

mod acp;
mod agent;
mod approval;
mod attachment;
mod bindings;
mod config;
mod content;
#[cfg(all(any(test, feature = "test-support"), not(target_arch = "wasm32")))]
pub mod corpus;
mod cron;
mod daemon;
pub mod diagnostics;
mod display;
#[cfg(not(target_arch = "wasm32"))]
pub mod editor;
pub mod errors;
mod event;
mod extension;
mod goal;
mod identity;
mod input_lanes;
mod manifest;
pub mod media_sniff;
mod memory;
mod model;
#[cfg(all(feature = "oauth-callback", not(target_arch = "wasm32")))]
pub mod oauth_callback;
mod open;
#[cfg(not(target_arch = "wasm32"))]
pub mod paths;
#[cfg(not(target_arch = "wasm32"))]
pub mod peer;
mod producer;
mod project;
mod provider;
mod rpc;
mod runtime;
pub mod runtime_protocol;
mod semver;
mod session;
mod session_model;
mod setup_value;
#[cfg(not(target_arch = "wasm32"))]
mod stop;
mod stored_display;
pub mod strict;
mod uploads;

pub use acp::*;
pub use agent::*;
pub use approval::*;
pub use attachment::*;
pub use bindings::*;
pub use config::*;
pub use content::*;
pub use cookie_agent_identity::{
    AdapterId, AgentId, AgentRevision, AttachmentName, AuthFieldName, AuthMethodId,
    AuthParameterId, AuthRecipeId, CacheEntryId, CacheRevision, CanonicalModelId, CapabilityName,
    CatalogRevision, ConfigRevision, IdentityError, MachineName, ManifestEntryId, ManifestRevision,
    ModelKey, ModelRevision, ModelSelection, ModelSnapshotRevision, ProjectId, ProjectName,
    ProtocolRecipeId, ProviderId, ProviderModelId, ProviderRecipeId, ProviderSetupRecipeId,
    ProviderStateRevision, ProviderStoreRevision, RecipeCompilerVersion, RecipeRegistryRevision,
    RuntimeId, RuntimeRevision, SafeCode, SetupFieldId, StoreEntryId, VariantId, WildcardPattern,
};
pub use cron::*;
pub use daemon::*;
pub use display::*;
pub use event::*;
pub use extension::*;
pub use goal::*;
pub use identity::*;
pub use input_lanes::*;
pub use manifest::*;
pub use memory::*;
pub use model::*;
pub use open::*;
pub use producer::*;
pub use project::*;
pub use provider::*;
pub use rpc::*;
pub use runtime::*;
pub use runtime_protocol::McpServerScope;
pub use semver::*;
pub use session::*;
pub use session_model::*;
pub use setup_value::*;
#[cfg(not(target_arch = "wasm32"))]
pub use stop::StopSignals;
pub use uploads::*;

// The crate's own tests run natively; on wasm32 the browser tests in
// `tests/browser.rs` cover the client.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod corpus_tests;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod goal_producer_tests;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
