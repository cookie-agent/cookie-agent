//! Provider row state, public setup parsing, and connect-form projections.

use std::collections::BTreeMap;

use cookie_agent_protocol::{
    AuthCredentialDescriptor, AuthMethodId, AvailableModelDescriptor, BoundedSetupString,
    EffectiveAuthState, ProviderConfigurationState, ProviderCredentialValues, ProviderDescriptor,
    ProviderPresence, ProviderSupportState, SafeCode, SafeSetupValue, SetupFieldDescriptor,
    SetupFieldId, SetupFieldType, SetupFieldValidation,
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
        ProviderFormFocus::Submit
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
            ProviderFormFocus::Submit => self.focus_count().saturating_sub(1),
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
        usize::from(self.has_auth_selector()) + self.secrets.len() + self.setup.len() + 1
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
                parse_setup_value(raw, &field.descriptor.validation)
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

pub(crate) const fn action_name(action: ProviderAction) -> &'static str {
    match action {
        ProviderAction::Connect => "connect",
        ProviderAction::Reconnect => "reconnect",
        ProviderAction::Disconnect => "disconnect",
    }
}

fn parse_setup_value(
    raw: &str,
    validation: &SetupFieldValidation,
) -> Result<SafeSetupValue, String> {
    validate_length(raw, validation)?;
    match validation.value_type {
        SetupFieldType::String => BoundedSetupString::new(raw.to_owned())
            .map(SafeSetupValue::String)
            .map_err(|error| error.to_string()),
        SetupFieldType::Code => SafeCode::new(raw.to_owned())
            .map(SafeSetupValue::Code)
            .map_err(|error| error.to_string()),
        SetupFieldType::Integer => {
            let value = raw
                .parse::<i64>()
                .map_err(|_| "must be an integer".to_owned())?;
            if validation.minimum.is_some_and(|minimum| value < minimum)
                || validation.maximum.is_some_and(|maximum| value > maximum)
            {
                return Err("is outside the allowed range".into());
            }
            Ok(SafeSetupValue::Integer(value))
        }
        SetupFieldType::Bool => raw
            .parse::<bool>()
            .map(SafeSetupValue::Bool)
            .map_err(|_| "must be true or false".into()),
    }
}

fn validate_length(raw: &str, validation: &SetupFieldValidation) -> Result<(), String> {
    let length = u32::try_from(raw.len()).unwrap_or(u32::MAX);
    if validation
        .min_length
        .is_some_and(|minimum| length < minimum)
    {
        return Err("is shorter than the allowed minimum".into());
    }
    if validation
        .max_length
        .is_some_and(|maximum| length > maximum)
    {
        return Err("is longer than the allowed maximum".into());
    }
    Ok(())
}

fn setup_value_text(value: &SafeSetupValue) -> String {
    match value {
        SafeSetupValue::Bool(value) => value.to_string(),
        SafeSetupValue::Integer(value) => value.to_string(),
        SafeSetupValue::Code(value) => value.to_string(),
        SafeSetupValue::String(value) => value.as_str().to_owned(),
    }
}
