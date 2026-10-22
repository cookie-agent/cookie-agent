use std::fs;

use cookie_agent_config::{ConfigError, McpServerScope, load_from_user_root};
use cookie_agent_identity::RuntimeId;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

fn load(config: &str) -> Result<cookie_agent_config::LoadedConfiguration, ConfigError> {
    let directory = root(config);
    load_from_user_root(Some(directory.path()))
}

fn rejected(config: &str) -> String {
    load(config).expect_err(config).to_string()
}

#[test]
fn a_template_parses_every_field() {
    let loaded = load(
        r#"
[runtimes.labpc]
command = ["ssh", "labpc", "cookie", "runtime", "serve", "--stdio"]
env = { SSH_ASKPASS = "/bin/false" }
startup_timeout_ms = 1
shutdown_grace_ms = 2

[runtimes.labpc.mcp.servers.rust_analyzer]
command = "rust-analyzer-mcp"
"#,
    )
    .expect("template");
    let template = &loaded.runtimes[&RuntimeId::new("labpc").unwrap()];
    assert_eq!(
        template.command,
        ["ssh", "labpc", "cookie", "runtime", "serve", "--stdio"]
    );
    assert_eq!(template.env["SSH_ASKPASS"], "/bin/false");
    assert_eq!(template.startup_timeout_ms, 1);
    assert_eq!(template.shutdown_grace_ms, 2);
    assert_eq!(
        template.mcp_servers["rust_analyzer"].command.as_deref(),
        Some("rust-analyzer-mcp")
    );
}

#[test]
fn the_removed_workspace_placeholder_is_refused() {
    for command in [
        r#"["runtime", "{workspace}"]"#,
        r#"["runtime", "--workspace={workspace}"]"#,
    ] {
        let error = rejected(&format!("[runtimes.labpc]\ncommand = {command}\n"));
        assert!(error.contains("no longer substituted"), "{error}");
    }
}

#[test]
fn template_fields_are_strict() {
    for field in [
        "kind = \"coding\"",
        "plugins = {}",
        "args = []",
        "cwd = \"/tmp\"",
        // A server's, not a template's.
        "scope = \"runtime\"",
    ] {
        let error = rejected(&format!(
            "[runtimes.labpc]\ncommand = [\"runtime\"]\n{field}\n"
        ));
        assert!(error.contains("unknown field"), "{field}: {error}");
    }
    for value in ["\"ssh labpc\"", "[1]", "{}"] {
        let error = rejected(&format!("[runtimes.labpc]\ncommand = {value}\n"));
        assert!(
            error.contains("malformed configuration content"),
            "{value}: {error}"
        );
    }
    let error = rejected("[runtimes.labpc]\ncommand = [\"runtime\"]\nstartup_timeout_ms = -1\n");
    assert!(error.contains("malformed configuration content"), "{error}");
    let error = rejected(
        "[runtimes.labpc]\ncommand = [\"runtime\"]\n[runtimes.labpc.mcp.servers.x]\ncommand = \"x\"\nlazy = true\n",
    );
    assert!(error.contains("unknown field"), "{error}");
}

#[test]
fn a_command_is_a_nonempty_array_naming_a_program() {
    for template in [
        "[runtimes.labpc]\n",
        "[runtimes.labpc]\ncommand = []\n",
        "[runtimes.labpc]\nenv = { A = \"b\" }\n",
    ] {
        let error = rejected(template);
        assert!(error.contains("nonempty array"), "{template}: {error}");
    }
    let error = rejected("[runtimes.labpc]\ncommand = [\"\", \"--stdio\"]\n");
    assert!(error.contains("name a program"), "{error}");
    // Later elements may be empty arguments.
    load("[runtimes.labpc]\ncommand = [\"runtime\", \"\"]\n").expect("empty argument");
}

#[test]
fn timeouts_must_be_positive() {
    for field in ["startup_timeout_ms", "shutdown_grace_ms"] {
        let error = rejected(&format!(
            "[runtimes.labpc]\ncommand = [\"runtime\"]\n{field} = 0\n"
        ));
        assert!(error.contains(field), "{error}");
        assert!(error.contains("positive"), "{error}");
        let loaded = load(&format!(
            "[runtimes.labpc]\ncommand = [\"runtime\"]\n{field} = 1\n"
        ))
        .expect("one millisecond");
        assert_eq!(loaded.runtimes.len(), 1);
    }
}

#[test]
fn environment_names_and_values_are_checked() {
    for env in [
        r#"{ "" = "x" }"#,
        r#"{ "A=B" = "x" }"#,
        r#"{ "A" = "x\u0000y" }"#,
    ] {
        let error = rejected(&format!(
            "[runtimes.labpc]\ncommand = [\"runtime\"]\nenv = {env}\n"
        ));
        assert!(error.contains("names must be nonempty"), "{env}: {error}");
    }
}

#[test]
fn runtime_ids_follow_the_tool_prefix_rule() {
    for id in [
        "Labpc",
        "1labpc",
        "lab-pc",
        "_labpc",
        "a23456789012345678901234567890123",
    ] {
        let error = rejected(&format!("[runtimes.\"{id}\"]\ncommand = [\"runtime\"]\n"));
        assert!(error.contains("[a-z][a-z0-9_]{0,31}"), "{id}: {error}");
    }
    let longest = "a2345678901234567890123456789012";
    let loaded = load(&format!("[runtimes.{longest}]\ncommand = [\"runtime\"]\n"))
        .expect("32-byte runtime ID");
    assert!(
        loaded
            .runtimes
            .contains_key(&RuntimeId::new(longest).unwrap())
    );
}

#[test]
fn global_is_reserved_and_local_takes_only_mcp_servers() {
    let error = rejected("[runtimes.global]\ncommand = [\"runtime\"]\n");
    assert!(error.contains("`global`"), "{error}");
    let error = rejected("[runtimes.global.mcp.servers.x]\ncommand = \"x\"\n");
    assert!(error.contains("`global`"), "{error}");

    for field in [
        "command = [\"runtime\"]",
        "env = {}",
        "startup_timeout_ms = 10",
        "shutdown_grace_ms = 10",
    ] {
        let error = rejected(&format!("[runtimes.local]\n{field}\n"));
        assert!(error.contains("only an `mcp` table"), "{field}: {error}");
    }
    let loaded =
        load("[runtimes.local.mcp.servers.docs]\ncommand = \"docs-mcp\"\n").expect("local MCP");
    let local = &loaded.runtimes[&RuntimeId::local()];
    assert!(local.command.is_empty());
    assert_eq!(local.mcp_servers.len(), 1);
}

#[test]
fn template_mcp_servers_use_the_mcp_schema_without_oauth() {
    let error = rejected(
        "[runtimes.labpc]\ncommand = [\"runtime\"]\n[runtimes.labpc.mcp.servers.both]\ncommand = \"x\"\nurl = \"https://example.test/mcp\"\n",
    );
    assert!(error.contains("MCP server `both`"), "{error}");
    for oauth in ["true", "{ client_id = \"cookie\" }"] {
        let error = rejected(&format!(
            "[runtimes.labpc]\ncommand = [\"runtime\"]\n[runtimes.labpc.mcp.servers.remote]\nurl = \"https://example.test/mcp\"\noauth = {oauth}\n"
        ));
        assert!(error.contains("OAuth"), "{oauth}: {error}");
    }
    let loaded = load(
        "[runtimes.labpc]\ncommand = [\"runtime\"]\n[runtimes.labpc.mcp.servers.remote]\nurl = \"https://example.test/mcp\"\nheaders = { authorization = \"Bearer token\" }\noauth = false\n[runtimes.labpc.mcp.servers.implicit]\nurl = \"https://example.test/other\"\n",
    )
    .expect("static-header HTTP servers");
    assert_eq!(
        loaded.runtimes[&RuntimeId::new("labpc").unwrap()]
            .mcp_servers
            .len(),
        2
    );
}

#[test]
fn runtimes_must_be_a_table_of_tables() {
    let error = rejected("runtimes = 1\n");
    assert!(error.contains("malformed configuration content"), "{error}");
    let error = rejected("[runtimes]\nlabpc = 1\n");
    assert!(error.contains("malformed configuration content"), "{error}");
}

#[test]
fn a_template_server_is_hosted_per_runtime_or_per_workspace() {
    let loaded = load(
        r#"
[runtimes.labpc]
command = ["runtime"]

[runtimes.labpc.mcp.servers.browser]
command = "chrome-devtools-mcp"
scope = "runtime"

[runtimes.labpc.mcp.servers.lsp]
command = "rust-analyzer-mcp"
scope = "workspace"

[runtimes.labpc.mcp.servers.docs]
command = "docs-mcp"

[runtimes.local.mcp.servers.my_chrome]
command = "chrome-devtools-mcp"
scope = "runtime"
"#,
    )
    .expect("scoped servers");
    let servers = &loaded.runtimes[&RuntimeId::new("labpc").unwrap()].mcp_servers;
    assert_eq!(servers["browser"].hosted_scope(), McpServerScope::Runtime);
    assert_eq!(servers["lsp"].hosted_scope(), McpServerScope::Workspace);
    // Without a scope, a server is hosted per workspace, as before scopes.
    assert_eq!(servers["docs"].scope, None);
    assert_eq!(servers["docs"].hosted_scope(), McpServerScope::Workspace);
    assert_eq!(
        loaded.runtimes[&RuntimeId::local()].mcp_servers["my_chrome"].hosted_scope(),
        McpServerScope::Runtime
    );
    // The scope travels to the runtime with the rest of the server's table,
    // and an absent one stays absent.
    assert_eq!(
        serde_json::to_value(&servers["browser"]).unwrap()["scope"],
        "runtime"
    );
    assert!(
        serde_json::to_value(&servers["docs"])
            .unwrap()
            .get("scope")
            .is_none()
    );
}

#[test]
fn a_scope_is_one_of_runtime_or_workspace() {
    for value in ["\"daemon\"", "\"Runtime\"", "\"\"", "true", "1"] {
        let error = rejected(&format!(
            "[runtimes.labpc]\ncommand = [\"runtime\"]\n[runtimes.labpc.mcp.servers.x]\ncommand = \"x\"\nscope = {value}\n"
        ));
        assert!(
            error.contains("malformed configuration content") || error.contains("unknown variant"),
            "{value}: {error}"
        );
    }
}

#[test]
fn a_top_level_server_takes_no_scope() {
    for scope in ["runtime", "workspace"] {
        let error = rejected(&format!(
            "[mcp.servers.browser]\ncommand = \"chrome-devtools-mcp\"\nscope = \"{scope}\"\n"
        ));
        assert!(
            error.contains("MCP server `browser`") && error.contains("`scope` applies only"),
            "{scope}: {error}"
        );
    }
    load("[mcp.servers.browser]\ncommand = \"chrome-devtools-mcp\"\n").expect("unscoped");
}
