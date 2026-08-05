//! Coherent protocol-8 runtime snapshot state machine.

use cookie_agent_protocol::{
    CatalogSource, RuntimeChangedNotification, RuntimeRevision, RuntimeSnapshotV1,
};

pub const EMPTY_RUNTIME_GUIDANCE: &str = "type /connect to continue";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimePhase {
    Loading,
    Empty,
    Ready,
    Stale,
    Bootstrap,
    ErrorRetry,
}

#[derive(Clone, Debug)]
pub struct RuntimeState {
    snapshot: Option<RuntimeSnapshotV1>,
    phase: RuntimePhase,
    durable_explanation: Option<String>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            snapshot: None,
            phase: RuntimePhase::Loading,
            durable_explanation: None,
        }
    }
}

impl RuntimeState {
    pub fn snapshot(&self) -> Option<&RuntimeSnapshotV1> {
        self.snapshot.as_ref()
    }

    pub const fn phase(&self) -> RuntimePhase {
        self.phase
    }

    pub fn durable_explanation(&self) -> Option<&str> {
        self.durable_explanation.as_deref()
    }

    pub fn revision(&self) -> Option<&RuntimeRevision> {
        self.snapshot
            .as_ref()
            .map(|snapshot| &snapshot.runtime_revision)
    }

    pub fn is_empty(&self) -> bool {
        self.phase == RuntimePhase::Empty
    }

    pub fn install_initial(&mut self, snapshot: RuntimeSnapshotV1) {
        if self.snapshot.is_none() {
            self.install(snapshot);
        }
    }

    /// Install an operation response only if no newer runtime was installed
    /// while that operation was in flight.
    pub fn install_response(
        &mut self,
        baseline: Option<&RuntimeRevision>,
        snapshot: RuntimeSnapshotV1,
    ) -> bool {
        if self.revision() == Some(&snapshot.runtime_revision) {
            return false;
        }
        if self.revision() != baseline {
            return false;
        }
        self.install(snapshot);
        true
    }

    /// Runtime notifications form a predecessor-linked monotonic stream.
    /// Duplicate and stale/out-of-order notifications never replace the
    /// currently installed coherent snapshot.
    pub fn apply_notification(&mut self, changed: RuntimeChangedNotification) -> bool {
        if self.revision() == Some(&changed.snapshot.runtime_revision) {
            return false;
        }
        if let Some(current) = self.revision()
            && changed.previous_revision.as_ref() != Some(current)
        {
            return false;
        }
        self.install(changed.snapshot);
        true
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.durable_explanation = Some(error);
        if self.snapshot.is_none() {
            self.phase = RuntimePhase::ErrorRetry;
        }
    }

    fn install(&mut self, snapshot: RuntimeSnapshotV1) {
        let empty = snapshot.models.is_empty()
            || !snapshot.agents.iter().any(|agent| agent.runnable_as_root);
        let (phase, explanation) = if empty {
            (RuntimePhase::Empty, catalog_explanation(&snapshot))
        } else if snapshot.catalog_source == CatalogSource::Bootstrap {
            (
                RuntimePhase::Bootstrap,
                Some("Using bundled bootstrap catalog; provider and model availability may be limited.".into()),
            )
        } else if snapshot.catalog_state.stale {
            (RuntimePhase::Stale, catalog_explanation(&snapshot))
        } else {
            (RuntimePhase::Ready, None)
        };
        self.snapshot = Some(snapshot);
        self.phase = phase;
        self.durable_explanation = explanation;
    }
}

fn catalog_explanation(snapshot: &RuntimeSnapshotV1) -> Option<String> {
    if snapshot.catalog_source == CatalogSource::Bootstrap {
        return Some(
            "Using bundled bootstrap catalog; provider and model availability may be limited."
                .into(),
        );
    }
    snapshot.catalog_state.last_error.as_ref().map(|error| {
        format!(
            "Using stale catalog cache after {} at {}: {}",
            error.code, error.time, error.message
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_guidance_is_exact() {
        assert_eq!(
            EMPTY_RUNTIME_GUIDANCE.as_bytes(),
            b"type /connect to continue"
        );
    }
}
