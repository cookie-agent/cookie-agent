//! The agent memory files the daemon keeps (`docs/site/specs/memory.md`
//! section 16.7): one per agent of every registry, made at start and kept
//! in line through configuration reloads.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use cookie_agent_protocol::{MemoryListParams, MemoryWriteParams};

use super::support::*;

const MODEL: &str = "amazon-bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0";

fn agent(description: &str, mode: &str) -> String {
    format!(
        "---\ndescription: {description}\nmode: {mode}\nenabled: true\nmodels: [{{ model: \"{MODEL}\", variant: base }}]\npermissions: {{}}\n---\nWork.\n"
    )
}

struct Workspace {
    directory: PanicResistantTempDir,
    workspace: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let directory = private_tempdir();
        let workspace = directory.path().join("workspace");
        empty_provider_workspace(&workspace);
        Self {
            directory,
            workspace,
        }
    }

    fn agents(&self) -> PathBuf {
        self.workspace.join(".cookie-agent/agents")
    }

    fn write_agent(&self, path: &str, text: &str) {
        let path = self.agents().join(path);
        let folder = path.parent().unwrap();
        if !folder.is_dir() {
            create_private_test_dir(folder);
        }
        write_private_test_file(&path, text);
    }

    fn open(&self) -> crate::Engine {
        let config =
            cookie_agent_config::load_from_user_root(Some(&self.workspace.join(".cookie-agent")))
                .expect("config");
        open_workspace_engine(
            &self.workspace,
            &self.directory.path().join("data"),
            &self.directory.path().join("provider-store"),
            bedrock_catalog(),
            config,
        )
        .0
    }

    fn memory_file(&self, agent: &str) -> PathBuf {
        self.directory
            .path()
            .join(format!("data/memory/agents/{agent}.md"))
    }

    /// `agent`'s file's text, without its stamp lines.
    fn memory(&self, agent: &str) -> String {
        unstamped(
            &fs::read_to_string(self.memory_file(agent))
                .unwrap_or_else(|error| panic!("{agent}: {error}")),
        )
    }

    /// Every agent memory file's text, without its stamp lines, by agent.
    fn memory_files(&self) -> BTreeMap<String, String> {
        let folder = self.directory.path().join("data/memory/agents");
        fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .map(|path| {
                (
                    path.file_stem().unwrap().to_string_lossy().into_owned(),
                    unstamped(&fs::read_to_string(&path).unwrap()),
                )
            })
            .collect()
    }
}

/// `text` without its stamp lines, which the daemon writes with the time.
fn unstamped(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| !line.starts_with("created: ") && !line.starts_with("modified: "))
        .collect()
}

fn set_old(path: &Path) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
        .unwrap();
}

fn modified(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

#[tokio::test]
async fn every_agent_has_a_memory_file_that_follows_its_document() {
    let workspace = Workspace::new();
    workspace.write_agent("reviewer.md", &agent("Reviews code", "subagent"));
    // The shared document wins over a preset's; among presets, the
    // alphabetically first.
    workspace.write_agent("python/reviewer.md", &agent("Python reviewer", "subagent"));
    workspace.write_agent("python/tester.md", &agent("Python tester", "subagent"));
    workspace.write_agent("rust/tester.md", &agent("Rust tester", "all"));
    workspace.write_agent("rust/helper.md", &agent("Rust helper", "subagent"));
    let engine = workspace.open();

    let files = workspace.memory_files();
    assert_eq!(
        files["primary"],
        "---\ndescription: \"Memory of the `primary` agent: Bedrock test agent\"\nkeywords: [primary]\n---\n"
    );
    assert_eq!(
        files["reviewer"],
        "---\ndescription: \"Memory of the `reviewer` agent: Reviews code\"\nkeywords: [reviewer, subagent]\n---\n"
    );
    assert_eq!(
        files["tester"],
        "---\ndescription: \"Memory of the `tester` agent: Python tester\"\nkeywords: [tester, subagent]\n---\n"
    );
    assert!(files.contains_key("helper"), "{files:?}");
    // Internal agents get none.
    for internal in ["approval", "compaction", "title"] {
        assert!(!files.contains_key(internal), "{files:?}");
    }

    // The user writes the reviewer's memory with a description and
    // keywords of their own: the daemon's stay, the extra keyword too.
    let current = engine
        .read_memory(cookie_agent_protocol::MemoryReadParams {
            url: "agent://reviewer".into(),
        })
        .await
        .unwrap();
    let written = engine
        .write_memory(MemoryWriteParams {
            url: "agent://reviewer".into(),
            text: "---\ndescription: My reviewer\nkeywords: [rust]\n---\n- Report bugs first.\n"
                .into(),
            revision: Some(current.revision),
        })
        .await
        .unwrap();
    assert_eq!(
        written.notice.as_deref(),
        Some(
            "The description of agent://reviewer is the daemon's and cannot be changed, so it stays \"Memory of the `reviewer` agent: Reviews code\". Its keywords always include `reviewer` and `subagent`, so they were added back; other keywords are kept."
        )
    );
    assert_eq!(
        workspace.memory("reviewer"),
        "---\ndescription: \"Memory of the `reviewer` agent: Reviews code\"\nkeywords: [reviewer, subagent, rust]\n---\n- Report bugs first.\n"
    );

    // A changed document rewrites the description after a reload; the body
    // and the extra keyword stay.
    workspace.write_agent("reviewer.md", &agent("Reviews Rust code", "all"));
    engine.reload_configuration().await.expect("reload");
    assert_eq!(
        workspace.memory("reviewer"),
        "---\ndescription: \"Memory of the `reviewer` agent: Reviews Rust code\"\nkeywords: [reviewer, all, rust]\n---\n- Report bugs first.\n"
    );

    // A new agent gets its file; agents that go take their untouched
    // files with them, and a file with notes stays, orphaned.
    workspace.write_agent("planner.md", &agent("Plans work", "subagent"));
    fs::remove_file(workspace.agents().join("reviewer.md")).unwrap();
    fs::remove_file(workspace.agents().join("python/reviewer.md")).unwrap();
    fs::remove_file(workspace.agents().join("rust/helper.md")).unwrap();
    fs::remove_file(workspace.agents().join("python/tester.md")).unwrap();
    engine.reload_configuration().await.expect("reload");
    let files = workspace.memory_files();
    assert!(files.contains_key("planner"), "{files:?}");
    assert!(!files.contains_key("helper"), "{files:?}");
    assert!(files["reviewer"].ends_with("- Report bugs first.\n"));
    let listed = engine
        .list_memory(MemoryListParams::default())
        .await
        .unwrap();
    let orphaned = |url: &str| {
        listed
            .memories
            .iter()
            .find(|memory| memory.url == url)
            .unwrap_or_else(|| panic!("{url} is listed"))
            .orphaned
    };
    assert!(orphaned("agent://reviewer"));
    assert!(!orphaned("agent://planner"));
    // The tester now comes from the rust preset alone.
    assert_eq!(
        files["tester"],
        "---\ndescription: \"Memory of the `tester` agent: Rust tester\"\nkeywords: [tester, all]\n---\n"
    );
    engine.shutdown().await;

    // A restart with nothing to do writes nothing.
    for agent in workspace.memory_files().keys() {
        set_old(&workspace.memory_file(agent));
    }
    let before = workspace.memory_files();
    let engine = workspace.open();
    assert_eq!(workspace.memory_files(), before);
    for agent in before.keys() {
        assert_eq!(
            modified(&workspace.memory_file(agent)),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000),
            "{agent}"
        );
    }
    assert!(engine.sync_agent_memory_now().unwrap().is_empty());
    engine.shutdown().await;
}
