//! The configuration status as clients see it.

use cookie_agent_config::ConfigPath;
use cookie_agent_engine::{ConfigChange, ConfigurationStatus, EngineError, RestartRequiredPart};
use cookie_agent_protocol::{ConfigRestartPart, ConfigStatus};

pub(crate) fn status_to_wire(status: ConfigurationStatus) -> ConfigStatus {
    ConfigStatus {
        applied_revision: status.applied_revision,
        restart_required: status
            .restart_required
            .into_iter()
            .map(|part| match part {
                RestartRequiredPart::Server => ConfigRestartPart::Server,
                RestartRequiredPart::Plugins => ConfigRestartPart::Plugins,
                RestartRequiredPart::Runtimes => ConfigRestartPart::Runtimes,
            })
            .collect(),
        error: status.error,
    }
}

/// A wire change with its path validated.
pub(crate) fn change_from_wire(
    change: cookie_agent_protocol::ConfigChange,
) -> Result<ConfigChange, EngineError> {
    let path = |path: &str| {
        ConfigPath::new(path).map_err(|error| {
            EngineError::ConfigChangeRefused(format!("`{path}` is not a document: {error}"))
        })
    };
    Ok(match change {
        cookie_agent_protocol::ConfigChange::Put {
            path: target,
            text,
            expected_fingerprint,
        } => ConfigChange::Put {
            path: path(&target)?,
            text,
            expected_fingerprint,
        },
        cookie_agent_protocol::ConfigChange::Delete {
            path: target,
            expected_fingerprint,
        } => ConfigChange::Delete {
            path: path(&target)?,
            expected_fingerprint,
        },
        cookie_agent_protocol::ConfigChange::Set { key, value } => {
            if key.is_empty() || key.len() > 32 || key.iter().any(String::is_empty) {
                return Err(EngineError::ConfigChangeRefused(
                    "a key is 1 to 32 nonempty names".into(),
                ));
            }
            ConfigChange::Set { key, value }
        }
    })
}
