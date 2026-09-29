//! Provider row state, public setup parsing, and connect-form projections.

use std::collections::BTreeMap;

use cookie_agent_protocol::{
    AuthCredentialDescriptor, AuthMethodId, AvailableModelDescriptor, CatalogAge, CatalogSource,
    EffectiveAuthState, ModelUnavailableKind, ProviderConfigurationState, ProviderCredentialValues,
    ProviderDescriptor, ProviderPresence, ProviderSupportState, RuntimeSnapshotV1, SafeSetupValue,
    SetupFieldDescriptor, SetupFieldId, UnavailableModelDescriptor, parse_setup_value,
    setup_value_text,
};
use serde::{Serialize, ser::SerializeMap as _};
use zeroize::Zeroizing;

use super::input::CredentialInput;

pub(crate) const DURABLE_PROVIDER_COPY: &str =
    "Stored setup, connections, and credentials are per-user and shared across workspaces.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderRowState {
    Unsupported,
    Disconnected,
    ConnectedReconnect,
    Removed,
    ErrorRetry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderAction {
    Connect,
    Reconnect,
    Disconnect,
}

#[derive(Clone, Debug)]
pub(crate) enum ProviderOperation {
    InProgress(ProviderAction),
    Error {
        action: ProviderAction,
        message: String,
    },
}

pub(crate) struct SetupInput {
    pub(crate) descriptor: SetupFieldDescriptor,
    pub(crate) input: CredentialInput,
}

pub(crate) struct SecretInput {
    pub(crate) descriptor: AuthCredentialDescriptor,
    pub(crate) input: CredentialInput,
}

pub(crate) struct ProviderForm {
    pub(crate) provider: ProviderDescriptor,
    pub(crate) auth_method: AuthMethodId,
    pub(crate) setup: Vec<SetupInput>,
    pub(crate) secrets: Vec<SecretInput>,
    pub(crate) field_index: usize,
    pub(crate) error: Option<String>,
    pub(crate) reconnect: bool,
    pub(crate) can_disconnect: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderFormFocus {
    AuthMethod,
    Credential(usize),
    Setup(usize),
    Submit,
    Cancel,
}

impl ProviderForm {
    pub(crate) fn new(provider: ProviderDescriptor, reconnect: bool) -> Option<Self> {
        let can_disconnect = provider.durable_connection.is_some();
        let auth_method = provider
            .durable_connection
            .as_ref()
            .map(|connection| connection.auth_method.clone())
            .or_else(|| {
                provider
                    .auth_methods
                    .first()
                    .map(|method| method.id.clone())
            })?;
        let selected_auth = provider
            .auth_methods
            .iter()
            .find(|method| method.id == auth_method)?;
        let stored_setup = provider
            .durable_connection
            .as_ref()
            .map(|connection| &connection.setup_values);
        let setup = provider
            .setup_fields
            .iter()
            .cloned()
            .map(|descriptor| {
                let value = descriptor
                    .safe_to_project
                    .then(|| stored_setup.and_then(|values| values.get(&descriptor.id)))
                    .flatten()
                    .or(descriptor.default.as_ref())
                    .map(setup_value_text)
                    .unwrap_or_default();
                let mut input = CredentialInput::default();
                input.set_buffer(value);
                SetupInput { descriptor, input }
            })
            .collect();
        let secrets = selected_auth
            .credentials
            .iter()
            .cloned()
            .map(|descriptor| SecretInput {
                descriptor,
                input: CredentialInput::default(),
            })
            .collect();
        Some(Self {
            provider,
            auth_method,
            setup,
            secrets,
            field_index: 0,
            error: None,
            reconnect,
            can_disconnect,
        })
    }

    pub(crate) fn wipe_secrets(&mut self) {
        for field in &mut self.setup {
            field.input.wipe();
        }
        for secret in &mut self.secrets {
            secret.input.wipe();
        }
    }

    pub(crate) fn wipe_sensitive_values(&mut self) {
        for field in &mut self.setup {
            if !field.descriptor.safe_to_project {
                field.input.wipe();
            }
        }
        for secret in &mut self.secrets {
            secret.input.wipe();
        }
    }

    pub(crate) fn focus(&self) -> ProviderFormFocus {
        let mut index = self.field_index;
        if self.has_auth_selector() {
            if index == 0 {
                return ProviderFormFocus::AuthMethod;
            }
            index -= 1;
        }
        if index < self.secrets.len() {
            return ProviderFormFocus::Credential(index);
        }
        index -= self.secrets.len();
        if index < self.setup.len() {
            return ProviderFormFocus::Setup(index);
        }
        index -= self.setup.len();
        if index == 0 {
            return ProviderFormFocus::Submit;
        }
        ProviderFormFocus::Cancel
    }

    pub(crate) fn move_focus(&mut self, backward: bool) {
        let last = self.focus_count().saturating_sub(1);
        self.field_index = if backward {
            self.field_index.saturating_sub(1)
        } else {
            (self.field_index + 1).min(last)
        };
    }

    /// Focus the field a pointer hit maps to: the inverse of `focus()`,
    /// clamped to the last valid linear index.
    pub(crate) fn set_focus(&mut self, focus: ProviderFormFocus) {
        let offset = usize::from(self.has_auth_selector());
        self.field_index = match focus {
            ProviderFormFocus::AuthMethod => 0,
            ProviderFormFocus::Credential(index) => offset + index,
            ProviderFormFocus::Setup(index) => offset + self.secrets.len() + index,
            ProviderFormFocus::Submit => self.focus_count().saturating_sub(2),
            ProviderFormFocus::Cancel => self.focus_count().saturating_sub(1),
        }
        .min(self.focus_count().saturating_sub(1));
    }

    pub(crate) fn has_auth_selector(&self) -> bool {
        self.provider.auth_methods.len() > 1
    }

    pub(crate) fn selected_auth(&self) -> Option<&cookie_agent_protocol::AuthMethodDescriptor> {
        self.provider
            .auth_methods
            .iter()
            .find(|method| method.id == self.auth_method)
    }

    pub(crate) fn cycle_auth_method(&mut self, backward: bool) {
        if !self.has_auth_selector() {
            return;
        }
        let len = self.provider.auth_methods.len();
        let current = self
            .provider
            .auth_methods
            .iter()
            .position(|method| method.id == self.auth_method)
            .unwrap_or(0);
        let next = if backward {
            (current + len - 1) % len
        } else {
            (current + 1) % len
        };
        self.wipe_auth_values();
        let method = &self.provider.auth_methods[next];
        self.auth_method = method.id.clone();
        self.secrets = method
            .credentials
            .iter()
            .cloned()
            .map(|descriptor| SecretInput {
                descriptor,
                input: CredentialInput::default(),
            })
            .collect();
        self.field_index = 0;
        // Rebuilt credential buffers supersede any stale inline error.
        self.error = None;
    }

    fn focus_count(&self) -> usize {
        // The trailing action row contributes two stops: Submit and Cancel.
        usize::from(self.has_auth_selector()) + self.secrets.len() + self.setup.len() + 2
    }

    fn wipe_auth_values(&mut self) {
        for secret in &mut self.secrets {
            secret.input.wipe();
        }
        self.secrets.clear();
    }

    pub(crate) fn setup_values(&self) -> Result<BTreeMap<SetupFieldId, SafeSetupValue>, String> {
        let mut values = BTreeMap::new();
        for field in &self.setup {
            let raw = field.input.as_str().trim();
            if raw.is_empty() {
                if field.descriptor.required {
                    return Err(format!("{} is required", field.descriptor.display_name));
                }
                continue;
            }
            values.insert(
                field.descriptor.id.clone(),
                parse_setup_value(&field.descriptor, raw)
                    .map_err(|error| format!("{}: {error}", field.descriptor.display_name))?,
            );
        }
        Ok(values)
    }

    pub(crate) fn auth_values(&self) -> Result<ProviderCredentialValues, String> {
        for field in &self.secrets {
            let value = field.input.as_str();
            if value.is_empty() {
                if field.descriptor.required {
                    return Err(format!("{} is required", field.descriptor.display_name));
                }
                continue;
            }
            if value.len() > 16 * 1024 {
                return Err(format!("{} is too long", field.descriptor.display_name));
            }
        }
        let capacity = self.secrets.iter().fold(2usize, |capacity, field| {
            capacity
                .saturating_add(field.descriptor.id.as_str().len().saturating_mul(6))
                .saturating_add(field.input.as_str().len().saturating_mul(6))
                .saturating_add(8)
        });
        let mut serialized = Zeroizing::new(Vec::with_capacity(capacity));
        serde_json::to_writer(&mut *serialized, &CredentialProjection(&self.secrets))
            .map_err(|error| error.to_string())?;
        serde_json::from_slice(&serialized).map_err(|error| error.to_string())
    }
}

struct CredentialProjection<'a>(&'a [SecretInput]);

impl Serialize for CredentialProjection<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let populated = self
            .0
            .iter()
            .filter(|field| !field.input.as_str().is_empty())
            .count();
        let mut map = serializer.serialize_map(Some(populated))?;
        for field in self.0 {
            if !field.input.as_str().is_empty() {
                map.serialize_entry(field.descriptor.id.as_str(), field.input.as_str())?;
            }
        }
        map.end()
    }
}

pub(crate) fn row_state(
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

pub(crate) fn row_label(
    provider: &ProviderDescriptor,
    models: &[AvailableModelDescriptor],
    operation: Option<&ProviderOperation>,
) -> String {
    let state = row_state(provider, models, operation);
    if let Some(ProviderOperation::InProgress(action)) = operation {
        return format!(
            "{} ({}) — {} in progress…",
            provider.display_name,
            provider.id,
            action_name(*action)
        );
    }
    let detail = match state {
        ProviderRowState::Unsupported => provider.support.reason.as_ref().map_or_else(
            || match provider.support.state {
                ProviderSupportState::Quarantined => "quarantined".into(),
                ProviderSupportState::Supported | ProviderSupportState::Unsupported => {
                    "unsupported".into()
                }
            },
            |reason| {
                let state = match provider.support.state {
                    ProviderSupportState::Quarantined => "quarantined",
                    ProviderSupportState::Supported | ProviderSupportState::Unsupported => {
                        "unsupported"
                    }
                };
                if provider.presence == ProviderPresence::Removed {
                    format!("removed · {state}: {reason}")
                } else {
                    format!("{state}: {reason}")
                }
            },
        ),
        ProviderRowState::Disconnected
            if provider.durable_connection.is_none()
                && authored_override_effective(provider, models) =>
        {
            "disconnected · config override active · Enter: create global stored connection".into()
        }
        ProviderRowState::Disconnected => "disconnected".into(),
        ProviderRowState::ConnectedReconnect => "connected · Enter: reconnect/update".into(),
        ProviderRowState::Removed => {
            "removed from current catalog · Enter: reconnect/update".into()
        }
        ProviderRowState::ErrorRetry => match operation {
            Some(ProviderOperation::Error { message, .. }) => {
                format!("error · Enter: retry · {message}")
            }
            _ => "error · Enter: retry".into(),
        },
    };
    let note = matches!(
        state,
        ProviderRowState::Disconnected
            | ProviderRowState::ConnectedReconnect
            | ProviderRowState::Removed
    )
    .then(|| unusable_models_note(provider))
    .flatten();
    // The availability note belongs to the state, ahead of any key hint.
    let detail = match (note, detail.split_once(" · Enter:")) {
        (Some(note), Some((status, action))) => format!("{status} · {note} · Enter:{action}"),
        (Some(note), None) => format!("{detail} · {note}"),
        (None, _) => detail,
    };
    format!("{} ({}) — {detail}", provider.display_name, provider.id)
}

fn authored_override_effective(
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
pub(crate) fn provider_configured(provider: &ProviderDescriptor) -> bool {
    provider.configuration != ProviderConfigurationState::Unconfigured
        || provider.durable_connection.is_some()
}

pub(crate) const UNAVAILABLE_KINDS: [ModelUnavailableKind; 4] = [
    ModelUnavailableKind::Quarantined,
    ModelUnavailableKind::Unsupported,
    ModelUnavailableKind::NeedsSetup,
    ModelUnavailableKind::NeedsCredentials,
];

/// Short verb phrase for one unavailable kind, read after a model count.
pub(crate) const fn unavailable_kind_label(kind: ModelUnavailableKind) -> &'static str {
    match kind {
        ModelUnavailableKind::Quarantined => "quarantined",
        ModelUnavailableKind::Unsupported => "unsupported",
        ModelUnavailableKind::NeedsSetup => "need setup",
        ModelUnavailableKind::NeedsCredentials => "need credentials",
    }
}

/// One unavailable model's reason, e.g. `quarantined: invalid_catalog_model_record`.
pub(crate) fn unavailable_model_reason(model: &UnavailableModelDescriptor) -> String {
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
pub(crate) fn model_counts_summary(provider: &ProviderDescriptor) -> String {
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
pub(crate) fn unusable_models_note(provider: &ProviderDescriptor) -> Option<String> {
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
pub(crate) fn env_config_hint(
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
pub(crate) fn catalog_status_line(snapshot: &RuntimeSnapshotV1) -> String {
    let state = &snapshot.catalog_state;
    let mut parts = vec![format!(
        "Catalog: {}",
        match snapshot.catalog_source {
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
    if state.stale && snapshot.catalog_source == CatalogSource::Cache {
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
pub(crate) fn catalog_needs_attention(snapshot: &RuntimeSnapshotV1) -> bool {
    snapshot.catalog_state.last_error.is_some()
        || snapshot.catalog_state.age != CatalogAge::Current
        || snapshot.catalog_source == CatalogSource::Bootstrap
}

pub(crate) const fn action_name(action: ProviderAction) -> &'static str {
    match action {
        ProviderAction::Connect => "connect",
        ProviderAction::Reconnect => "reconnect",
        ProviderAction::Disconnect => "disconnect",
    }
}
