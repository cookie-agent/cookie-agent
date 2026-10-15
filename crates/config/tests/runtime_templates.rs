use std::fs;

use cookie_agent_config::{ConfigError, load_from_user_root};
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
command = ["ssh", "labpc", "cookie", "runtime", "coding", "--stdio", "--workspace", "{workspace}"]
env = { SSH_ASKPASS = "/bin/false" }
startup_timeout_ms = 1
shutdown_grace_ms = 2

[runtimes.labpc.mcp.servers.rust_analyzer]
command = "rust-analyzer-mcp"
"#,
    )
    .expect("template");
    let template = &loaded.runtimes[&RuntimeId::new("labpc").unwrap()];
    assert_eq!(template.command[0], "ssh");
    assert_eq!(template.command.len(), 8);
    assert_eq!(template.env["SSH_ASKPASS"], "/bin/false");
    assert_eq!(template.startup_timeout_ms, 1);
    assert_eq!(template.shutdown_grace_ms, 2);
    assert_eq!(
        template.mcp_servers["rust_analyzer"].command.as_deref(),
        Some("rust-analyzer-mcp")
    );
    assert!(template.takes_workspace());
    assert_eq!(
        template.argv(Some("~/my repo")),
        [
            "ssh",
            "labpc",
            "cookie",
            "runtime",
            "coding",
            "--stdio",
            "--workspace",
            "~/my repo"
        ]
    );
}

#[test]
fn only_a_whole_placeholder_element_takes_a_workspace() {
    let loaded = load("[runtimes.fixed]\ncommand = [\"runtime\", \"--stdio\"]\n").unwrap();
    let template = &loaded.runtimes[&RuntimeId::new("fixed").unwrap()];
    assert!(!template.takes_workspace());
    assert_eq!(template.argv(None), ["runtime", "--stdio"]);

    for command in [
        r#"["runtime", "--workspace={workspace}"]"#,
        r#"["runtime", "{workspace}/sub"]"#,
    ] {
        let error = rejected(&format!("[runtimes.partial]\ncommand = {command}\n"));
        assert!(error.contains("whole element"), "{error}");
    }
}

#[test]
fn template_fields_are_strict() {
    for field in [
        "kind = \"coding\"",
        "plugins = {}",
        "args = []",
        "cwd = \"/tmp\"",
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
    assert!(!local.takes_workspace());
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
