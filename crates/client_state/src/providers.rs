//! The `/connect` panel's rules every client shares: a provider row's
//! state, what it says about the provider's models and the catalog, the
//! public setup values and credentials a connect sends (credentials are
//! serialized once, from buffers the caller wipes, never kept), and the
//! store-contention policy of `provider.connect` and `provider.disconnect`.

use std::collections::BTreeMap;

use cookie_agent_protocol::{
    AuthCredentialDescriptor, AuthMethodDescriptor, AvailableModelDescriptor, CatalogAge,
    CatalogSource, ClientError, EffectiveAuthState, ModelCatalogView, ModelUnavailableKind,
    ProviderConfigurationState, ProviderCredentialValues, ProviderDescriptor, ProviderPresence,
    ProviderSupportState, SafeSetupValue, SetupFieldDescriptor, SetupFieldId,
    UnavailableModelDescriptor, parse_setup_value, setup_value_text,
};
use serde::{Serialize, ser::SerializeMap as _};
use zeroize::Zeroizing;

pub const DURABLE_PROVIDER_COPY: &str =
    "Stored setup, connections, and credentials are per-user and shared across workspaces.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderRowState {
    Unsupported,
    Disconnected,
    ConnectedReconnect,
    Removed,
    ErrorRetry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderAction {
    Connect,
    Reconnect,
    Disconnect,
}

#[derive(Clone, Debug)]
pub enum ProviderOperation {
    InProgress(ProviderAction),
    Error {
        action: ProviderAction,
        message: String,
    },
}

pub fn row_state(
    provider: &ProviderDescriptor,
    _models: &[AvailableModelDescriptor],
    operation: Option<&ProviderOperation>,
) -> ProviderRowState {
    if matches!(operation, Some(ProviderOperation::Error { .. })) {
        return ProviderRowState::ErrorRetry;
    }
    if provider.support.state != ProviderSupportState::Supported {
        return ProviderRowState::Unsupported;
    }
    if provider.presence == ProviderPresence::Removed {
        return ProviderRowState::Removed;
    }
    if provider.durable_connection.is_some() {
        return ProviderRowState::ConnectedReconnect;
    }
    ProviderRowState::Disconnected
}

pub fn authored_override_effective(
    provider: &ProviderDescriptor,
    models: &[AvailableModelDescriptor],
) -> bool {
    provider.configuration == ProviderConfigurationState::Authored
        && provider.effective_auth_state == EffectiveAuthState::AuthoredOverride
        && provider.setup_fields.iter().all(|field| {
            !field.required
                || field.default.is_some()
                || models
                    .iter()
                    .any(|model| model.key.provider_id() == provider.id)
        })
}

/// Whether the user expressed intent to use the provider: an authored config
/// entry or a stored connection. Only these providers list per-model reasons.
pub fn provider_configured(provider: &ProviderDescriptor) -> bool {
    provider.configuration != ProviderConfigurationState::Unconfigured
        || provider.durable_connection.is_some()
}

pub const UNAVAILABLE_KINDS: [ModelUnavailableKind; 4] = [
    ModelUnavailableKind::Quarantined,
    ModelUnavailableKind::Unsupported,
    ModelUnavailableKind::NeedsSetup,
    ModelUnavailableKind::NeedsCredentials,
];

/// Short verb phrase for one unavailable kind, read after a model count.
pub const fn unavailable_kind_label(kind: ModelUnavailableKind) -> &'static str {
    match kind {
        ModelUnavailableKind::Quarantined => "quarantined",
        ModelUnavailableKind::Unsupported => "unsupported",
        ModelUnavailableKind::NeedsSetup => "need setup",
        ModelUnavailableKind::NeedsCredentials => "need credentials",
    }
}

/// One unavailable model's reason, e.g. `quarantined: invalid_catalog_model_record`.
pub fn unavailable_model_reason(model: &UnavailableModelDescriptor) -> String {
    let label = match model.kind {
        ModelUnavailableKind::NeedsSetup => "needs setup",
        ModelUnavailableKind::NeedsCredentials => "needs credentials",
        kind => unavailable_kind_label(kind),
    };
    model
        .reason
        .as_ref()
        .map_or_else(|| label.to_owned(), |reason| format!("{label}: {reason}"))
}

/// The unavailable kinds with nonzero counts, e.g.
/// `4 quarantined (invalid_catalog_model_record) · 2 need credentials`. Up to
/// two distinct listed reasons follow quarantined and unsupported counts.
fn unavailable_parts(provider: &ProviderDescriptor) -> Vec<String> {
    UNAVAILABLE_KINDS
        .into_iter()
        .filter_map(|kind| {
            let count = provider.model_counts.count(kind);
            if count == 0 {
                return None;
            }
            let mut reasons = Vec::<&str>::new();
            for model in &provider.unavailable_models {
                if model.kind == kind
                    && let Some(reason) = &model.reason
                    && !reasons.contains(&reason.as_str())
                {
                    reasons.push(reason.as_str());
                }
            }
            let label = unavailable_kind_label(kind);
            Some(match reasons.len() {
                0 => format!("{count} {label}"),
                1 | 2 => format!("{count} {label} ({})", reasons.join(", ")),
                _ => format!("{count} {label} ({}, …)", reasons[..2].join(", ")),
            })
        })
        .collect()
}

/// Full model availability for the details view, e.g.
/// `3 usable · 4 quarantined (invalid_catalog_model_record)`.
pub fn model_counts_summary(provider: &ProviderDescriptor) -> String {
    let counts = &provider.model_counts;
    if counts.available == 0 && counts.unavailable() == 0 {
        return "no catalog models".to_owned();
    }
    let mut parts = vec![format!("{} usable", counts.available)];
    parts.extend(unavailable_parts(provider));
    parts.join(" · ")
}

/// Why a provider row yields no (or fewer) usable models, when that is news
/// to the user: configured providers always explain unavailable models, and
/// unconfigured providers only when connecting could not help because every
/// model is quarantined or unsupported.
pub fn unusable_models_note(provider: &ProviderDescriptor) -> Option<String> {
    let counts = &provider.model_counts;
    let unavailable = counts.unavailable();
    if unavailable == 0 {
        return None;
    }
    let broken = counts.quarantined.saturating_add(counts.unsupported);
    if provider_configured(provider) {
        if counts.available == 0 {
            return Some(format!(
                "no usable models: {}",
                unavailable_parts(provider).join(" · ")
            ));
        }
        return Some(format!(
            "{unavailable} of {} models unavailable",
            counts.available.saturating_add(unavailable)
        ));
    }
    (counts.available == 0 && broken == unavailable).then(|| {
        format!(
            "no usable models: {}",
            unavailable_parts(provider).join(" · ")
        )
    })
}

/// A config.toml line that supplies the provider's single API key from the
/// first catalog environment variable, offered only when the selected method
/// is exactly one API key. Display-only: cookie never reads the variable
/// unless the user authors this line.
pub fn env_config_hint(
    provider: &ProviderDescriptor,
    auth: Option<&cookie_agent_protocol::AuthMethodDescriptor>,
) -> Option<String> {
    let single_key = auth.is_some_and(|method| {
        method.credentials.len() == 1
            && method.credentials[0].credential_type
                == cookie_agent_protocol::CredentialFieldType::ApiKey
    });
    if !single_key {
        return None;
    }
    let name = provider
        .environment
        .iter()
        .find(|name| name.as_str().contains("KEY"))
        .or_else(|| provider.environment.first())?;
    Some(format!(
        "Or in config.toml: [providers.{}] source = \"models_dev\", api_key = \"${{env:{name}}}\"",
        provider.id
    ))
}

/// One-line catalog provenance: source, age when stale, quarantine counts,
/// and the last refresh error when there is one.
pub fn catalog_status_line(snapshot: &ModelCatalogView) -> String {
    let state = &snapshot.catalog;
    let mut parts = vec![format!(
        "Catalog: {}",
        match snapshot.catalog.source {
            CatalogSource::Network => "models.dev (network)",
            CatalogSource::Cache => "models.dev (cache)",
            CatalogSource::Bootstrap => "bundled bootstrap",
        }
    )];
    match state.age {
        CatalogAge::Current => {}
        CatalogAge::OlderThanSevenDays => parts.push("older than 7 days".to_owned()),
        CatalogAge::OlderThanThirtyDays => parts.push("older than 30 days".to_owned()),
    }
    if state.stale && snapshot.catalog.source == CatalogSource::Cache {
        parts.push("stale".to_owned());
    }
    if state.model_quarantine_count > 0 {
        parts.push(format!(
            "{} quarantined model record{}",
            state.model_quarantine_count,
            if state.model_quarantine_count == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if state.provider_quarantine_count > 0 {
        parts.push(format!(
            "{} quarantined provider record{}",
            state.provider_quarantine_count,
            if state.provider_quarantine_count == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if let Some(error) = &state.last_error {
        parts.push(format!(
            "last refresh failed ({}): {}",
            error.code, error.message
        ));
    }
    parts.join(" · ")
}

/// Whether the catalog line should read as a warning rather than metadata.
pub fn catalog_needs_attention(snapshot: &ModelCatalogView) -> bool {
    snapshot.catalog.last_error.is_some()
        || snapshot.catalog.age != CatalogAge::Current
        || snapshot.catalog.source == CatalogSource::Bootstrap
}

pub const fn action_name(action: ProviderAction) -> &'static str {
    match action {
        ProviderAction::Connect => "connect",
        ProviderAction::Reconnect => "reconnect",
        ProviderAction::Disconnect => "disconnect",
    }
}

/// The auth method a provider's form starts with: its stored connection's,
/// else its first.
pub fn initial_auth_method(provider: &ProviderDescriptor) -> Option<&AuthMethodDescriptor> {
    let id = provider
        .durable_connection
        .as_ref()
        .map(|connection| &connection.auth_method)
        .or_else(|| provider.auth_methods.first().map(|method| &method.id))?;
    provider.auth_methods.iter().find(|method| method.id == *id)
}

/// The text a setup field starts with: the stored connection's value, else
/// the field's default.
pub fn initial_setup_text(provider: &ProviderDescriptor, field: &SetupFieldDescriptor) -> String {
    provider
        .durable_connection
        .as_ref()
        .and_then(|connection| connection.setup_values.get(&field.id))
        .or(field.default.as_ref())
        .map(setup_value_text)
        .unwrap_or_default()
}

/// The public setup values typed into `fields`, parsed, or what is wrong.
pub fn setup_values(
    fields: &[(&SetupFieldDescriptor, &str)],
) -> Result<BTreeMap<SetupFieldId, SafeSetupValue>, String> {
    let mut values = BTreeMap::new();
    for (descriptor, raw) in fields {
        let raw = raw.trim();
        if raw.is_empty() {
            if descriptor.required {
                return Err(format!("{} is required", descriptor.display_name));
            }
            continue;
        }
        values.insert(
            descriptor.id.clone(),
            parse_setup_value(descriptor, raw)
                .map_err(|error| format!("{}: {error}", descriptor.display_name))?,
        );
    }
    Ok(values)
}

/// The credentials typed into `fields`, or what is wrong. They are
/// serialized once into a buffer wiped on drop; empty optional fields are
/// left out.
pub fn credential_values(
    fields: &[(&AuthCredentialDescriptor, &str)],
) -> Result<ProviderCredentialValues, String> {
    for (descriptor, value) in fields {
        if value.is_empty() {
            if descriptor.required {
                return Err(format!("{} is required", descriptor.display_name));
            }
            continue;
        }
        if value.len() > 16 * 1024 {
            return Err(format!("{} is too long", descriptor.display_name));
        }
    }
    let capacity = fields.iter().fold(2usize, |capacity, (descriptor, value)| {
        capacity
            .saturating_add(descriptor.id.as_str().len().saturating_mul(6))
            .saturating_add(value.len().saturating_mul(6))
            .saturating_add(8)
    });
    let mut serialized = Zeroizing::new(Vec::with_capacity(capacity));
    serde_json::to_writer(&mut *serialized, &CredentialProjection(fields))
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&serialized).map_err(|error| error.to_string())
}

struct CredentialProjection<'a>(&'a [(&'a AuthCredentialDescriptor, &'a str)]);

impl Serialize for CredentialProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let populated = self.0.iter().filter(|(_, value)| !value.is_empty()).count();
        let mut map = serializer.serialize_map(Some(populated))?;
        for (descriptor, value) in self.0 {
            if !value.is_empty() {
                map.serialize_entry(descriptor.id.as_str(), value)?;
            }
        }
        map.end()
    }
}

/// Whether the daemon reported retryable store contention
/// (`lock_contention`), covering both the bounded-lock budget and a lost
/// commit-time compare-and-swap.
pub fn is_store_contention(error: &ClientError) -> bool {
    error.is(&cookie_agent_protocol::errors::LOCK_CONTENTION)
}

/// Provider disconnect resubmit policy: the first contention-class failure
/// gets exactly one automatic resubmission of the identical request (same
/// client request id, so the daemon's replay path answers a commit whose
/// response was lost); a second consecutive contention surfaces to the user.
/// Connect follows the same policy through `Client::connect_provider_retrying`.
pub fn retry_store_contention_once(attempt: usize, error: &ClientError) -> bool {
    attempt == 0 && is_store_contention(error)
}

/// The spec's store-contention copy when the daemon reports
/// `lock_contention`; `store` is the human noun for the error site.
pub fn store_contention_message(error: &ClientError, store: &str) -> String {
    if is_store_contention(error) {
        format!("Another cookie-agent process is writing the {store} — try again.")
    } else {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::AuthCredentialDescriptor;

    use super::credential_values;

    fn credential(id: &str, required: bool) -> AuthCredentialDescriptor {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "display_name": id.to_uppercase(),
            "help": "the key",
            "required": required,
            "credential_type": "api_key",
        }))
        .expect("credential")
    }

    #[test]
    fn credentials_need_their_required_fields_and_leave_empty_ones_out() {
        let key = credential("api_key", true);
        let org = credential("organization", false);
        assert_eq!(
            credential_values(&[(&key, ""), (&org, "acme")]).unwrap_err(),
            "API_KEY is required"
        );
        let values = credential_values(&[(&key, "sk-1"), (&org, "")]).expect("values");
        assert_eq!(values.field_names().collect::<Vec<_>>(), ["api_key"]);
        assert_eq!(values.get(&key.id), Some("sk-1"));
    }
}
