//! ACP agents in the configuration (`docs/site/specs/acp-agents.md`
//! sections 3, 4, and 11): `[runtimes.*.acp.agents.*]`, `[acp]`, and the
//! agent document's `acp` block.

use std::fs;

use cookie_agent_config::{ConfigError, load_from_user_root};
use cookie_agent_identity::{AgentId, RuntimeId};

fn root(config: &str, documents: &[(&str, &str)]) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    let agents = directory.path().join("agents");
    fs::create_dir(&agents).expect("agents");
    for (name, text) in documents {
        fs::write(agents.join(name), text).expect("document");
    }
    directory
}

fn load(
    config: &str,
    documents: &[(&str, &str)],
) -> Result<cookie_agent_config::LoadedConfiguration, ConfigError> {
    let directory = root(config, documents);
    load_from_user_root(Some(directory.path()))
}

fn rejected(config: &str, documents: &[(&str, &str)]) -> String {
    load(config, documents).expect_err(config).to_string()
}

#[test]
fn agents_parse_on_local_and_on_a_template() {
    let loaded = load(
        r#"
[runtimes.local.acp.agents.claude]
command = ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
description = "Claude Code"

[runtimes.labpc]
command = ["ssh", "labpc", "cookie", "runtime", "serve", "--stdio"]

[runtimes.labpc.acp.agents.codex]
command = ["npx", "-y", "@agentclientprotocol/codex-acp"]
env = { CODEX_HOME = "/home/ada/.codex" }
enabled = false
"#,
        &[],
    )
    .expect("agents");
    let local = &loaded.runtimes[&RuntimeId::new("local").unwrap()].acp_agents
        [&AgentId::new("claude").unwrap()];
    assert_eq!(
        local.command,
        ["npx", "-y", "@agentclientprotocol/claude-agent-acp"]
    );
    assert_eq!(local.description, "Claude Code");
    assert!(local.enabled);
    assert!(local.env.is_empty());
    let codex = &loaded.runtimes[&RuntimeId::new("labpc").unwrap()].acp_agents
        [&AgentId::new("codex").unwrap()];
    assert_eq!(codex.env["CODEX_HOME"], "/home/ada/.codex");
    assert!(!codex.enabled);
    assert_eq!(codex.description, "");
}

#[test]
fn an_agent_refuses_unknown_and_malformed_fields() {
    for (table, expected) in [
        ("command = [\"x\"]\nmodel = \"gpt\"\n", "unknown field"),
        ("command = []\n", "nonempty"),
        ("command = [\"\"]\n", "name a program"),
        ("command = \"x\"\n", "malformed"),
        (
            "command = [\"x\"]\nenv = { \"A=B\" = \"c\" }\n",
            "environment variable",
        ),
        (
            "command = [\"x\"]\ndescription = \"line\\nbreak\"\n",
            "description",
        ),
        ("command = [\"x\"]\nenabled = \"yes\"\n", "malformed"),
    ] {
        let error = rejected(&format!("[runtimes.local.acp.agents.claude]\n{table}"), &[]);
        assert!(error.contains(expected), "{table}: {error}");
    }
    let error = rejected("[runtimes.local.acp]\nagent = {}\n", &[]);
    assert!(error.contains("unknown field"), "{error}");
}

#[test]
fn agent_keys_follow_the_agent_id_grammar() {
    for key in ["Claude", "claude_code", "-claude", "claude--code"] {
        let error = rejected(
            &format!("[runtimes.local.acp.agents.\"{key}\"]\ncommand = [\"x\"]\n"),
            &[],
        );
        assert!(error.contains("agent IDs are lowercase"), "{key}: {error}");
    }
    load(
        "[runtimes.local.acp.agents.claude-code-2]\ncommand = [\"x\"]\n",
        &[],
    )
    .expect("a hyphenated key");
}

#[test]
fn runtimes_may_declare_the_same_agent_and_a_daemon_document_may_share_its_id() {
    // Every runtime that declares `codex` offers the one agent `codex`.
    let loaded = load(
        r#"
[runtimes.lab_pc]
command = ["x"]
[runtimes.lab_pc.acp.agents.codex]
command = ["x"]

[runtimes.lab]
command = ["x"]
[runtimes.lab.acp.agents.codex]
command = ["y"]
"#,
        &[],
    )
    .expect("two runtimes declaring one agent");
    assert_eq!(loaded.runtimes.len(), 2);
    // A daemon document without an `acp` block keeps its ID; the engine
    // ignores the declaration and says so in the configuration status.
    let daemon = "---\ndescription: A daemon agent\nmode: primary\nmodels: [{ model: \"openai/gpt\" }]\n---\nHello.\n";
    load(
        "[runtimes.local.acp.agents.claude]\ncommand = [\"x\"]\n",
        &[("claude.md", daemon)],
    )
    .expect("a declaration that a daemon document shadows");
}

#[test]
fn an_acp_document_names_no_models_and_may_have_an_empty_body() {
    let document = "---\ndescription: Codex on labpc\nmode: all\nacp:\n  mode: auto\n  config: { model: gpt-5 }\n  hide_tools: [read, \"exa_*\"]\npermissions:\n  bash: { \"git *\": allow }\n---\n";
    let loaded = load(
        "[runtimes.local.acp.agents.claude]\ncommand = [\"x\"]\n",
        &[("local-claude.md", document)],
    )
    .expect("an ACP document");
    let settings = loaded.agents[&AgentId::new("local-claude").unwrap()]
        .frontmatter
        .acp
        .clone()
        .expect("its acp block");
    assert_eq!(settings.mode.as_deref(), Some("auto"));
    assert_eq!(settings.config["model"], "gpt-5");
    assert_eq!(settings.hidden_tools(), ["read", "exa_*"]);
    assert!(
        loaded.agents[&AgentId::new("local-claude").unwrap()]
            .body
            .is_empty()
    );

    for (frontmatter, field) in [
        ("models: [{ model: \"openai/gpt\" }]\nacp: {}\n", "models"),
        ("agent_md: true\nacp: {}\n", "agent_md"),
        ("acp: { mode: \"\" }\n", "acp"),
        ("acp: { hide_tools: [\"\"] }\n", "acp.hide_tools"),
    ] {
        let text = format!("---\ndescription: ACP\nmode: all\n{frontmatter}---\n");
        let error = rejected("", &[("local-claude.md", &text)]);
        assert!(error.contains(field), "{frontmatter}: {error}");
    }
    let error = rejected(
        "",
        &[(
            "local-claude.md",
            "---\ndescription: ACP\nmode: all\nacp: { models: [] }\n---\n",
        )],
    );
    assert!(error.contains("unknown field"), "{error}");
    let error = rejected(
        "",
        &[("plain.md", "---\ndescription: Plain\nmode: subagent\n---\n")],
    );
    assert!(
        error.contains("empty"),
        "a daemon agent still needs a body: {error}"
    );
}

#[test]
fn the_idle_shutdown_is_positive() {
    let loaded = load("[acp]\nidle_shutdown_ms = 1\n", &[]).expect("acp table");
    assert_eq!(loaded.runtime.acp.idle_shutdown_ms, 1);
    let error = rejected("[acp]\nidle_shutdown_ms = 0\n", &[]);
    assert!(
        error.contains("acp.idle_shutdown_ms must be positive"),
        "{error}"
    );
    let error = rejected("[acp]\nidle = 5\n", &[]);
    assert!(error.contains("unknown field"), "{error}");
}

#[test]
fn the_global_runtime_hosts_no_agents() {
    let error = rejected(
        "[runtimes.global.acp.agents.claude]\ncommand = [\"x\"]\n",
        &[],
    );
    assert!(error.contains("global"), "{error}");
}
