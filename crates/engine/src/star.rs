//! Asking for a GitHub star (`docs/site/specs/github-star.md`): when the
//! web page's card shows, and what each answer records in
//! `<data_root>/star.json`.
//!
//! The daemon reaches GitHub only once the card would otherwise show, with a
//! token already on its machine (`GH_TOKEN`, `GITHUB_TOKEN`, or `gh auth
//! token`). The token is held for the calls alone: never logged, stored, or
//! sent to a client.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use cookie_agent_protocol::{STAR_REPOSITORY, StarAnswer, StarAnswerResult, StarStatusResult};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

pub(crate) const STAR_FILE: &str = "star.json";
/// How long the daemon is used before the card shows.
const WAIT: SignedDuration = SignedDuration::from_hours(7 * 24);
/// How long **Later** hides the card.
const SNOOZE: SignedDuration = SignedDuration::from_hours(7 * 24);
/// How long what GitHub said about the account is kept.
const GITHUB_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long `gh auth token` may take.
const GH_TIMEOUT: Duration = Duration::from_secs(5);
const GITHUB_API: &str = "https://api.github.com";

/// `star.json`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StarFile {
    first_used: Timestamp,
    #[serde(default)]
    snoozed_until: Option<Timestamp>,
    #[serde(default)]
    done: Option<Done>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Done {
    how: DoneHow,
    at: Timestamp,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DoneHow {
    /// **Star** was clicked, whether the daemon starred or the page opened
    /// GitHub.
    Starred,
    /// **Don't remind me**.
    Dismissed,
    /// The signed-in account had starred the repository already.
    Already,
}

/// What GitHub said about the account on the daemon's machine.
#[derive(Clone, Debug)]
enum Account {
    /// No usable token, or GitHub could not be reached.
    None,
    SignedIn {
        login: String,
        starred: bool,
    },
}

/// The card's state: `star.json` and what GitHub said.
pub(crate) struct StarPrompt {
    path: PathBuf,
    /// `None` when the file did not parse: the card never shows.
    file: Mutex<Option<StarFile>>,
    /// What GitHub said, and when. The lock also keeps two page loads from
    /// asking GitHub at once.
    github: tokio::sync::Mutex<Option<(Instant, Account)>>,
    changes: tokio::sync::watch::Sender<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl StarPrompt {
    /// Reads `<data_dir>/star.json`, writing it with `first_used` now when
    /// it does not exist.
    pub(crate) fn open(data_dir: &Path) -> Self {
        let path = data_dir.join(STAR_FILE);
        let file = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<StarFile>(&bytes) {
                Ok(file) => Some(file),
                Err(error) => {
                    eprintln!(
                        "cookie-agent: {}: {error}; the star card stays hidden",
                        path.display()
                    );
                    None
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let file = StarFile {
                    first_used: Timestamp::now(),
                    snoozed_until: None,
                    done: None,
                };
                save(&path, &file);
                Some(file)
            }
            Err(error) => {
                eprintln!(
                    "cookie-agent: {}: {error}; the star card stays hidden",
                    path.display()
                );
                None
            }
        };
        Self {
            path,
            file: Mutex::new(file),
            github: tokio::sync::Mutex::new(None),
            changes: tokio::sync::watch::channel(0).0,
        }
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Whether the card would show, before asking GitHub.
    fn due(&self, now: Timestamp) -> bool {
        lock(&self.file).as_ref().is_some_and(|file| {
            file.done.is_none()
                && now
                    >= file
                        .first_used
                        .saturating_add(WAIT)
                        .unwrap_or(file.first_used)
                && file.snoozed_until.is_none_or(|until| now >= until)
        })
    }

    /// Changes the file, saves it, and tells the connections; `false` when
    /// the card is already done or the file did not parse.
    fn update(&self, change: impl FnOnce(&mut StarFile)) -> bool {
        {
            let mut file = lock(&self.file);
            let Some(file) = file.as_mut().filter(|file| file.done.is_none()) else {
                return false;
            };
            change(file);
            save(&self.path, file);
        }
        self.changes
            .send_modify(|count| *count = count.wrapping_add(1));
        true
    }

    fn finish(&self, how: DoneHow) -> bool {
        self.update(|file| {
            file.done = Some(Done {
                how,
                at: Timestamp::now(),
            });
        })
    }

    /// `star.status`.
    pub(crate) async fn status(&self) -> StarStatusResult {
        if !self.due(Timestamp::now()) {
            return StarStatusResult::default();
        }
        let account = {
            let mut cache = self.github.lock().await;
            match cache.as_ref() {
                Some((at, account)) if at.elapsed() < GITHUB_TTL => account.clone(),
                _ => {
                    let account = ask_github().await;
                    *cache = Some((Instant::now(), account.clone()));
                    account
                }
            }
        };
        match account {
            Account::SignedIn { starred: true, .. } => {
                self.finish(DoneHow::Already);
                StarStatusResult::default()
            }
            // An answer may have come in while GitHub was asked.
            _ if !self.due(Timestamp::now()) => StarStatusResult::default(),
            Account::SignedIn { login, .. } => StarStatusResult {
                show: true,
                login: Some(login),
            },
            Account::None => StarStatusResult {
                show: true,
                login: None,
            },
        }
    }

    /// `star.answer`. An answer once the card is done changes nothing.
    pub(crate) async fn answer(&self, answer: StarAnswer) -> StarAnswerResult {
        if lock(&self.file)
            .as_ref()
            .is_none_or(|file| file.done.is_some())
        {
            return StarAnswerResult::default();
        }
        let starred = match answer {
            StarAnswer::Star => {
                let starred = star().await;
                if let Some(login) = starred.clone() {
                    *self.github.lock().await = Some((
                        Instant::now(),
                        Account::SignedIn {
                            login,
                            starred: true,
                        },
                    ));
                }
                self.finish(DoneHow::Starred);
                starred.is_some()
            }
            StarAnswer::Opened => {
                self.finish(DoneHow::Starred);
                false
            }
            StarAnswer::Later => {
                self.update(|file| {
                    file.snoozed_until = Timestamp::now().checked_add(SNOOZE).ok();
                });
                false
            }
            StarAnswer::Dismiss => {
                self.finish(DoneHow::Dismissed);
                false
            }
            // The server refuses answers it does not know.
            StarAnswer::Unknown => false,
        };
        StarAnswerResult { starred }
    }
}

fn save(path: &Path, file: &StarFile) {
    if let Some(directory) = path.parent()
        && let Err(error) = std::fs::create_dir_all(directory)
    {
        eprintln!("cookie-agent: {}: {error}", directory.display());
        return;
    }
    if let Err(error) = crate::native_compaction::write_atomic(path, file) {
        eprintln!("cookie-agent: could not write {}: {error}", path.display());
    }
}

/// A GitHub token on the daemon's machine: `GH_TOKEN`, `GITHUB_TOKEN`, then
/// `gh auth token`.
async fn token() -> Option<String> {
    for name in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(token) = std::env::var(name) {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_owned());
            }
        }
    }
    let output = tokio::process::Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(GH_TIMEOUT, output).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

fn client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("cookie-agent/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()
}

fn request(
    client: &reqwest::Client,
    method: reqwest::Method,
    path: &str,
    token: &str,
) -> reqwest::RequestBuilder {
    client
        .request(method, format!("{GITHUB_API}{path}"))
        .bearer_auth(token)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

/// The account's login, or `None` when the token is not usable.
async fn login(client: &reqwest::Client, token: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct User {
        login: String,
    }
    let response = request(client, reqwest::Method::GET, "/user", token)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<User>().await.ok().map(|user| user.login)
}

/// Who is signed in on the daemon's machine, and whether they starred the
/// repository.
async fn ask_github() -> Account {
    let (Some(token), Some(client)) = (token().await, client()) else {
        return Account::None;
    };
    let Some(login) = login(&client, &token).await else {
        return Account::None;
    };
    let starred = request(
        &client,
        reqwest::Method::GET,
        &format!("/user/starred/{STAR_REPOSITORY}"),
        &token,
    )
    .send()
    .await
    .is_ok_and(|response| response.status() == reqwest::StatusCode::NO_CONTENT);
    Account::SignedIn { login, starred }
}

/// Stars the repository; the account's login when GitHub accepted.
async fn star() -> Option<String> {
    let token = token().await?;
    let client = client()?;
    let login = login(&client, &token).await?;
    let response = request(
        &client,
        reqwest::Method::PUT,
        &format!("/user/starred/{STAR_REPOSITORY}"),
        &token,
    )
    .header(reqwest::header::CONTENT_LENGTH, "0")
    .send()
    .await
    .ok()?;
    (response.status() == reqwest::StatusCode::NO_CONTENT).then_some(login)
}

impl crate::Engine {
    /// Whether the web page's star card shows (`star.status`).
    pub async fn star_status(&self) -> StarStatusResult {
        self.inner.star.status().await
    }

    /// Records what the user did with the star card (`star.answer`).
    pub async fn star_answer(&self, answer: StarAnswer) -> StarAnswerResult {
        self.inner.star.answer(answer).await
    }

    /// Signals every change to the star card's state.
    #[must_use]
    pub fn subscribe_star_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.star.subscribe()
    }
}
