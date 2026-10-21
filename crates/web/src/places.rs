//! Where sessions work, as text: a session's runtimes and workspaces
//! (`SessionMeta.attachments`) in the bottom bar, and its directory
//! (`cwd_identity`, its first workspace) in the sessions filter, named by
//! its runtime. (The session rows show them as badges,
//! `client_state::badges`.) Every path shows under its runtime's home
//! (`~/…`) with the shared `client_state::paths` rule, and absolute where
//! the runtime reported no home.

use cookie_agent_client_state::paths::{RuntimeHomes, display_path};
use cookie_agent_protocol::{RuntimeId, SessionMeta};

/// A runtime at a workspace a session attaches. A daemon older than client
/// protocol 1.7 lists no attachments: a session's place is then its
/// directory, of no known runtime.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Place {
    pub runtime_id: Option<String>,
    /// Canonical, as the runtime reported it.
    pub workspace: String,
}

/// The places `session` works in, in attach order. `listed` says whether
/// the daemon lists attachments (client protocol 1.7), so that none means
/// none.
pub fn places(session: &SessionMeta, listed: bool) -> Vec<Place> {
    if session.attachments.is_empty() && !listed {
        return vec![Place {
            runtime_id: None,
            workspace: session.cwd_identity.to_string(),
        }];
    }
    session
        .attachments
        .iter()
        .filter_map(|attachment| {
            Some(Place {
                runtime_id: Some(attachment.runtime_id.to_string()),
                workspace: attachment.workspace.clone()?,
            })
        })
        .collect()
}

/// What the bottom bar says of where a session works: its workspaces,
/// each under its runtime's home and named by the runtime when that is
/// not `local`, or that it has none.
pub fn places_label(session: &SessionMeta, homes: &RuntimeHomes, listed: bool) -> String {
    let places = places(session, listed);
    if places.is_empty() {
        return "no workspace".to_owned();
    }
    places
        .iter()
        .map(|place| match place.runtime_id.as_deref() {
            Some(RuntimeId::LOCAL) => {
                display_path(&place.workspace, homes.get_str(RuntimeId::LOCAL))
            }
            Some(runtime_id) => format!(
                "{runtime_id}:{}",
                display_path(&place.workspace, homes.get_str(runtime_id))
            ),
            None => homes.session_directory(session),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The filter's label for `session`'s directory: the runtime whose
/// workspace it is and the directory under that runtime's home
/// (`RuntimeHomes::session_directory`), as `labpc · ~/api`; the directory
/// alone when no attachment has a workspace.
pub fn directory_label(session: &SessionMeta, homes: &RuntimeHomes) -> String {
    let directory = homes.session_directory(session);
    let with_workspace = || {
        session
            .attachments
            .iter()
            .filter(|attachment| attachment.workspace.is_some())
    };
    let runtime = with_workspace()
        .find(|attachment| attachment.workspace.as_deref() == Some(session.cwd_identity.as_str()))
        .or_else(|| with_workspace().next())
        .map(|attachment| attachment.runtime_id.as_str());
    match runtime {
        Some(runtime_id) => format!("{runtime_id} · {directory}"),
        None => directory,
    }
}

/// The filter's choices: each session directory (`cwd_identity`) once, by
/// label; a label naming a runtime wins over one that cannot.
pub fn filter_options(sessions: &[SessionMeta], homes: &RuntimeHomes) -> Vec<(String, String)> {
    let mut options = Vec::<(String, String)>::new();
    for session in sessions {
        let value = session.cwd_identity.to_string();
        let label = directory_label(session, homes);
        match options.iter_mut().find(|(known, _)| *known == value) {
            Some(option) if !option.1.contains(" · ") => option.1 = label,
            Some(_) => {}
            None => options.push((value, label)),
        }
    }
    options.sort_by(|a, b| a.1.cmp(&b.1));
    options
}

/// Whether `session` works in the directory the filter's `value` names.
pub fn matches(session: &SessionMeta, value: &str) -> bool {
    session.cwd_identity.as_str() == value
}

/// `text` with every absolute path in it under `home` shown as `~/…`: a
/// tool row's title and argument name paths of the runtime the tool runs
/// on. Words are split at whitespace, which they keep.
pub fn paths_under_home(text: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return text.to_owned();
    };
    let mut shown = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, shown: &mut String| {
        if !word.is_empty() {
            // A quote or bracket around the path stays around it.
            let start = word
                .find(|character: char| !matches!(character, '"' | '\'' | '(' | '[' | '`'))
                .unwrap_or(word.len());
            let (before, rest) = word.split_at(start);
            let end = rest
                .rfind(|character: char| {
                    !matches!(character, '"' | '\'' | ')' | ']' | '`' | ',' | ':')
                })
                .map_or(0, |end| {
                    end + rest[end..].chars().next().map_or(1, char::len_utf8)
                });
            let (path, after) = rest.split_at(end);
            shown.push_str(before);
            shown.push_str(&display_path(path, Some(home)));
            shown.push_str(after);
            word.clear();
        }
    };
    for character in text.chars() {
        if character.is_whitespace() {
            flush(&mut word, &mut shown);
            shown.push(character);
        } else {
            word.push(character);
        }
    }
    flush(&mut word, &mut shown);
    shown
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AttachmentName, CapabilityName, RuntimeAttachment, RuntimeKind, RuntimeLifetime,
        RuntimeListing, RuntimesListResult,
    };

    use super::*;

    fn homes() -> RuntimeHomes {
        let runtime = |id: &str, home: Option<&str>| RuntimeListing {
            runtime_id: RuntimeId::new(id).unwrap(),
            lifetime: RuntimeLifetime::Builtin,
            connected: true,
            capabilities: Vec::new(),
            platform: None,
            machine: None,
            home: home.map(str::to_owned),
            message: None,
        };
        RuntimeHomes::from_listing(&RuntimesListResult {
            runtimes: vec![
                runtime("local", Some("/home/ada")),
                runtime("labpc", Some("/home/yun")),
                runtime("box", None),
            ],
            instances: Vec::new(),
        })
    }

    fn session(cwd: &str, attachments: &[(&str, Option<&str>)]) -> SessionMeta {
        let mut meta: SessionMeta = serde_json::from_value(serde_json::json!({
            "session_id": cookie_agent_protocol::SessionId::new_v7(),
            "origin": {"type": "root"},
            "short_id": null,
            "cwd_identity": cwd,
            "title": null,
            "last_agent_activity": null,
            "creation_selection": {
                "agent": "primary",
                "model": {"model": "custom.local/test", "variant": null},
            },
            "runtime_revision": format!("sha256:{}", "1".repeat(64)),
            "catalog_revision": format!("sha256:{}", "2".repeat(64)),
            "provider_state_revision": format!("sha256:{}", "3".repeat(64)),
            "model_revision": format!("sha256:{}", "4".repeat(64)),
            "agent_revision": format!("sha256:{}", "5".repeat(64)),
            "recipe_registry_revision": format!("sha256:{}", "6".repeat(64)),
            "manifest_revision": format!("sha256:{}", "7".repeat(64)),
            "title_updated_seq": 0,
            "last_event_seq": 1,
            "last_activity": "2026-10-20T00:00:00Z",
            "status": "idle",
            "skipped_events": [],
        }))
        .expect("session meta");
        meta.attachments = attachments
            .iter()
            .map(|(runtime, workspace)| RuntimeAttachment {
                name: AttachmentName::new(*runtime).unwrap(),
                runtime_id: RuntimeId::new(*runtime).unwrap(),
                capability: CapabilityName::new(if *runtime == "global" {
                    "global"
                } else {
                    "coding"
                })
                .unwrap(),
                kind: RuntimeKind::new("coding").unwrap(),
                workspace: workspace.map(str::to_owned),
            })
            .collect();
        meta
    }

    #[test]
    fn rows_name_each_workspace_under_its_runtimes_home() {
        let homes = homes();
        let both = session(
            "/home/ada/web",
            &[
                ("global", None),
                ("local", Some("/home/ada/web")),
                ("labpc", Some("/home/yun/api")),
            ],
        );
        assert_eq!(places_label(&both, &homes, true), "~/web, labpc:~/api");
        let remote = session("/srv/x", &[("box", Some("/srv/x"))]);
        assert_eq!(places_label(&remote, &homes, true), "box:/srv/x");
        let global_only = session("/home/ada", &[("global", None)]);
        assert_eq!(places_label(&global_only, &homes, true), "no workspace");
        // A daemon older than 1.7 lists no attachments: the directory,
        // absolute. A newer one listing none means none.
        let old = session("/home/ada/old", &[]);
        assert_eq!(places_label(&old, &homes, false), "/home/ada/old");
        assert_eq!(places_label(&old, &homes, true), "no workspace");
    }

    #[test]
    fn the_filter_offers_each_directory_once_named_by_its_runtime() {
        let homes = homes();
        let sessions = vec![
            session("/home/ada/web", &[("local", Some("/home/ada/web"))]),
            session(
                "/home/yun/api",
                &[
                    ("labpc", Some("/home/yun/api")),
                    ("local", Some("/home/ada/web")),
                ],
            ),
            // An older daemon's metadata: no attachments, the directory.
            session("/home/ada/old", &[]),
            // The same directory without an attachment, then with one.
            session("/srv/x", &[]),
            session("/srv/x", &[("box", Some("/srv/x"))]),
        ];
        let options = filter_options(&sessions, &homes);
        let labels = options
            .iter()
            .map(|(_, label)| label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "/home/ada/old",
                "box · /srv/x",
                "labpc · ~/api",
                "local · ~/web"
            ]
        );
        let labpc = &options[2].0;
        assert_eq!(labpc, "/home/yun/api");
        assert!(!matches(&sessions[0], labpc));
        assert!(matches(&sessions[1], labpc));
        assert!(matches(&sessions[0], "/home/ada/web"));
        // Filtering is by the session's directory, not every workspace.
        assert!(!matches(&sessions[1], "/home/ada/web"));
        assert_eq!(directory_label(&sessions[1], &homes), "labpc · ~/api");
    }

    #[test]
    fn paths_in_tool_text_show_under_home() {
        let home = Some("/home/ada");
        assert_eq!(
            paths_under_home("Read /home/ada/web/main.rs", home),
            "Read ~/web/main.rs"
        );
        assert_eq!(
            paths_under_home("cd \"/home/ada/web\" && ls /etc /home/adam", home),
            "cd \"~/web\" && ls /etc /home/adam"
        );
        assert_eq!(
            paths_under_home("Glob (/home/ada): *.rs", home),
            "Glob (~): *.rs"
        );
        assert_eq!(
            paths_under_home("Read /home/ada/x", None),
            "Read /home/ada/x"
        );
        assert_eq!(paths_under_home("  two  spaces ", home), "  two  spaces ");
    }
}
