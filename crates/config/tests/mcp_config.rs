use std::fs;

use cookie_agent_config::{
    ConfigError, ConfigTree, McpServerConfig, McpServerSource, load_from_tree, load_from_user_root,
    write_tree,
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
        let error = load_from_user_root(Some(directory.path())).expect_err("invalid MCP server");
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
            load_from_user_root(Some(directory.path())),
            Err(ConfigError::McpServer { server, .. }) if server == "bad"
        ));
    }
}

#[test]
fn removed_lazy_setting_is_an_unknown_field() {
    for lazy in ["lazy = true\n", "lazy = false\n"] {
        let directory = root(&format!("[mcp.servers.old]\ncommand = \"server\"\n{lazy}"));
        let error = load_from_user_root(Some(directory.path())).expect_err("lazy is not a field");
        assert!(error.to_string().contains("lazy"), "{error}");
    }
}

#[test]
fn user_servers_have_user_file_provenance() {
    let user = root("[mcp.servers.github]\nurl = \"https://example.test/mcp\"\n");
    let loaded = load_from_user_root(Some(user.path())).expect("configuration");
    let server = &loaded.mcp_servers["github"];
    assert_eq!(server.source, McpServerSource::UserFile);
    assert_eq!(
        server.config.url.as_deref(),
        Some("https://example.test/mcp")
    );
    assert_eq!(
        loaded.user_mcp_servers.keys().collect::<Vec<_>>(),
        ["github"]
    );
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
        let loaded = load_from_user_root(Some(directory.path())).expect("OAuth config");
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
        assert!(load_from_user_root(Some(directory.path())).is_err());
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
        timeout_ms: Some(5000),
        scope: None,
        include_tools: Some(vec!["browser_*".into()]),
        exclude_tools: vec!["browser_install".into()],
    };
    let original = ConfigTree::read(Some(directory.path())).expect("read the tree");
    let mut candidate = original.clone();
    candidate
        .set_mcp_server("demo", Some(&replacement))
        .expect("set the MCP table");
    load_from_tree(&candidate).expect("a valid candidate");
    write_tree(&original, &candidate).expect("write MCP table");
    let text = fs::read_to_string(&path).expect("written config");
    assert!(text.contains("# keep this comment"));
    assert!(text.contains("max_lines = 42"));
    let loaded = load_from_user_root(Some(directory.path())).expect("strict round trip");
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
        timeout_ms: None,
        scope: None,
        include_tools: None,
        exclude_tools: Vec::new(),
    };
    let mut candidate = ConfigTree::read(Some(directory.path())).expect("read the tree");
    candidate
        .set_mcp_server("demo", Some(&config))
        .expect("set the MCP table");
    assert!(load_from_tree(&candidate).is_err());
    assert_eq!(fs::read_to_string(path).expect("unchanged config"), before);
}

#[test]
fn tool_filters_load_and_decide_which_listed_tools_are_published() {
    let directory = root(
        "[mcp.servers.my_chrome]\nurl = \"https://example.test/mcp\"\n\
         include_tools = [\"browser_*\", \"tab_?\"]\nexclude_tools = [\"browser_install\"]\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("filters");
    let config = &loaded.mcp_servers["my_chrome"].config;
    assert_eq!(
        config.include_tools.as_deref(),
        Some(&["browser_*".to_owned(), "tab_?".to_owned()][..])
    );
    assert_eq!(config.exclude_tools, ["browser_install"]);
    for (tool, published) in [
        ("browser_click", true),
        ("tab_1", true),
        ("tab_12", false),
        ("browser_install", false),
        ("screenshot", false),
    ] {
        assert_eq!(config.publishes_tool(tool), published, "{tool}");
    }
}

#[test]
fn absent_filters_publish_every_tool_and_stay_absent_when_written() {
    let directory = root("[mcp.servers.docs]\ncommand = \"docs-mcp\"\n");
    let loaded = load_from_user_root(Some(directory.path())).expect("no filters");
    let config = &loaded.mcp_servers["docs"].config;
    assert!(config.publishes_tool("anything"));
    let table = toml::Value::try_from(config).expect("table");
    assert!(table.get("include_tools").is_none());
    assert!(table.get("exclude_tools").is_none());
}

#[test]
fn empty_tool_filter_patterns_and_an_empty_include_list_are_errors() {
    for (filters, expected) in [
        ("include_tools = []\n", "`include_tools` is empty"),
        ("include_tools = [\"\"]\n", "patterns may not be empty"),
        (
            "exclude_tools = [\"browser_*\", \"\"]\n",
            "patterns may not be empty",
        ),
    ] {
        let directory = root(&format!(
            "[mcp.servers.bad]\ncommand = \"server\"\n{filters}"
        ));
        let error = load_from_user_root(Some(directory.path())).expect_err(filters);
        assert!(
            matches!(&error, ConfigError::McpServer { server, reason } if server == "bad" && reason.contains(expected)),
            "{filters}: {error:?}"
        );
    }
    // An empty exclude list excludes nothing.
    let directory = root("[mcp.servers.ok]\ncommand = \"server\"\nexclude_tools = []\n");
    load_from_user_root(Some(directory.path())).expect("empty exclude list");
    // The filters are lists of strings.
    let directory =
        root("[mcp.servers.bad]\ncommand = \"server\"\ninclude_tools = \"browser_*\"\n");
    assert!(load_from_user_root(Some(directory.path())).is_err());
}
