//! How memory reads out ages and sizes (`docs/site/specs/memory.md`
//! section 16.9.4 and 16.9.7): `3 weeks`, `5 days`, `1.4 KB`, `812 bytes`.
//! The wording lives in the protocol crate, so the clients show ages and
//! sizes the way the model reads them.

pub(crate) use cookie_agent_protocol::{memory_age as age, memory_size as size};
