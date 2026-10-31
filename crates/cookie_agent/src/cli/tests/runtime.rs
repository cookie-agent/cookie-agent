//! `cookie runtime`.

use super::*;

#[test]
fn a_dial_in_runtime_defaults_to_its_host_name() {
    for (host, expected) in [
        ("labpc", "labpc"),
        ("LabPC.local", "labpc_local"),
        ("1box", "host_1box"),
        ("", "host"),
    ] {
        let id = host_runtime_id("", host);
        assert_eq!(id, expected, "{host}");
        cookie_agent_protocol::RuntimeId::new(id.as_str()).expect("a valid runtime ID");
    }
}
