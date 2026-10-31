//! Machines (`docs/site/specs/machines-on-demand.md`): what the engine
//! serves about each runtime's machine, attaching machines to a run,
//! and where a `transfer` reads and writes.

pub(crate) mod api;
pub(crate) mod attach;
pub(crate) mod transfers;
