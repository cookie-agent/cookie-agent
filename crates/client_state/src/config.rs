//! The `/config` panel's rules every client shares: the documents as rows,
//! what a new agent document starts from, what `config.apply` and
//! `config.reload` answered, and what the panel says about it.

use cookie_agent_protocol::{
    ClientError, ConfigApplyErrorCode, ConfigApplyResult, ConfigGetResult, ConfigStatus,
};

/// The text a new agent document starts from.
pub const NEW_AGENT: &str = "---\ndescription: \nmode: subagent\nenabled: true\nmodels: [{ model: \"\" }]\npermissions: {}\n---\n";

/// What `config.apply` answered for a change.
#[derive(Debug)]
pub enum ConfigApplyOutcome {
    Applied(ConfigStatus),
    /// `config_apply_refused`: why, and what the daemon says to fix.
    Refused {
        reason: ConfigApplyErrorCode,
        detail: String,
    },
    Failed(String),
}

impl ConfigApplyOutcome {
    pub fn from_result(result: Result<ConfigApplyResult, ClientError>) -> Self {
        match result {
            Ok(result) => Self::Applied(result.status),
            Err(error) if error.is(&cookie_agent_protocol::errors::CONFIG_APPLY_REFUSED) => {
                let data = error.error_data().unwrap_or_default();
                let reason = data
                    .fields
                    .get("reason")
                    .and_then(|reason| serde_json::from_value(reason.clone()).ok())
                    .unwrap_or(ConfigApplyErrorCode::InvalidChange);
                Self::Refused {
                    reason,
                    detail: data.detail.unwrap_or_else(|| error.to_string()),
                }
            }
            Err(error) => Self::Failed(error.to_string()),
        }
    }

    /// What the panel says about the change to `path`.
    pub fn report(&self, path: &str) -> ConfigApplyReport {
        match self {
            Self::Applied(status) => ConfigApplyReport {
                status: format!("Applied {path}.{}", restart_note(status)),
                error: None,
                keep_draft: false,
            },
            Self::Refused {
                reason: ConfigApplyErrorCode::Conflict,
                detail,
            } => ConfigApplyReport {
                status: format!("{path} changed meanwhile; edit it again"),
                error: Some(format!(
                    "{path} was not changed: {detail}. The list is reloaded; edit it again."
                )),
                keep_draft: false,
            },
            Self::Refused { detail, .. } => ConfigApplyReport {
                status: format!("Not applied; edit {path} again."),
                error: Some(format!("{path} was not changed: {detail}")),
                keep_draft: true,
            },
            Self::Failed(error) => ConfigApplyReport {
                status: error.clone(),
                error: None,
                keep_draft: true,
            },
        }
    }
}

/// What the panel shows after an apply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigApplyReport {
    pub status: String,
    /// Shown in the panel until the next apply.
    pub error: Option<String>,
    /// The edit stays, to be fixed and applied again; a conflict drops it,
    /// since the document changed underneath.
    pub keep_draft: bool,
}

/// The status line after `config.reload`.
pub fn reload_status(status: &ConfigStatus) -> String {
    match &status.error {
        Some(error) => format!("Not reloaded: {error}"),
        None => format!("Reloaded the configuration.{}", restart_note(status)),
    }
}

/// The applied configuration's diagnostics, one line each: what it names
/// that does not exist yet, such as a delegation target no runtime has
/// reported.
pub fn diagnostic_lines(status: &ConfigStatus) -> Vec<String> {
    status
        .diagnostics
        .iter()
        .map(|diagnostic| format!("Note: {diagnostic}"))
        .collect()
}

/// What a change needs the daemon restarted for, as a sentence.
pub fn restart_note(status: &ConfigStatus) -> String {
    if status.restart_required.is_empty() {
        return String::new();
    }
    format!(
        " Restart the daemon to apply: {}.",
        crate::restart::restart_parts(status)
    )
}

/// One row: a document, or a preset heading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigRow {
    pub label: String,
    pub path: Option<String>,
    pub preset: Option<String>,
}

/// The rows of `config`: `config.toml`, the agents without a preset, then
/// each preset with its agents.
pub fn config_rows(config: &ConfigGetResult) -> Vec<ConfigRow> {
    let mut rows = vec![ConfigRow {
        label: if config
            .documents
            .iter()
            .any(|document| document.path == "config.toml")
        {
            "config.toml".into()
        } else {
            "config.toml (not created)".into()
        },
        path: Some("config.toml".into()),
        preset: None,
    }];
    let agent = |path: &str| -> Option<(Option<String>, String)> {
        let rest = path.strip_prefix("agents/")?.strip_suffix(".md")?;
        Some(match rest.split_once('/') {
            Some((preset, id)) => (Some(preset.to_owned()), id.to_owned()),
            None => (None, rest.to_owned()),
        })
    };
    let mut presets = std::collections::BTreeMap::<String, Vec<(String, String)>>::new();
    for preset in &config.empty_presets {
        presets.entry(preset.clone()).or_default();
    }
    for document in &config.documents {
        match agent(&document.path) {
            Some((None, id)) => rows.push(ConfigRow {
                label: format!("agent {id}"),
                path: Some(document.path.clone()),
                preset: None,
            }),
            Some((Some(preset), id)) => presets
                .entry(preset)
                .or_default()
                .push((id, document.path.clone())),
            None => {}
        }
    }
    for (preset, agents) in presets {
        rows.push(ConfigRow {
            label: format!("preset {preset}"),
            path: None,
            preset: Some(preset.clone()),
        });
        rows.extend(agents.into_iter().map(|(id, path)| ConfigRow {
            label: format!("  agent {id}"),
            path: Some(path),
            preset: Some(preset.clone()),
        }));
    }
    rows
}

/// Why `path` cannot be a new agent document of `config`, if it cannot.
pub fn new_agent_refusal(config: &ConfigGetResult, path: &str) -> Option<String> {
    if !path.starts_with("agents/") || !path.ends_with(".md") {
        return Some("a new agent is agents/<id>.md or agents/<preset>/<id>.md".into());
    }
    config
        .documents
        .iter()
        .any(|document| document.path == path)
        .then(|| format!("{path} exists; edit it instead"))
}

/// The text a document opens with: the document, else a new agent's
/// template, else nothing.
pub fn document_text(config: &ConfigGetResult, path: &str) -> String {
    config
        .documents
        .iter()
        .find(|document| document.path == path)
        .map(|document| document.text.clone())
        .unwrap_or_else(|| {
            if path.ends_with(".md") {
                NEW_AGENT.to_owned()
            } else {
                String::new()
            }
        })
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{ConfigApplyErrorCode, ConfigGetResult};

    use super::{ConfigApplyOutcome, config_rows, new_agent_refusal};

    fn config() -> ConfigGetResult {
        serde_json::from_value(serde_json::json!({
            "revision": format!("sha256:{}", "a".repeat(64)),
            "status": {"applied_revision": format!("sha256:{}", "a".repeat(64))},
            "documents": [
                {"path": "agents/primary.md", "text": "---\n---\n", "fingerprint": "a"},
                {"path": "agents/fast/helper.md", "text": "---\n---\n", "fingerprint": "b"}
            ],
            "empty_presets": ["empty"]
        }))
        .expect("config")
    }

    #[test]
    fn documents_list_config_toml_then_agents_then_presets() {
        let labels = config_rows(&config())
            .into_iter()
            .map(|row| row.label)
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "config.toml (not created)",
                "agent primary",
                "preset empty",
                "preset fast",
                "  agent helper"
            ]
        );
    }

    #[test]
    fn new_agents_need_an_agent_path_that_is_free() {
        assert!(new_agent_refusal(&config(), "agents/new.md").is_none());
        assert!(new_agent_refusal(&config(), "agents/primary.md").is_some());
        assert!(new_agent_refusal(&config(), "notes.md").is_some());
    }

    #[test]
    fn a_refused_edit_stays_to_be_fixed_and_a_conflict_is_dropped() {
        let refused = ConfigApplyOutcome::Refused {
            reason: ConfigApplyErrorCode::InvalidChange,
            detail: "unknown key `colour`".into(),
        }
        .report("config.toml");
        assert!(refused.keep_draft);
        assert_eq!(
            refused.error.as_deref(),
            Some("config.toml was not changed: unknown key `colour`")
        );
        let conflict = ConfigApplyOutcome::Refused {
            reason: ConfigApplyErrorCode::Conflict,
            detail: "changed".into(),
        }
        .report("config.toml");
        assert!(!conflict.keep_draft);
    }
}
