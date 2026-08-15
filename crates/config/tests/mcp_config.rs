use std::fs;

use cookie_agent_config::{ConfigError, McpServerSource, load_from_roots};

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
        let directory = root(&format!("schema_version = 10\n{server}"));
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
        let directory = root(&format!("schema_version = 10\n[mcp.servers.bad]\n{server}"));
        assert!(matches!(
            load_from_roots(Some(directory.path()), None),
            Err(ConfigError::McpServer { server, .. }) if server == "bad"
        ));
    }
}

#[test]
fn workspace_server_override_retains_workspace_provenance() {
    let user = root("schema_version = 10\n[mcp.servers.github]\ncommand = \"global-server\"\n");
    let workspace = root(
        "schema_version = 10\n[mcp.servers.github]\nurl = \"https://example.test/mcp\"\nlazy = true\n",
    );
    let loaded = load_from_roots(Some(user.path()), Some(workspace.path())).expect("configuration");
    let server = &loaded.mcp_servers["github"];
    assert_eq!(server.source, McpServerSource::Workspace);
    assert_eq!(
        server.config.url.as_deref(),
        Some("https://example.test/mcp")
    );
    assert!(server.config.lazy);
}
