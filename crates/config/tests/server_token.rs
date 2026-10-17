use std::fs;

use cookie_agent_config::{load_from_user_root, write_server_token};

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
    write_server_token(&path, TOKEN).expect("write the token");
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
    write_server_token(&path, TOKEN).expect("create the file");
    assert!(fs::read_to_string(&path).unwrap().contains(TOKEN));
    assert!(write_server_token(&path, "short").is_err());
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
