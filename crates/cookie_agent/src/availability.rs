//! Plain-text model availability explanations for the headless CLI.

use cookie_agent_protocol::{
    AgentId, CredentialFieldType, ModelKey, ModelUnavailableKind, ProviderConfigurationState,
    ProviderDescriptor, ProviderModelCounts, ProviderSupportState, UnavailableModelDescriptor,
};

const KINDS: [(ModelUnavailableKind, &str); 4] = [
    (ModelUnavailableKind::Quarantined, "quarantined"),
    (ModelUnavailableKind::Unsupported, "unsupported"),
    (ModelUnavailableKind::NeedsSetup, "need setup"),
    (ModelUnavailableKind::NeedsCredentials, "need credentials"),
];

/// Nonzero counts, e.g. `0 usable, 2 quarantined, 1 unsupported`.
#[must_use]
pub fn model_counts_text(counts: &ProviderModelCounts) -> String {
    let mut parts = vec![format!("{} usable", counts.available)];
    for (kind, label) in KINDS {
        let count = counts.count(kind);
        if count > 0 {
            parts.push(format!("{count} {label}"));
        }
    }
    parts.join(", ")
}

/// One unavailable model's reason, e.g. `quarantined: invalid_catalog_model_record`.
#[must_use]
pub fn unavailable_model_text(model: &UnavailableModelDescriptor) -> String {
    let label = match model.kind {
        ModelUnavailableKind::Quarantined => "quarantined",
        ModelUnavailableKind::Unsupported => "unsupported",
        ModelUnavailableKind::NeedsSetup => "needs setup",
        ModelUnavailableKind::NeedsCredentials => "needs credentials",
    };
    model
        .reason
        .as_ref()
        .map_or_else(|| label.to_owned(), |reason| format!("{label}: {reason}"))
}

/// Why a live model without tool calling cannot run `agent`, which publishes
/// tools: `no tool calling: agent `primary` uses tools`.
#[must_use]
pub fn no_tool_calling_text(agent: &AgentId) -> String {
    format!("no tool calling: agent `{agent}` uses tools")
}

/// Why `key` is absent from the live model list, when its provider says so.
#[must_use]
pub fn model_unavailable_reason(
    providers: &[ProviderDescriptor],
    key: &ModelKey,
) -> Option<String> {
    let provider_id = key.provider_id();
    let provider = providers
        .iter()
        .find(|provider| provider.id == provider_id)?;
    if provider.support.state != ProviderSupportState::Supported {
        return Some(provider.support.reason.as_ref().map_or_else(
            || format!("provider `{provider_id}` is not supported"),
            |reason| format!("provider `{provider_id}` is not supported: {reason}"),
        ));
    }
    let model_id = key.model_id();
    if let Some(model) = provider
        .unavailable_models
        .iter()
        .find(|model| model.id == model_id)
    {
        return Some(unavailable_model_text(model));
    }
    (provider.model_counts.available == 0 && provider.model_counts.unavailable() > 0).then(|| {
        format!(
            "provider `{provider_id}` has no usable models ({})",
            model_counts_text(&provider.model_counts)
        )
    })
}

/// Configured providers that yield no usable model, with their counts.
#[must_use]
pub fn unusable_configured_providers(providers: &[ProviderDescriptor]) -> Option<String> {
    let names = providers
        .iter()
        .filter(|provider| {
            (provider.configuration != ProviderConfigurationState::Unconfigured
                || provider.durable_connection.is_some())
                && provider.model_counts.available == 0
                && provider.model_counts.unavailable() > 0
        })
        .map(|provider| {
            format!(
                "{} ({})",
                provider.id,
                model_counts_text(&provider.model_counts)
            )
        })
        .collect::<Vec<_>>();
    (!names.is_empty()).then(|| {
        format!(
            "configured providers without usable models: {}",
            names.join("; ")
        )
    })
}

/// A config.toml alternative that reads the provider's single API key from
/// the first catalog environment variable. Display-only: cookie never reads
/// the variable unless the user authors this line.
#[must_use]
pub fn env_config_hint(provider: &ProviderDescriptor) -> Option<String> {
    let single_key = provider.auth_methods.iter().any(|method| {
        method.credentials.len() == 1
            && method.credentials[0].credential_type == CredentialFieldType::ApiKey
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
        "[providers.{}] source = \"models_dev\", api_key = \"${{env:{name}}}\"",
        provider.id
    ))
}
