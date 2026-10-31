//! `[shell_environment]` and `[runtimes.<id>.shell_environment]` (shell
//! environment spec section 4).

use std::fs;

use cookie_agent_config::load_from_user_root;
use cookie_agent_protocol::runtime_protocol::{ShellCapture, ShellEnvironmentSettings};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

fn settings(capture: ShellCapture, timeout_ms: i64, shell: &str) -> ShellEnvironmentSettings {
    ShellEnvironmentSettings {
        capture,
        timeout_ms,
        shell: shell.to_owned(),
    }
}

#[test]
fn every_key_is_read_and_applies_to_every_runtime() {
    let directory =
        root("[shell_environment]\ncapture = \"login\"\ntimeout_ms = 2500\nshell = \"/bin/zsh\"\n");
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let expected = settings(ShellCapture::Login, 2500, "/bin/zsh");
    assert_eq!(loaded.runtime.shell_environment.settings(), expected);
    assert_eq!(loaded.shell_environment("local").settings(), expected);
    assert_eq!(loaded.shell_environment("labpc").settings(), expected);
}

#[test]
fn a_runtime_table_replaces_only_the_keys_it_sets() {
    let directory = root(
        "[shell_environment]\ncapture = \"interactive\"\ntimeout_ms = 2500\nshell = \"/bin/bash\"\n\
         [runtimes.labpc]\ncommand = [\"ssh\", \"labpc\", \"cookie\", \"runtime\", \"serve\"]\n\
         [runtimes.labpc.shell_environment]\ncapture = \"off\"\n\
         [runtimes.local.shell_environment]\ntimeout_ms = 0\nshell = \"\"\n\
         [runtimes.laptop.shell_environment]\nshell = \"/usr/bin/fish\"\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    assert_eq!(
        loaded.shell_environment("labpc").settings(),
        settings(ShellCapture::Off, 2500, "/bin/bash")
    );
    assert_eq!(
        loaded.shell_environment("local").settings(),
        settings(ShellCapture::Interactive, 0, "")
    );
    // A table without `command` holds a dial-in runtime's settings.
    assert_eq!(
        loaded.shell_environment("laptop").settings(),
        settings(ShellCapture::Interactive, 2500, "/usr/bin/fish")
    );
    assert_eq!(
        loaded.shell_environment("unconfigured").settings(),
        settings(ShellCapture::Interactive, 2500, "/bin/bash")
    );
}

#[test]
fn a_timeout_of_zero_or_less_is_accepted() {
    for timeout in ["0", "-1"] {
        let directory = root(&format!("[shell_environment]\ntimeout_ms = {timeout}\n"));
        let loaded = load_from_user_root(Some(directory.path())).expect("loads");
        assert_eq!(
            loaded.runtime.shell_environment.timeout_ms.to_string(),
            timeout
        );
    }
}

#[test]
fn shell_environment_is_strict() {
    for invalid in [
        "[shell_environment]\ncapture = \"always\"\n",
        "[shell_environment]\ncapture = true\n",
        "[shell_environment]\ntimeout_ms = \"10s\"\n",
        "[shell_environment]\nshell = 1\n",
        "[shell_environment]\npassthrough = [\"PATH\"]\n",
        "[runtimes.local.shell_environment]\ncapture = \"sometimes\"\n",
        "[runtimes.local.shell_environment]\nenv = {}\n",
    ] {
        let directory = root(invalid);
        assert!(
            load_from_user_root(Some(directory.path())).is_err(),
            "{invalid}"
        );
    }
}
