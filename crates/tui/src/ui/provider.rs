//! Provider row state, public setup parsing, and connect-form projections.

use std::collections::BTreeMap;

use cookie_agent_client_state::providers::{credential_values, setup_values};
use cookie_agent_protocol::{
    AuthCredentialDescriptor, AuthMethodId, AvailableModelDescriptor, ProviderCredentialValues,
    ProviderDescriptor, ProviderPresence, ProviderSupportState, SafeSetupValue,
    SetupFieldDescriptor, SetupFieldId, setup_value_text,
};

use super::input::CredentialInput;

pub(crate) use cookie_agent_client_state::providers::{
    DURABLE_PROVIDER_COPY, ProviderAction, ProviderOperation, ProviderRowState, action_name,
    authored_override_effective, catalog_needs_attention, catalog_status_line, env_config_hint,
    model_counts_summary, row_state, unavailable_model_reason, unusable_models_note,
};

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
                let value = stored_setup
                    .and_then(|values| values.get(&descriptor.id))
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
        let fields = self
            .setup
            .iter()
            .map(|field| (&field.descriptor, field.input.as_str()))
            .collect::<Vec<_>>();
        setup_values(&fields)
    }

    pub(crate) fn auth_values(&self) -> Result<ProviderCredentialValues, String> {
        let fields = self
            .secrets
            .iter()
            .map(|field| (&field.descriptor, field.input.as_str()))
            .collect::<Vec<_>>();
        credential_values(&fields)
    }
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
