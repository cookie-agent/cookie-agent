//! `cookie skills`: list a daemon's skills with where each comes from,
//! refresh its official skills, and check skills before publishing them
//! (`docs/site/specs/default-models-and-onboarding.md` section 13).

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use clap::{CommandFactory as _, Subcommand};
use cookie_agent_protocol::{SkillSource, SkillsListParams};
use cookie_agent_protocol_transport::Client;
use sha2::{Digest as _, Sha256};

/// The published documentation every skill links to.
pub(crate) const DOCS_SITE: &str = "https://cookie-agent.github.io/doc/dev/";

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum SkillsCommand {
    /// List the daemon's own skills (user and official) with where each
    /// comes from and, for official ones, their version.
    List,
    /// Refresh the official skills now.
    Update,
    /// Check the skills of a directory: each `<name>/SKILL.md` (or the
    /// directory's own `SKILL.md`) parses, names only `cookie` commands
    /// that exist, and links only to documentation pages that exist; an
    /// `index.json` beside them must match their files. Needs no daemon.
    Check {
        /// The skills directory.
        dir: PathBuf,
        /// Check documentation links against this `docs/site` checkout
        /// instead of fetching each page from the published site.
        #[arg(long, value_name = "DIR")]
        docs: Option<PathBuf>,
    },
}

/// `cookie skills list` and `cookie skills update` against a daemon.
pub(crate) async fn run_skills(
    url: &str,
    token: &str,
    command: SkillsCommand,
) -> anyhow::Result<()> {
    let client = Client::connect_websocket_with_token(url, token).await?;
    client
        .handshake_as(super::client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    for line in skills_with(&client, command).await? {
        println!("{line}");
    }
    Ok(())
}

/// What `cookie skills list` or `update` prints, over `client`, which has
/// made its handshake.
pub(crate) async fn skills_with(
    client: &Client,
    command: SkillsCommand,
) -> anyhow::Result<Vec<String>> {
    let mut lines = Vec::new();
    match command {
        SkillsCommand::List => {
            let skills = client
                .list_skills(SkillsListParams { session_id: None })
                .await
                .context("skills.list failed")?
                .skills;
            if skills.is_empty() {
                lines.push("No skills.".into());
            }
            for skill in skills {
                let origin = match skill.source {
                    SkillSource::User => "user".to_owned(),
                    SkillSource::Project => "project".to_owned(),
                    SkillSource::Official => match &skill.version {
                        Some(version) => format!("official {version}"),
                        None => "official".to_owned(),
                    },
                };
                let shadowed = if skill.precedence_winner {
                    ""
                } else {
                    " (shadowed)"
                };
                lines.push(format!(
                    "{}\t{origin}{shadowed}\t{}",
                    skill.name, skill.location
                ));
            }
        }
        SkillsCommand::Update => {
            let result = client
                .update_skills()
                .await
                .context("skills.update failed")?;
            if !result.enabled {
                lines.push("Official skills are off ([skills] official = false).".into());
                return Ok(lines);
            }
            lines.extend(result.updated.iter().map(|name| format!("Updated {name}")));
            lines.extend(result.removed.iter().map(|name| format!("Removed {name}")));
            for skill in &result.skills {
                lines.push(format!("{}\tofficial {}", skill.name, skill.version));
            }
            if let Some(error) = result.error {
                anyhow::bail!("{}\n{error}", lines.join("\n"));
            }
            if result.updated.is_empty() && result.removed.is_empty() {
                lines.push("The official skills are up to date.".into());
            }
        }
        SkillsCommand::Check { .. } => unreachable!("checked without a daemon"),
    }
    Ok(lines)
}

/// `cookie skills check`: prints each problem and fails when there is one.
pub(crate) async fn run_check(dir: &Path, docs: Option<&Path>) -> anyhow::Result<()> {
    let problems = check(dir, docs).await?;
    for problem in &problems {
        println!("{problem}");
    }
    if problems.is_empty() {
        println!("The skills of {} check out.", dir.display());
        Ok(())
    } else {
        anyhow::bail!("{} problem(s) in {}", problems.len(), dir.display())
    }
}

/// The problems of the skills in `dir`, each naming its file.
pub(crate) async fn check(dir: &Path, docs: Option<&Path>) -> anyhow::Result<Vec<String>> {
    let skills = skill_directories(dir)?;
    if skills.is_empty() {
        anyhow::bail!("{} holds no SKILL.md", dir.display());
    }
    let mut problems = Vec::new();
    let mut links = BTreeSet::new();
    for directory in &skills {
        let path = directory.join("SKILL.md");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                problems.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        if let Err(error) = cookie_agent_config::parse_skill_document(
            &text,
            SkillSource::Official,
            &path,
            directory,
            Vec::new(),
        ) {
            problems.push(format!("{}: {error}", path.display()));
        }
        problems.extend(
            command_problems(&text)
                .into_iter()
                .map(|problem| format!("{}: {problem}", path.display())),
        );
        for link in doc_links(&text) {
            links.insert((link, path.clone()));
        }
    }
    let mut checked = std::collections::BTreeMap::<String, bool>::new();
    for (link, path) in links {
        let exists = match checked.get(&link) {
            Some(exists) => *exists,
            None => {
                let exists = page_exists(&link, docs).await;
                checked.insert(link.clone(), exists);
                exists
            }
        };
        if !exists {
            problems.push(format!(
                "{}: {DOCS_SITE}{link} names no documentation page",
                path.display()
            ));
        }
    }
    let index = dir.join("index.json");
    if index.exists() {
        problems.extend(index_problems(dir, &index));
    }
    Ok(problems)
}

/// The skill directories of `dir`: itself when it holds a `SKILL.md`, else
/// each subdirectory that does.
fn skill_directories(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if dir.join("SKILL.md").is_file() {
        return Ok(vec![dir.to_owned()]);
    }
    let mut directories = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("SKILL.md").is_file())
        .collect::<Vec<_>>();
    directories.sort();
    Ok(directories)
}

/// Every backticked `cookie …` command of `text` whose subcommands do not
/// exist in this build's CLI.
pub(crate) fn command_problems(text: &str) -> Vec<String> {
    let root = super::Cli::command();
    let mut problems = Vec::new();
    for span in text.split('`').skip(1).step_by(2) {
        let Some(rest) = span.strip_prefix("cookie ") else {
            continue;
        };
        let mut command = &root;
        for word in rest.split_whitespace() {
            if word.starts_with('-') || word.starts_with('<') || word.starts_with('[') {
                break;
            }
            match command.find_subcommand(word) {
                Some(subcommand) => command = subcommand,
                None => {
                    if command.get_subcommands().next().is_some()
                        && !command.get_arguments().any(clap::Arg::is_positional)
                    {
                        problems.push(format!(
                            "`cookie {rest}`: `{word}` is not a subcommand of `{}`",
                            command.get_name()
                        ));
                    }
                    break;
                }
            }
        }
    }
    problems
}

/// The pages of the documentation site that `text` links to, as paths
/// below [`DOCS_SITE`], without a trailing `/` or anchor.
pub(crate) fn doc_links(text: &str) -> Vec<String> {
    text.match_indices(DOCS_SITE)
        .map(|(index, _)| {
            text[index + DOCS_SITE.len()..]
                .split(|c: char| c.is_whitespace() || matches!(c, ')' | '>' | '`' | '"'))
                .next()
                .unwrap_or_default()
                .split('#')
                .next()
                .unwrap_or_default()
                .trim_end_matches(['/', '.', ','])
                .to_owned()
        })
        .collect()
}

async fn page_exists(page: &str, docs: Option<&Path>) -> bool {
    match docs {
        Some(docs) => {
            page.is_empty() && docs.join("index.md").is_file()
                || docs.join(format!("{page}.md")).is_file()
                || docs.join(page).join("index.md").is_file()
        }
        None => {
            let url = format!(
                "{DOCS_SITE}{page}{}",
                if page.is_empty() { "" } else { "/" }
            );
            let Ok(client) = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .user_agent(concat!("cookie-agent/", env!("CARGO_PKG_VERSION")))
                .build()
            else {
                return false;
            };
            client
                .get(&url)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
        }
    }
}

/// What does not match between `index.json` and the skills beside it.
fn index_problems(dir: &Path, index: &Path) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Index {
        version: u32,
        skills: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        version: String,
        requires_cookie: String,
        files: Vec<File>,
    }
    #[derive(serde::Deserialize)]
    struct File {
        path: String,
        sha256: String,
    }
    let shown = index.display();
    let parsed = std::fs::read(index)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            serde_json::from_slice::<Index>(&bytes).map_err(|error| error.to_string())
        });
    let index = match parsed {
        Ok(index) => index,
        Err(error) => return vec![format!("{shown}: {error}")],
    };
    let mut problems = Vec::new();
    if index.version != 1 {
        problems.push(format!("{shown}: version {} is not 1", index.version));
    }
    let mut listed = BTreeSet::new();
    for skill in &index.skills {
        listed.insert(skill.name.clone());
        if skill.version.trim().is_empty() {
            problems.push(format!("{shown}: `{}` has no version", skill.name));
        }
        if cookie_agent_engine::version_satisfies(&skill.requires_cookie, "0.0.0").is_none() {
            problems.push(format!(
                "{shown}: `{}`: `{}` is not a version range",
                skill.name, skill.requires_cookie
            ));
        }
        if !skill.files.iter().any(|file| file.path == "SKILL.md") {
            problems.push(format!("{shown}: `{}` lists no SKILL.md", skill.name));
        }
        for file in &skill.files {
            let path = dir.join(&skill.name).join(&file.path);
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let actual = format!("{:x}", Sha256::digest(&bytes));
                    if !actual.eq_ignore_ascii_case(&file.sha256) {
                        problems.push(format!(
                            "{shown}: {} has SHA-256 {actual}, the index says {}",
                            path.display(),
                            file.sha256
                        ));
                    }
                }
                Err(error) => problems.push(format!("{shown}: {}: {error}", path.display())),
            }
        }
    }
    if let Ok(directories) = skill_directories(dir) {
        for directory in directories {
            let name = directory
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !listed.contains(&name) {
                problems.push(format!("{shown}: `{name}` is not listed"));
            }
        }
    }
    problems
}
