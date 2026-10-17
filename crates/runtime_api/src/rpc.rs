//! The JSON-RPC peer and line transport the runtime protocol runs over,
//! which live in [`cookie_agent_protocol::peer`].

pub use cookie_agent_protocol::peer::*;

#[cfg(test)]
mod tests;
