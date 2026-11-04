use std::fs;

use cookie_agent_config::{load_from_user_root, write_server_tokens};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA-_0";

#[test]
fn the_server_token_is_read_from_the_server_table() {
    let authored = root(&format!("[server]\ntoken = \"{TOKEN}\"\n"));
    let loaded = load_from_user_root(Some(authored.path())).expect("authored");
    assert_eq!(loaded.runtime.server.token.as_deref(), Some(TOKEN));
    let unset = root("");
    let loaded = load_from_user_root(Some(unset.path())).expect("no server table");
    assert_eq!(loaded.runtime.server.token, None);
    assert!(!format!("{:?}", loaded.runtime.server).contains("AAAA"));
}

#[test]
fn the_server_token_must_be_43_base64url_characters() {
    for (token, valid) in [
        (TOKEN.to_owned(), true),
        (TOKEN[..42].to_owned(), false),
        (format!("{TOKEN}A"), false),
        (format!("{}+", &TOKEN[..42]), false),
        (String::new(), false),
    ] {
        let authored = root(&format!("[server]\ntoken = \"{token}\"\n"));
        assert_eq!(
            load_from_user_root(Some(authored.path())).is_ok(),
            valid,
            "token = {token:?}"
        );
    }
}

#[test]
fn writing_the_server_token_keeps_the_rest_of_the_file() {
    let directory =
        root("# my settings\n[server]\nport = 7420\n\n[producers]\nmax_body_bytes = 1024\n");
    let path = directory.path().join("config.toml");
    write_server_tokens(&path, Some(TOKEN), None).expect("write the token");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("# my settings"), "{text}");
    let loaded = load_from_user_root(Some(directory.path())).expect("reload");
    assert_eq!(loaded.runtime.server.token.as_deref(), Some(TOKEN));
    assert_eq!(loaded.runtime.server.port, 7420);
    assert_eq!(loaded.runtime.producers.max_body_bytes, 1024);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // A file that does not exist yet is created; an invalid token is refused.
    let empty = tempfile::tempdir().unwrap();
    let path = empty.path().join("config.toml");
    write_server_tokens(&path, Some(TOKEN), None).expect("create the file");
    assert!(fs::read_to_string(&path).unwrap().contains(TOKEN));
    assert!(write_server_tokens(&path, Some("short"), None).is_err());
    assert!(write_server_tokens(&path, None, Some("short")).is_err());
}

const RUNTIME_TOKEN: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB-_0";

#[test]
fn the_runtime_token_is_a_second_token_unlike_the_client_one() {
    let authored = root(&format!(
        "[server]\ntoken = \"{TOKEN}\"\nruntime_token = \"{RUNTIME_TOKEN}\"\n"
    ));
    let loaded = load_from_user_root(Some(authored.path())).expect("both tokens");
    assert_eq!(
        loaded.runtime.server.runtime_token.as_deref(),
        Some(RUNTIME_TOKEN)
    );
    assert!(!format!("{:?}", loaded.runtime.server).contains("BBBB"));
    let short = root("[server]\nruntime_token = \"short\"\n");
    assert!(load_from_user_root(Some(short.path())).is_err());
    let same = root(&format!(
        "[server]\ntoken = \"{TOKEN}\"\nruntime_token = \"{TOKEN}\"\n"
    ));
    let error = load_from_user_root(Some(same.path()))
        .expect_err("the same token twice")
        .to_string();
    assert!(error.contains("must differ"), "{error}");
}

#[test]
fn writing_the_runtime_token_keeps_the_client_token() {
    let directory = root(&format!("[server]\ntoken = \"{TOKEN}\"\n"));
    let path = directory.path().join("config.toml");
    write_server_tokens(&path, None, Some(RUNTIME_TOKEN)).expect("write the runtime token");
    let loaded = load_from_user_root(Some(directory.path())).expect("reload");
    assert_eq!(loaded.runtime.server.token.as_deref(), Some(TOKEN));
    assert_eq!(
        loaded.runtime.server.runtime_token.as_deref(),
        Some(RUNTIME_TOKEN)
    );
}

#[test]
fn the_daemon_url_defaults_to_the_daemon_this_configuration_describes() {
    let unset = root("[server]\nport = 7420\n");
    let loaded = load_from_user_root(Some(unset.path())).expect("default url");
    assert_eq!(loaded.runtime.server.url, None);
    assert_eq!(loaded.runtime.server.client_url(), "ws://127.0.0.1:7420/ws");
    let remote = root("[server]\nurl = \"wss://laptop.example:7419/ws\"\n");
    let loaded = load_from_user_root(Some(remote.path())).expect("remote url");
    assert_eq!(
        loaded.runtime.server.client_url(),
        "wss://laptop.example:7419/ws"
    );
    // The local daemon is the one host and port describe, whatever `url`
    // names.
    let both = root(
        "[server]\nhost = \"localhost\"\nport = 7420\nurl = \"wss://laptop.example:7419/ws\"\n",
    );
    let loaded = load_from_user_root(Some(both.path())).expect("both");
    assert_eq!(loaded.runtime.server.local_url(), "ws://localhost:7420/ws");
}

#[test]
fn the_daemon_url_names_a_websocket_client_endpoint() {
    for (url, valid) in [
        ("ws://127.0.0.1:7419/ws", true),
        ("wss://laptop.example/ws", true),
        ("http://laptop.example/ws", false),
        ("ws://laptop.example/other", false),
        ("ws://laptop.example/ws?id=x", false),
        ("ws://user:pass@laptop.example/ws", false),
        ("not a url", false),
    ] {
        let authored = root(&format!("[server]\nurl = \"{url}\"\n"));
        assert_eq!(
            load_from_user_root(Some(authored.path())).is_ok(),
            valid,
            "url = {url:?}"
        );
    }
}

#[test]
fn the_server_table_has_no_origin_allowlist() {
    // `/ws` takes any web origin, so `[server]` names none: the removed
    // `allowed_origins` is an unknown key like any other.
    let removed = root("[server]\nallowed_origins = [\"*\"]\n");
    let error = load_from_user_root(Some(removed.path()))
        .expect_err("allowed_origins is no key")
        .to_string();
    assert!(error.contains("allowed_origins"), "{error}");
}

#[test]
fn the_page_url_is_an_http_or_https_url_without_query_or_fragment() {
    let unset = root("[server]\nport = 7420\n");
    let loaded = load_from_user_root(Some(unset.path())).expect("no page url");
    assert_eq!(loaded.runtime.server.page_url, None);
    for (url, valid) in [
        ("https://cookie.example", true),
        ("http://127.0.0.1:7419/", true),
        ("https://example.com/cookie/", true),
        ("wss://cookie.example/ws", false),
        ("https://cookie.example/?token=x", false),
        ("https://cookie.example/#token=x", false),
        ("https://me:secret@cookie.example", false),
        ("cookie.example", false),
        ("", false),
    ] {
        let authored = root(&format!("[server]\npage_url = \"{url}\"\n"));
        let loaded = load_from_user_root(Some(authored.path()));
        assert_eq!(loaded.is_ok(), valid, "page_url = {url:?}");
        if valid {
            assert_eq!(
                loaded.unwrap().runtime.server.page_url.as_deref(),
                Some(url)
            );
        }
    }
    let wrong_type = root("[server]\npage_url = 7\n");
    assert!(load_from_user_root(Some(wrong_type.path())).is_err());
}
