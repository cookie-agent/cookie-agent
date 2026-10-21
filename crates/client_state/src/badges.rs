//! Where a session runs, as the badges a session row shows
//! (`docs/site/specs/projects.md` section 9), and the stable colour of a
//! machine or a runtime. The `global` attachment never counts. A session
//! with no other attachment has a "no workspace" badge; one attachment
//! shows its runtime and its workspace; several on one machine show the
//! machine, then each runtime; several machines show each machine, in
//! attachment order. The machine comes from `runtimes.list`; a runtime of
//! no known machine stands for its own machine with its runtime badge.

use cookie_agent_protocol::{
    CapabilityName, ProjectDefaults, ProjectRuntime, RuntimeAttachment, RuntimeId,
    RuntimesListResult, SessionMeta,
};

use crate::paths::RuntimeHomes;

/// How many hues badges take.
pub const BADGE_HUES: u8 = 8;

/// The hue (`0..BADGE_HUES`) of the machine or runtime named `key`: a
/// 64-bit FNV-1a hash of its bytes, so every client and every run picks the
/// same one.
#[must_use]
pub fn badge_hue(key: &str) -> u8 {
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    u8::try_from(hash % u64::from(BADGE_HUES)).expect("a hue below eight")
}

/// One badge of a session row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionBadge {
    /// The session attaches nothing besides `global`.
    NoWorkspace,
    /// A machine, with the operating system its runtimes report.
    Machine {
        name: String,
        platform: Option<String>,
    },
    /// A runtime; `capability` only when the runtime offers several.
    Runtime {
        runtime_id: String,
        capability: Option<String>,
    },
    /// The one attachment's workspace, as clients show paths (`~/…`). From
    /// a daemon that lists no attachments, the session's directory.
    Workspace(String),
}

impl SessionBadge {
    /// The badge's hue: a machine's by its name, a runtime's by its ID.
    /// The others are grey.
    #[must_use]
    pub fn hue(&self) -> Option<u8> {
        match self {
            Self::Machine { name, .. } => Some(badge_hue(name)),
            Self::Runtime { runtime_id, .. } => Some(badge_hue(runtime_id)),
            Self::NoWorkspace | Self::Workspace(_) => None,
        }
    }

    /// What the badge says.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::NoWorkspace => "no workspace".to_owned(),
            Self::Machine { name, .. } => name.clone(),
            Self::Runtime {
                runtime_id,
                capability: Some(capability),
            } => format!("{runtime_id}·{capability}"),
            Self::Runtime { runtime_id, .. } => runtime_id.clone(),
            Self::Workspace(path) => path.clone(),
        }
    }
}

/// What `runtimes.list` says of the runtime `runtime_id`.
fn listing_of<'a>(
    listing: Option<&'a RuntimesListResult>,
    runtime_id: &RuntimeId,
) -> Option<&'a cookie_agent_protocol::RuntimeListing> {
    listing?
        .runtimes
        .iter()
        .find(|runtime| runtime.runtime_id == *runtime_id)
}

/// The machine `runtime_id` runs on, if `runtimes.list` knows it.
#[must_use]
pub fn runtime_machine(
    listing: Option<&RuntimesListResult>,
    runtime_id: &RuntimeId,
) -> Option<String> {
    listing_of(listing, runtime_id)?
        .machine
        .as_ref()
        .map(|machine| machine.as_str().to_owned())
}

/// The operating system of `machine`: the first of its runtimes that
/// reported one.
fn machine_platform(listing: Option<&RuntimesListResult>, machine: &str) -> Option<String> {
    listing?
        .runtimes
        .iter()
        .filter(|runtime| {
            runtime
                .machine
                .as_ref()
                .is_some_and(|name| name.as_str() == machine)
        })
        .find_map(|runtime| runtime.platform.clone())
}

/// The badge of `attachment`'s runtime: its capability named only when the
/// runtime offers several.
#[must_use]
pub fn runtime_badge(
    listing: Option<&RuntimesListResult>,
    attachment: &RuntimeAttachment,
) -> SessionBadge {
    Placed::of_attachment(attachment).runtime_badge(listing)
}

/// What the badges read of an attachment, whether a session has it or a
/// project's defaults name it: the runtime, the capability when known, and
/// the workspace.
#[derive(Clone, Copy, Debug)]
struct Placed<'a> {
    runtime_id: &'a RuntimeId,
    capability: Option<&'a CapabilityName>,
    workspace: Option<&'a str>,
}

impl<'a> Placed<'a> {
    fn of_attachment(attachment: &'a RuntimeAttachment) -> Self {
        Self {
            runtime_id: &attachment.runtime_id,
            capability: Some(&attachment.capability),
            workspace: attachment.workspace.as_deref(),
        }
    }

    fn of_default(runtime: &'a ProjectRuntime) -> Self {
        Self {
            runtime_id: &runtime.runtime_id,
            capability: runtime.capability.as_ref(),
            workspace: runtime.workspace.as_deref(),
        }
    }

    fn runtime_badge(self, listing: Option<&RuntimesListResult>) -> SessionBadge {
        let several = listing_of(listing, self.runtime_id)
            .is_some_and(|runtime| runtime.capabilities.len() > 1);
        SessionBadge::Runtime {
            runtime_id: self.runtime_id.to_string(),
            capability: self.capability.filter(|_| several).map(ToString::to_string),
        }
    }

    /// Where it runs: its machine, or the runtime itself when the machine
    /// is not known.
    fn place(self, listing: Option<&RuntimesListResult>) -> Place {
        runtime_machine(listing, self.runtime_id)
            .map_or_else(|| Place::Runtime(self.runtime_id.clone()), Place::Machine)
    }
}

/// The attachments that count for badges: all but `global`.
fn counted(session: &SessionMeta) -> Vec<Placed<'_>> {
    session
        .attachments
        .iter()
        .filter(|attachment| attachment.runtime_id.as_str() != RuntimeId::GLOBAL)
        .map(Placed::of_attachment)
        .collect()
}

/// Where an attachment runs: its machine, or the runtime itself when the
/// machine is not known.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Place {
    Machine(String),
    Runtime(RuntimeId),
}

/// The badges of `session`'s row, in order. `listed` says whether the
/// daemon lists attachments (client protocol 1.7): from an older one the
/// row shows the session's directory alone.
#[must_use]
pub fn session_badges(
    session: &SessionMeta,
    listing: Option<&RuntimesListResult>,
    homes: &RuntimeHomes,
    listed: bool,
) -> Vec<SessionBadge> {
    if session.attachments.is_empty() && !listed {
        return vec![SessionBadge::Workspace(homes.session_directory(session))];
    }
    placed_badges(&counted(session), listing, homes)
}

/// The badges of a project header: where its default attachments run, by
/// the session rows' rules; none when the defaults name no runtime besides
/// `global`, so the header has no second line.
#[must_use]
pub fn project_badges(
    defaults: Option<&ProjectDefaults>,
    listing: Option<&RuntimesListResult>,
    homes: &RuntimeHomes,
) -> Vec<SessionBadge> {
    let placed = defaults
        .map(|defaults| {
            defaults
                .runtimes
                .iter()
                .filter(|runtime| runtime.runtime_id.as_str() != RuntimeId::GLOBAL)
                .map(Placed::of_default)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if placed.is_empty() {
        return Vec::new();
    }
    placed_badges(&placed, listing, homes)
}

fn placed_badges(
    attachments: &[Placed<'_>],
    listing: Option<&RuntimesListResult>,
    homes: &RuntimeHomes,
) -> Vec<SessionBadge> {
    let [first, rest @ ..] = attachments else {
        return vec![SessionBadge::NoWorkspace];
    };
    if rest.is_empty() {
        let mut badges = vec![first.runtime_badge(listing)];
        if let Some(workspace) = first.workspace {
            badges.push(SessionBadge::Workspace(
                homes.display(first.runtime_id, workspace),
            ));
        }
        return badges;
    }
    let mut places = Vec::<Place>::new();
    for attachment in attachments {
        let place = attachment.place(listing);
        if !places.contains(&place) {
            places.push(place);
        }
    }
    let machine_badge = |place: &Place| match place {
        Place::Machine(name) => SessionBadge::Machine {
            platform: machine_platform(listing, name),
            name: name.clone(),
        },
        Place::Runtime(runtime_id) => attachments
            .iter()
            .find(|attachment| attachment.runtime_id == runtime_id)
            .expect("a place comes from an attachment")
            .runtime_badge(listing),
    };
    let mut badges = Vec::new();
    if let [only] = places.as_slice() {
        // One machine: it, then each runtime once. A runtime of no known
        // machine is that machine.
        if matches!(only, Place::Machine(_)) {
            badges.push(machine_badge(only));
        }
        for attachment in attachments {
            let badge = attachment.runtime_badge(listing);
            if !badges.contains(&badge) {
                badges.push(badge);
            }
        }
    } else {
        badges.extend(places.iter().map(machine_badge));
    }
    badges
}

/// The row's tooltip: every attachment but `global`, one a line, as
/// `machine · runtime · ~/workspace`, the machine left out when it is not
/// known.
#[must_use]
pub fn badges_tooltip(
    session: &SessionMeta,
    listing: Option<&RuntimesListResult>,
    homes: &RuntimeHomes,
    listed: bool,
) -> String {
    if session.attachments.is_empty() && !listed {
        return homes.session_directory(session);
    }
    placed_tooltip(&counted(session), listing, homes)
}

/// A project header's tooltip: its default attachments, as
/// [`badges_tooltip`] lists a session's.
#[must_use]
pub fn project_badges_tooltip(
    defaults: &ProjectDefaults,
    listing: Option<&RuntimesListResult>,
    homes: &RuntimeHomes,
) -> String {
    let placed = defaults
        .runtimes
        .iter()
        .filter(|runtime| runtime.runtime_id.as_str() != RuntimeId::GLOBAL)
        .map(Placed::of_default)
        .collect::<Vec<_>>();
    placed_tooltip(&placed, listing, homes)
}

fn placed_tooltip(
    attachments: &[Placed<'_>],
    listing: Option<&RuntimesListResult>,
    homes: &RuntimeHomes,
) -> String {
    let lines = attachments
        .iter()
        .map(|attachment| {
            let mut parts = Vec::new();
            if let Some(machine) = runtime_machine(listing, attachment.runtime_id) {
                parts.push(machine);
            }
            parts.push(attachment.runtime_badge(listing).label());
            if let Some(workspace) = attachment.workspace {
                parts.push(homes.display(attachment.runtime_id, workspace));
            }
            parts.join(" · ")
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return "no workspace".to_owned();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AttachmentName, CapabilityName, MachineName, RuntimeCapabilityInfo, RuntimeKind,
        RuntimeLifetime, RuntimeListing,
    };

    use super::*;

    fn listing() -> RuntimesListResult {
        let runtime = |id: &str, machine: Option<&str>, platform: Option<&str>, caps: &[&str]| {
            RuntimeListing {
                runtime_id: RuntimeId::new(id).unwrap(),
                lifetime: RuntimeLifetime::Builtin,
                connected: true,
                capabilities: caps
                    .iter()
                    .map(|name| RuntimeCapabilityInfo {
                        name: CapabilityName::new(*name).unwrap(),
                        kind: RuntimeKind::new(*name).unwrap(),
                        takes_workspace: true,
                        description: String::new(),
                    })
                    .collect(),
                platform: platform.map(str::to_owned),
                machine: machine.map(|name| MachineName::new(name).unwrap()),
                home: Some(format!("/home/{id}")),
                message: None,
            }
        };
        RuntimesListResult {
            runtimes: vec![
                runtime("global", Some("desk"), Some("linux"), &["global"]),
                runtime("local", Some("desk"), Some("linux"), &["coding"]),
                runtime("alt", Some("desk"), None, &["coding", "notes"]),
                runtime("labpc", Some("lab"), Some("macos"), &["coding"]),
                runtime("box", None, None, &[]),
            ],
            instances: Vec::new(),
        }
    }

    fn session(attachments: &[(&str, &str, Option<&str>)]) -> SessionMeta {
        let mut meta: SessionMeta = serde_json::from_value(serde_json::json!({
            "session_id": cookie_agent_protocol::SessionId::new_v7(),
            "origin": {"type": "root"},
            "short_id": null,
            "cwd_identity": "/home/local/old",
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
            .enumerate()
            .map(
                |(index, (runtime, capability, workspace))| RuntimeAttachment {
                    name: AttachmentName::new(format!("{runtime}{index}")).unwrap(),
                    runtime_id: RuntimeId::new(*runtime).unwrap(),
                    capability: CapabilityName::new(*capability).unwrap(),
                    kind: RuntimeKind::new("coding").unwrap(),
                    workspace: workspace.map(str::to_owned),
                },
            )
            .collect();
        meta
    }

    fn labels(badges: &[SessionBadge]) -> Vec<String> {
        badges.iter().map(SessionBadge::label).collect()
    }

    fn badges(session: &SessionMeta) -> Vec<SessionBadge> {
        let listing = listing();
        session_badges(
            session,
            Some(&listing),
            &RuntimeHomes::from_listing(&listing),
            true,
        )
    }

    #[test]
    fn hues_are_stable_and_spread_over_eight() {
        // Pinned: every client and every release colours these the same.
        assert_eq!(badge_hue("local"), badge_hue("local"));
        assert_eq!(
            ["local", "alt", "box", "roamer", "labpc", "global"].map(badge_hue),
            [0, 4, 2, 7, 5, 6]
        );
        let mut seen = [false; BADGE_HUES as usize];
        for index in 0..64 {
            let hue = badge_hue(&format!("runtime-{index}"));
            assert!(hue < BADGE_HUES);
            seen[usize::from(hue)] = true;
        }
        assert!(seen.iter().all(|seen| *seen), "{seen:?}");
    }

    #[test]
    fn global_alone_is_no_workspace() {
        assert_eq!(
            badges(&session(&[("global", "global", None)])),
            [SessionBadge::NoWorkspace]
        );
        assert_eq!(badges(&session(&[])), [SessionBadge::NoWorkspace]);
        let listing = listing();
        // A daemon that lists no attachments: the directory, absolute.
        assert_eq!(
            session_badges(
                &session(&[]),
                Some(&listing),
                &RuntimeHomes::default(),
                false
            ),
            [SessionBadge::Workspace("/home/local/old".into())]
        );
    }

    #[test]
    fn one_attachment_is_its_runtime_and_workspace() {
        let one = session(&[
            ("global", "global", None),
            ("labpc", "coding", Some("/home/labpc/api")),
        ]);
        assert_eq!(
            badges(&one),
            [
                SessionBadge::Runtime {
                    runtime_id: "labpc".into(),
                    capability: None
                },
                SessionBadge::Workspace("~/api".into()),
            ]
        );
        // A runtime of several capabilities names the one attached.
        let alt = session(&[("alt", "notes", Some("/srv/notes"))]);
        assert_eq!(labels(&badges(&alt)), ["alt·notes", "/srv/notes"]);
    }

    #[test]
    fn several_on_one_machine_are_the_machine_then_each_runtime() {
        let desk = session(&[
            ("global", "global", None),
            ("local", "coding", Some("/home/local/web")),
            ("alt", "coding", Some("/home/alt/api")),
            ("local", "coding", Some("/home/local/docs")),
        ]);
        let shown = badges(&desk);
        assert_eq!(labels(&shown), ["desk", "local", "alt·coding"]);
        assert_eq!(
            shown[0],
            SessionBadge::Machine {
                name: "desk".into(),
                platform: Some("linux".into())
            }
        );
        // Two workspaces of a runtime of no known machine: that runtime.
        let boxed = session(&[("box", "coding", Some("/a")), ("box", "coding", Some("/b"))]);
        assert_eq!(labels(&badges(&boxed)), ["box"]);
    }

    #[test]
    fn project_headers_show_their_default_attachments_by_the_same_rules() {
        let listing = listing();
        let homes = RuntimeHomes::from_listing(&listing);
        let runtime =
            |id: &str, capability: Option<&str>, workspace: Option<&str>| ProjectRuntime {
                runtime_id: RuntimeId::new(id).unwrap(),
                capability: capability.map(|name| CapabilityName::new(name).unwrap()),
                workspace: workspace.map(str::to_owned),
            };
        // No defaults, or defaults that name no runtime: no second line.
        assert!(project_badges(None, Some(&listing), &homes).is_empty());
        let mut defaults = ProjectDefaults {
            global: false,
            ..ProjectDefaults::default()
        };
        assert!(project_badges(Some(&defaults), Some(&listing), &homes).is_empty());
        defaults.runtimes = vec![runtime("labpc", Some("coding"), Some("/home/labpc/api"))];
        assert_eq!(
            labels(&project_badges(Some(&defaults), Some(&listing), &homes)),
            ["labpc", "~/api"]
        );
        // A capability left out (the runtime's only one) still names the
        // runtime; several machines show each machine.
        defaults.runtimes = vec![
            runtime("local", None, Some("/home/local/web")),
            runtime("labpc", Some("coding"), Some("/home/labpc/api")),
        ];
        assert_eq!(
            labels(&project_badges(Some(&defaults), Some(&listing), &homes)),
            ["desk", "lab"]
        );
        assert_eq!(
            project_badges_tooltip(&defaults, Some(&listing), &homes),
            "desk · local · ~/web\nlab · labpc · ~/api"
        );
    }

    #[test]
    fn several_machines_are_each_machine_in_attachment_order() {
        let spread = session(&[
            ("labpc", "coding", Some("/home/labpc/api")),
            ("local", "coding", Some("/home/local/web")),
            ("box", "coding", Some("/srv/x")),
            ("alt", "coding", Some("/home/alt/x")),
        ]);
        let shown = badges(&spread);
        assert_eq!(labels(&shown), ["lab", "desk", "box"]);
        assert_eq!(
            shown[0],
            SessionBadge::Machine {
                name: "lab".into(),
                platform: Some("macos".into())
            }
        );
        // A runtime of no known machine shows as its runtime.
        assert!(matches!(shown[2], SessionBadge::Runtime { .. }));
        assert_eq!(shown[1].hue(), Some(badge_hue("desk")));
        assert_eq!(shown[2].hue(), Some(badge_hue("box")));
        assert_eq!(SessionBadge::NoWorkspace.hue(), None);
    }

    #[test]
    fn the_tooltip_lists_every_attachment_with_its_path() {
        let listing = listing();
        let homes = RuntimeHomes::from_listing(&listing);
        let spread = session(&[
            ("global", "global", None),
            ("labpc", "coding", Some("/home/labpc/api")),
            ("box", "coding", Some("/srv/x")),
        ]);
        assert_eq!(
            badges_tooltip(&spread, Some(&listing), &homes, true),
            "lab · labpc · ~/api\nbox · /srv/x"
        );
        assert_eq!(
            badges_tooltip(
                &session(&[("global", "global", None)]),
                Some(&listing),
                &homes,
                true
            ),
            "no workspace"
        );
    }
}
