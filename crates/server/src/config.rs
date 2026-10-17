//! The configuration status as clients see it.

use cookie_agent_engine::{ConfigurationStatus, RestartRequiredPart};
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
