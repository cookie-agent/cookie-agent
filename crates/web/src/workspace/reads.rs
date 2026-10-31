//! The reads a change notice starts. Every notice reads its listing anew,
//! and the daemon answers a connection's requests concurrently, so an
//! older read's answer can land after a newer one's; a notice can also
//! carry a value newer than a read on its way. Each listing's answers pass
//! through its [`Latest`] here: only the newest read's answer is installed.

use std::collections::HashMap;

use cookie_agent_client_state::refresh::{Latest, Ticket};
use cookie_agent_protocol::SessionId;
use leptos::prelude::*;

use super::Workspace;

/// Every listing's reads.
#[derive(Debug, Default)]
pub(super) struct Reads {
    pub crons: Latest,
    pub memory: Latest,
    pub plugins: Latest,
    pub mcp: Latest,
    /// `runtimes.list` with the `machines.list` read after it.
    pub runtimes: Latest,
    /// How many of those reads are on their way.
    pub runtimes_in_flight: u32,
    pub projects: Latest,
    pub config: Latest,
    pub config_status: Latest,
    pub runtime: Latest,
    pub onboarding: Latest,
    /// The escalated approvals, by root session.
    pub approvals: HashMap<SessionId, Latest>,
    /// The permission modes, by session.
    pub permission_modes: HashMap<SessionId, Latest>,
    /// `/skills`'s listing, of the session it opened on.
    pub skills: Latest,
    /// The `/usage` panel's figures, of the session it opened on.
    pub usage: Latest,
    /// The estimated costs, by session.
    pub costs: HashMap<SessionId, Latest>,
}

impl Workspace {
    /// Starts a read of the listing `which` picks.
    pub(super) fn begin_read(self, which: impl FnOnce(&mut Reads) -> &mut Latest) -> Ticket {
        self.reads
            .try_update_value(|reads| which(reads).begin())
            .expect("the reads live")
    }

    /// Whether `read` is still the newest read of the listing `which`
    /// picks: its answer may be installed.
    pub(super) fn read_is_current(
        self,
        which: impl FnOnce(&mut Reads) -> &mut Latest,
        read: Ticket,
    ) -> bool {
        self.reads
            .try_update_value(|reads| which(reads).is_current(read))
            .unwrap_or(false)
    }

    /// A notice carried a value newer than any read on its way of the
    /// listing `which` picks.
    pub(super) fn supersede_reads(self, which: impl FnOnce(&mut Reads) -> &mut Latest) {
        self.reads.update_value(|reads| which(reads).supersede());
    }
}
