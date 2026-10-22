//! How clients show paths. The daemon and runtimes store paths canonical
//! and absolute; a client shows a path of a runtime under the home directory
//! that runtime reported (`home` in `runtimes.list`), and absolute otherwise.

use std::collections::BTreeMap;

use cookie_agent_protocol::{RuntimeId, RuntimesListResult, SessionMeta};

/// `path` as a client shows it, for a runtime whose home directory is
/// `home`: `~` for the home itself, `~` followed by the rest under it
/// (`~/src/app`, or `~\src\app` for a Windows path), and `path` unchanged
/// anywhere else, for a runtime that reported no home, and for a home that
/// is a filesystem root.
///
/// A POSIX home (`/home/ada`) matches exactly. A Windows home
/// (`C:\Users\ada`, `\\server\share\ada`) matches ignoring ASCII case and
/// with `\` and `/` alike, as Windows compares paths. Pure: nothing on disk
/// is consulted, since the path may be on another machine.
#[must_use]
pub fn display_path(path: &str, home: Option<&str>) -> String {
    match home.and_then(|home| relative_to_home(path, home)) {
        Some(rest) => format!("~{rest}"),
        None => path.to_owned(),
    }
}

/// The client protocol minor from which `runtimes.list` reports each
/// runtime's `home` and `SessionMeta` lists its `attachments`.
pub const RUNTIME_HOMES_MINOR: u32 = 7;

/// Whether a daemon whose handshake reported `version` reports runtime homes
/// and session attachments. Before the handshake nothing is known.
#[must_use]
pub fn daemon_reports_runtime_homes(version: Option<cookie_agent_protocol::SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= RUNTIME_HOMES_MINOR)
    })
}

/// The home directory each runtime reported, by runtime ID, from
/// `runtimes.list` (client protocol 1.7.0; empty from an older daemon).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeHomes(BTreeMap<RuntimeId, String>);

impl RuntimeHomes {
    /// The homes `listing` reports.
    #[must_use]
    pub fn from_listing(listing: &RuntimesListResult) -> Self {
        Self(
            listing
                .runtimes
                .iter()
                .filter_map(|runtime| Some((runtime.runtime_id.clone(), runtime.home.clone()?)))
                .collect(),
        )
    }

    /// The home `runtime_id` reported, if any.
    #[must_use]
    pub fn get(&self, runtime_id: &RuntimeId) -> Option<&str> {
        self.0.get(runtime_id).map(String::as_str)
    }

    /// The home of the runtime with ID `runtime_id`, given as text.
    #[must_use]
    pub fn get_str(&self, runtime_id: &str) -> Option<&str> {
        RuntimeId::new(runtime_id)
            .ok()
            .and_then(|runtime_id| self.get(&runtime_id))
    }

    /// `workspace`, a path of the runtime `runtime_id`, as a client shows it
    /// ([`display_path`]).
    #[must_use]
    pub fn display(&self, runtime_id: &RuntimeId, workspace: &str) -> String {
        display_path(workspace, self.get(runtime_id))
    }

    /// The directory of the session `meta` as a client shows it: its
    /// `cwd_identity` under the home of the runtime whose attachment has that
    /// workspace, else of its first attachment with a workspace, and
    /// absolute when that runtime reported no home or the metadata lists no
    /// attachments.
    #[must_use]
    pub fn session_directory(&self, meta: &SessionMeta) -> String {
        let directory = meta.cwd_identity.as_str();
        let with_workspace = || {
            meta.attachments
                .iter()
                .filter(|attachment| attachment.workspace.is_some())
        };
        let runtime = with_workspace()
            .find(|attachment| attachment.workspace.as_deref() == Some(directory))
            .or_else(|| with_workspace().next())
            .map(|attachment| &attachment.runtime_id);
        display_path(directory, runtime.and_then(|runtime| self.get(runtime)))
    }

    /// Where the session `meta` works, for a one-line label: its directory
    /// ([`Self::session_directory`]) when one of its own attachments has a
    /// workspace or its subagents brought none; else the workspaces its
    /// subagents brought (`SessionMeta::delegated_attachments`), each as
    /// `runtime:~/path` (`local`'s without the runtime), comma-joined, so a
    /// root with only `global` shows where its subagents work.
    #[must_use]
    pub fn session_place(&self, meta: &SessionMeta) -> String {
        let own = meta
            .attachments
            .iter()
            .any(|attachment| attachment.workspace.is_some());
        let mut delegated = Vec::<String>::new();
        if !own {
            for attachment in &meta.delegated_attachments {
                let Some(workspace) = &attachment.workspace else {
                    continue;
                };
                let path = self.display(&attachment.runtime_id, workspace);
                let shown = if attachment.runtime_id.as_str() == RuntimeId::LOCAL {
                    path
                } else {
                    format!("{}:{path}", attachment.runtime_id)
                };
                if !delegated.contains(&shown) {
                    delegated.push(shown);
                }
            }
        }
        if delegated.is_empty() {
            self.session_directory(meta)
        } else {
            delegated.join(", ")
        }
    }
}

/// What follows `home` in `path`: empty for the home itself, else the rest
/// beginning with its separator. `None` when `path` is not under `home`.
fn relative_to_home<'p>(path: &'p str, home: &str) -> Option<&'p str> {
    let windows = is_windows_path(home);
    let separator = |byte: u8| byte == b'/' || (windows && byte == b'\\');
    let home =
        home.trim_end_matches(|character: char| character.is_ascii() && separator(character as u8));
    // A root home (`/`, `C:\`) would put every path under `~`.
    if home.is_empty() || (windows && home.len() <= 2) {
        return None;
    }
    let prefix = path.get(..home.len())?;
    let same = if windows {
        prefix
            .bytes()
            .zip(home.bytes())
            .all(|(a, b)| a.eq_ignore_ascii_case(&b) || (separator(a) && separator(b)))
    } else {
        prefix == home
    };
    if !same {
        return None;
    }
    let rest = &path[home.len()..];
    match rest.bytes().next() {
        None => Some(rest),
        Some(byte) if separator(byte) => {
            // `~` alone for the home spelled with a trailing separator.
            Some(if rest.bytes().all(separator) {
                ""
            } else {
                rest
            })
        }
        Some(_) => None,
    }
}

/// Whether `path` is spelled as a Windows path: a drive (`C:\`, `C:/`) or a
/// UNC share (`\\server\share`).
fn is_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.starts_with(r"\\")
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

#[cfg(test)]
mod tests {
    use super::{RuntimeHomes, display_path};

    #[test]
    fn a_posix_path_under_the_home_shows_from_the_tilde() {
        let home = Some("/home/ada");
        assert_eq!(display_path("/home/ada", home), "~");
        assert_eq!(display_path("/home/ada/", home), "~");
        assert_eq!(display_path("/home/ada/src/app", home), "~/src/app");
        assert_eq!(
            display_path("/home/ada/src/app", Some("/home/ada/")),
            "~/src/app"
        );
        // Outside the home, a sibling sharing its prefix, and another case.
        assert_eq!(display_path("/srv/api", home), "/srv/api");
        assert_eq!(display_path("/home/adam/x", home), "/home/adam/x");
        assert_eq!(display_path("/home/Ada/x", home), "/home/Ada/x");
        assert_eq!(display_path("/home", home), "/home");
        // No home, or a root one: absolute.
        assert_eq!(display_path("/home/ada/x", None), "/home/ada/x");
        assert_eq!(display_path("/etc/hosts", Some("/")), "/etc/hosts");
        // Not a path at all, as a raw cwd identity may be.
        assert_eq!(display_path("~/x", home), "~/x");
        assert_eq!(display_path("", home), "");
    }

    #[test]
    fn a_windows_path_under_the_home_keeps_its_separators() {
        let home = Some(r"C:\Users\ada");
        assert_eq!(display_path(r"C:\Users\ada", home), "~");
        assert_eq!(display_path(r"C:\Users\ada\src\app", home), r"~\src\app");
        // Windows compares ignoring case, with either separator.
        assert_eq!(display_path(r"c:\users\ADA\src", home), r"~\src");
        assert_eq!(display_path("C:/Users/ada/src", home), "~/src");
        assert_eq!(display_path(r"C:\Users\adam\x", home), r"C:\Users\adam\x");
        assert_eq!(display_path(r"D:\Users\ada\x", home), r"D:\Users\ada\x");
        let share = Some(r"\\server\home\ada");
        assert_eq!(display_path(r"\\server\home\ada\x", share), r"~\x");
        // No home, or a drive root: absolute, never `~`.
        assert_eq!(display_path(r"C:\Users\ada\x", None), r"C:\Users\ada\x");
        assert_eq!(display_path(r"C:\x", Some(r"C:\")), r"C:\x");
        // A POSIX home never matches a Windows path, nor the reverse.
        assert_eq!(
            display_path(r"C:\home\ada", Some("/home/ada")),
            r"C:\home\ada"
        );
        assert_eq!(display_path("/home/ada", home), "/home/ada");
    }

    #[test]
    fn homes_come_from_a_daemon_of_minor_seven() {
        use cookie_agent_protocol::SemVer;
        let reports = |version| super::daemon_reports_runtime_homes(Some(version));
        assert!(!super::daemon_reports_runtime_homes(None));
        assert!(!reports(SemVer::new(1, 6, 0)));
        assert!(reports(SemVer::new(1, 7, 0)));
        assert!(!reports(SemVer::new(2, 7, 0)));
    }

    #[test]
    fn a_session_directory_shows_under_the_home_of_its_workspace_runtime() {
        let listing: cookie_agent_protocol::RuntimesListResult =
            serde_json::from_value(serde_json::json!({
                "runtimes": [
                    {"runtime_id": "local", "lifetime": "builtin", "connected": true,
                     "capabilities": [], "home": "/home/ada"},
                    {"runtime_id": "labpc", "lifetime": "dial_in", "connected": true,
                     "capabilities": [], "home": "/home/lab"},
                    {"runtime_id": "box", "lifetime": "spawned", "connected": false,
                     "capabilities": []}
                ],
                "instances": []
            }))
            .unwrap();
        let homes = RuntimeHomes::from_listing(&listing);
        assert_eq!(homes.get_str("labpc"), Some("/home/lab"));
        assert_eq!(homes.get_str("box"), None);
        let session = |cwd: &str,
                       attachments: serde_json::Value|
         -> cookie_agent_protocol::SessionMeta {
            serde_json::from_value(serde_json::json!({
                "session_id": "0192f3a4-5b6c-7d8e-9f00-112233445566",
                "origin": {"type": "root"},
                "short_id": null,
                "cwd_identity": cwd,
                "creation_selection": {"agent": "default", "model": {"model": "openai/gpt-5", "variant": null}},
                "runtime_revision": format!("sha256:{}", "1".repeat(64)),
                "catalog_revision": format!("sha256:{}", "2".repeat(64)),
                "provider_state_revision": format!("sha256:{}", "3".repeat(64)),
                "model_revision": format!("sha256:{}", "4".repeat(64)),
                "agent_revision": format!("sha256:{}", "5".repeat(64)),
                "recipe_registry_revision": format!("sha256:{}", "6".repeat(64)),
                "manifest_revision": format!("sha256:{}", "7".repeat(64)),
                "title": null,
                "title_updated_seq": 0,
                "last_event_seq": 1,
                "last_activity": "2026-10-21T00:00:00Z",
                "status": "idle",
                "skipped_events": [],
                "attachments": attachments,
            }))
            .unwrap()
        };
        let attachment = |runtime: &str, workspace: Option<&str>| {
            serde_json::json!({"name": runtime, "runtime_id": runtime, "capability": "coding",
                               "kind": "coding", "workspace": workspace})
        };
        // The attachment whose workspace is the directory picks the home.
        let both = serde_json::json!([
            attachment("global", None),
            attachment("local", Some("/home/ada/notes")),
            attachment("labpc", Some("/home/lab/api")),
        ]);
        assert_eq!(
            homes.session_directory(&session("/home/lab/api", both.clone())),
            "~/api"
        );
        // Else the first with a workspace.
        assert_eq!(
            homes.session_directory(&session("/home/ada/x", both)),
            "~/x"
        );
        // A runtime without a home, or no attachments: absolute.
        assert_eq!(
            homes.session_directory(&session(
                "/home/ada/x",
                serde_json::json!([attachment("box", Some("/home/ada/x"))])
            )),
            "/home/ada/x"
        );
        assert_eq!(
            homes.session_directory(&session("/home/ada/x", serde_json::json!([]))),
            "/home/ada/x"
        );
        // A root with only `global` shows where its subagents work.
        let mut root = session("/home/ada", serde_json::json!([attachment("global", None)]));
        assert_eq!(homes.session_place(&root), "/home/ada");
        root.delegated_attachments = serde_json::from_value(serde_json::json!([
            attachment("labpc", Some("/home/lab/api")),
            attachment("local", Some("/home/ada/web")),
        ]))
        .unwrap();
        assert_eq!(homes.session_place(&root), "labpc:~/api, ~/web");
        // With a workspace of its own, it shows its directory.
        root.attachments
            .extend(root.delegated_attachments[1..].iter().cloned());
        assert_eq!(homes.session_place(&root), "~");
    }
}
