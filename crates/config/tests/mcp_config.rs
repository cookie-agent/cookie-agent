use std::fs;

use cookie_agent_config::{
    ConfigError, McpServerConfig, McpServerSource, load_from_roots, write_mcp_server,
};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn mcp_transport_requires_exactly_one_command_or_url() {
    for server in [
        "[mcp.servers.bad]\n",
        "[mcp.servers.bad]\ncommand = \"one\"\nurl = \"https://example.test/mcp\"\n",
    ] {
        let directory = root(server);
        let error = load_from_roots(Some(directory.path()), None).expect_err("invalid MCP server");
        assert!(matches!(error, ConfigError::McpServer { ref server, .. } if server == "bad"));
    }
}

#[test]
fn mcp_transport_specific_fields_and_timeout_are_strict() {
    for server in [
        "command = \"server\"\nheaders = { x = \"y\" }\n",
        "url = \"https://example.test/mcp\"\nargs = [\"bad\"]\n",
        "command = \"server\"\ntimeout_ms = 0\n",
        "url = \"file:///tmp/mcp\"\n",
    ] {
        let directory = root(&format!("[mcp.servers.bad]\n{server}"));
        assert!(matches!(
            load_from_roots(Some(directory.path()), None),
            Err(ConfigError::McpServer { server, .. }) if server == "bad"
        ));
    }
}

#[test]
fn workspace_server_override_retains_workspace_provenance() {
    let user = root("[mcp.servers.github]\ncommand = \"global-server\"\n");
    let workspace = root("[mcp.servers.github]\nurl = \"https://example.test/mcp\"\nlazy = true\n");
    let loaded = load_from_roots(Some(user.path()), Some(workspace.path())).expect("configuration");
    let server = &loaded.mcp_servers["github"];
    assert_eq!(server.source, McpServerSource::WorkspaceFile);
    assert_eq!(
        server.config.url.as_deref(),
        Some("https://example.test/mcp")
    );
    assert!(server.config.lazy);
}

#[test]
fn remote_oauth_accepts_auto_booleans_and_strict_settings() {
    for (oauth, enabled) in [
        ("", true),
        ("oauth = true\n", true),
        ("oauth = false\n", false),
        ("oauth = {}\n", true),
        (
            "oauth = { client_id = \"client\", client_secret = \"oauth-secret-sentinel\", scopes = [\"read\"] }\n",
            true,
        ),
    ] {
        let directory = root(&format!(
            "[mcp.servers.remote]\nurl = \"https://example.test/mcp\"\n{oauth}"
        ));
        let loaded = load_from_roots(Some(directory.path()), None).expect("OAuth config");
        assert_eq!(loaded.mcp_servers["remote"].config.oauth.enabled(), enabled);
        assert!(
            !format!("{:?}", loaded.mcp_servers["remote"].config).contains("oauth-secret-sentinel")
        );
    }

    for oauth in [
        "oauth = { unknown = true }\n",
        "oauth = { client_secret = \"secret\" }\n",
        "oauth = { client_metadata_url = \"http://example.test/client.json\" }\n",
    ] {
        let directory = root(&format!(
            "[mcp.servers.remote]\nurl = \"https://example.test/mcp\"\n{oauth}"
        ));
        assert!(load_from_roots(Some(directory.path()), None).is_err());
    }
}

#[test]
fn write_back_replaces_only_the_named_table_and_round_trips_strictly() {
    let directory = root(
        "# keep this comment\n[tool_output]\nmax_lines = 42\nmax_bytes = 2048\n\n[mcp.servers.demo]\ncommand = \"old\"\n",
    );
    let path = directory.path().join("config.toml");
    let replacement = McpServerConfig {
        command: None,
        args: Vec::new(),
        env: Default::default(),
        cwd: None,
        url: Some("https://example.test/mcp".into()),
        headers: std::collections::BTreeMap::from([("Authorization".into(), "Bearer x".into())]),
        oauth: Default::default(),
        enabled: false,
        lazy: true,
        timeout_ms: Some(5000),
    };
    write_mcp_server(&path, "demo", &replacement).expect("write MCP table");
    let text = fs::read_to_string(&path).expect("written config");
    assert!(text.contains("# keep this comment"));
    assert!(text.contains("max_lines = 42"));
    let loaded = load_from_roots(Some(directory.path()), None).expect("strict round trip");
    assert_eq!(loaded.mcp_servers["demo"].config, replacement);
}

#[test]
fn write_back_rejects_an_existing_strict_conflict_without_changing_the_file() {
    let directory = root("unknown = true\n");
    let path = directory.path().join("config.toml");
    let before = fs::read_to_string(&path).expect("original config");
    let config = McpServerConfig {
        command: Some("server".into()),
        args: Vec::new(),
        env: Default::default(),
        cwd: None,
        url: None,
        headers: Default::default(),
        oauth: Default::default(),
        enabled: true,
        lazy: false,
        timeout_ms: None,
    };
    assert!(write_mcp_server(&path, "demo", &config).is_err());
    assert_eq!(fs::read_to_string(path).expect("unchanged config"), before);
}
