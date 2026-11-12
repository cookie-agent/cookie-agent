//! `[public]` and `[share]` (share links spec section 6, renamed by the
//! webhooks spec, decision 5).

use std::fs;

use cookie_agent_config::load_from_user_root;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn every_public_and_share_key_is_read() {
    let directory = root(
        "[share]\nenabled = false\n[public]\nurl = \"https://share.example.com\"\nlisten = \"127.0.0.1:7420\"\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    assert!(!loaded.runtime.share.enabled);
    let public = &loaded.runtime.public;
    assert_eq!(public.url.as_deref(), Some("https://share.example.com"));
    assert_eq!(
        public.listen,
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], 7420)))
    );
    let directory = root("[public]\nlisten = \"[::1]:7420\"\n");
    let loaded = load_from_user_root(Some(directory.path())).expect("an IPv6 address");
    assert!(
        loaded
            .runtime
            .public
            .listen
            .is_some_and(|address| address.is_ipv6())
    );
}

#[test]
fn the_public_url_is_an_http_or_https_url_without_query_or_fragment() {
    for (url, valid) in [
        ("https://share.example.com", true),
        ("https://box.tail1234.ts.net:8443", true),
        ("http://127.0.0.1:7420/", true),
        ("wss://share.example.com", false),
        ("https://share.example.com/?k=v", false),
        ("https://share.example.com/#x", false),
        ("https://me:secret@share.example.com", false),
        ("share.example.com", false),
    ] {
        let authored = root(&format!("[public]\nurl = \"{url}\"\n"));
        assert_eq!(
            load_from_user_root(Some(authored.path())).is_ok(),
            valid,
            "url = {url:?}"
        );
    }
}

#[test]
fn the_public_listener_needs_an_address_with_a_port() {
    for (listen, valid) in [
        ("127.0.0.1:7420", true),
        ("0.0.0.0:1", true),
        ("127.0.0.1:0", false),
        ("127.0.0.1", false),
        ("localhost:7420", false),
        ("7420", false),
    ] {
        let authored = root(&format!("[public]\nlisten = \"{listen}\"\n"));
        assert_eq!(
            load_from_user_root(Some(authored.path())).is_ok(),
            valid,
            "listen = {listen:?}"
        );
    }
}

#[test]
fn unknown_keys_wrong_types_and_the_old_share_address_keys_fail() {
    for config in [
        "[share]\nport = 7420\n",
        "[share]\nenabled = \"yes\"\n",
        // Moved to `[public]`; strict configuration has no alias.
        "[share]\nurl = \"https://share.example.com\"\n",
        "[share]\nlisten = \"127.0.0.1:7420\"\n",
        "[public]\nenabled = true\n",
        "[public]\nlisten = 7420\n",
    ] {
        let authored = root(config);
        assert!(
            load_from_user_root(Some(authored.path())).is_err(),
            "{config}"
        );
    }
}
