//! Which machine a tool call ran on, as every client labels it
//! (`docs/site/specs/tool-call-machines.md`). A session labels its calls once
//! its machines come to two or more: those attached now and those its calls
//! ran on, so detaching back down to one keeps the labels. Machines come from
//! the listing a client already reads (`runtimes.list`, or
//! `session.runtimes.list` for a guest); until one answers, nothing is labelled.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

use cookie_agent_protocol::{RuntimeId, RuntimesListResult, ToolSource};

use crate::badges::badge_hue;
use crate::{SessionState, ToolCallState};

/// The columns a label's name may take; a longer one is cut.
const MAX_NAME_COLUMNS: usize = 24;

/// How many machines a work row names before it counts the rest.
const WORK_ROW_MACHINES: usize = 3;

/// Each runtime's machine, as a listing says.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeMachines(BTreeMap<RuntimeId, String>);

impl RuntimeMachines {
    #[must_use]
    pub fn from_listing(listing: &RuntimesListResult) -> Self {
        Self(
            listing
                .runtimes
                .iter()
                .filter_map(|runtime| {
                    let machine = runtime.machine.as_ref()?;
                    Some((runtime.runtime_id.clone(), machine.as_str().to_owned()))
                })
                .collect(),
        )
    }

    /// The name `runtime` goes by: its machine's, or its own ID when the
    /// listing knows no machine for it.
    fn name(&self, runtime: &RuntimeId) -> String {
        self.0
            .get(runtime)
            .map_or_else(|| runtime.to_string(), Clone::clone)
    }
}

/// The runtime a call ran on, when it ran on a machine: `global`'s tools, the
/// base layer, and plugins act on none.
#[must_use]
pub fn call_runtime(tool: &ToolCallState) -> Option<&RuntimeId> {
    let runtime = match tool.presentation.tool.source.as_ref()?.known()? {
        ToolSource::Runtime { runtime_id, .. } | ToolSource::Acp { runtime_id, .. } => runtime_id,
        ToolSource::Mcp {
            runtime_id: Some(runtime_id),
            ..
        } => runtime_id,
        _ => return None,
    };
    (runtime.as_str() != RuntimeId::GLOBAL).then_some(runtime)
}

/// One machine as its label shows it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MachineLabel {
    pub name: String,
    /// The name on a narrow row: without the prefix it shares with the
    /// session's other machines.
    pub short: String,
    /// Its colour, the one its badges have (`badges::badge_hue`).
    pub hue: u8,
}

/// The labels of one session's calls; empty while it does not label them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MachineLabels {
    runtimes: BTreeMap<RuntimeId, MachineLabel>,
    wide_columns: usize,
    narrow_columns: usize,
    key: u64,
}

impl MachineLabels {
    /// The labels of `state`'s calls, given the listing's `machines`.
    #[must_use]
    pub fn of_session(state: &SessionState, machines: Option<&RuntimeMachines>) -> Self {
        let Some(machines) = machines else {
            return Self::default();
        };
        let runtimes = state
            .runtime_attachments
            .attachments()
            .iter()
            .map(|attachment| &attachment.runtime_id)
            .chain(state.acp.runtime_id.as_ref())
            .chain(&state.call_runtimes)
            .filter(|runtime| runtime.as_str() != RuntimeId::GLOBAL)
            .map(|runtime| (runtime.clone(), machines.name(runtime)))
            .collect::<BTreeMap<_, _>>();
        let names = runtimes
            .values()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if names.len() < 2 {
            return Self::default();
        }
        let shorts = short_names(&names);
        let columns = |name: &str| name.chars().count().min(MAX_NAME_COLUMNS);
        let wide_columns = names.iter().map(|name| columns(name)).max().unwrap_or(0);
        let narrow_columns = shorts.values().map(|name| columns(name)).max().unwrap_or(0);
        let labels = runtimes
            .iter()
            .map(|(runtime, name)| {
                let label = MachineLabel {
                    name: name.clone(),
                    short: shorts[name.as_str()].to_owned(),
                    hue: badge_hue(name),
                };
                (runtime.clone(), label)
            })
            .collect::<BTreeMap<_, _>>();
        // FNV-1a, as `badge_hue`: stable, and a client's caches compare it only
        // within one run.
        let mut hasher = Fnv(0xcbf2_9ce4_8422_2325);
        labels.hash(&mut hasher);
        Self {
            runtimes: labels,
            wide_columns,
            narrow_columns,
            key: hasher.0,
        }
    }

    /// `tool`'s label, while the session labels its calls.
    #[must_use]
    pub fn call(&self, tool: &ToolCallState) -> Option<&MachineLabel> {
        self.runtime(call_runtime(tool)?)
    }

    #[must_use]
    pub fn runtime(&self, runtime: &RuntimeId) -> Option<&MachineLabel> {
        self.runtimes.get(runtime)
    }

    /// What a work row names of the machines of `runtimes`, each once, and how
    /// many more it leaves to a count.
    #[must_use]
    pub fn work_row(&self, runtimes: &[RuntimeId]) -> (Vec<&MachineLabel>, usize) {
        let mut labels = Vec::<&MachineLabel>::new();
        for label in runtimes.iter().filter_map(|runtime| self.runtime(runtime)) {
            if !labels.iter().any(|known| known.name == label.name) {
                labels.push(label);
            }
        }
        let more = labels.len().saturating_sub(WORK_ROW_MACHINES);
        labels.truncate(WORK_ROW_MACHINES);
        (labels, more)
    }

    /// The columns the widest name takes, so labels line up down a transcript.
    #[must_use]
    pub fn wide_columns(&self) -> usize {
        self.wide_columns
    }

    /// The columns the widest short name takes.
    #[must_use]
    pub fn narrow_columns(&self) -> usize {
        self.narrow_columns
    }

    /// A fingerprint of what the labels show, which a cached row depends on.
    #[must_use]
    pub fn key(&self) -> u64 {
        self.key
    }
}

struct Fnv(u64);

impl Hasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Each of `names` without the longest prefix, up to a separator, it shares
/// with another: `yunhao-labpc` beside `yunhao-labserver` is `labpc`. A name
/// the cut would leave empty or like another's stays whole.
fn short_names<'a>(names: &BTreeSet<&'a str>) -> BTreeMap<&'a str, &'a str> {
    let cut = |name: &'a str| -> &'a str {
        let shared = names
            .iter()
            .filter(|other| **other != name)
            .map(|other| {
                let common = name
                    .char_indices()
                    .zip(other.chars())
                    .find(|((_, ours), theirs)| ours != theirs)
                    .map_or(name.len().min(other.len()), |((at, _), _)| at);
                name[..common]
                    .rfind(['-', '_', '.'])
                    .map_or(0, |separator| separator + 1)
            })
            .max()
            .unwrap_or(0);
        &name[shared..]
    };
    let cuts = names.iter().map(|name| cut(name)).collect::<Vec<_>>();
    names
        .iter()
        .zip(&cuts)
        .map(|(name, short)| {
            let unique = cuts.iter().filter(|other| *other == short).count() == 1;
            (
                *name,
                if unique && !short.is_empty() {
                    *short
                } else {
                    *name
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        AssistantToolCallRef, AttachmentName, CapabilityName, EventPayload, MachineName, Open,
        PreparedPresentation, RuntimeKind, RuntimeLifetime, RuntimeListing, RuntimesListResult,
        SafeDisplayText, ToolIdentity, ToolPresentation,
    };

    use super::*;

    fn runtime(id: &str) -> RuntimeId {
        RuntimeId::new(id).unwrap()
    }

    fn machines(pairs: &[(&str, &str)]) -> RuntimeMachines {
        RuntimeMachines::from_listing(&RuntimesListResult {
            runtimes: pairs
                .iter()
                .map(|(id, machine)| RuntimeListing {
                    runtime_id: runtime(id),
                    lifetime: RuntimeLifetime::Spawned,
                    connected: true,
                    capabilities: Vec::new(),
                    platform: None,
                    machine: Some(MachineName::new(*machine).unwrap()),
                    home: None,
                    message: None,
                    active_runs: 0,
                    background_calls: 0,
                })
                .collect(),
            instances: Vec::new(),
        })
    }

    fn attach(state: &mut SessionState, id: &str) {
        state
            .runtime_attachments
            .observe(&EventPayload::RuntimeAttached {
                name: AttachmentName::new(format!("{id}_work")).unwrap(),
                runtime_id: runtime(id),
                capability: CapabilityName::coding(),
                kind: RuntimeKind::new("coding").unwrap(),
                workspace: None,
            });
    }

    fn detach(state: &mut SessionState, id: &str) {
        state
            .runtime_attachments
            .observe(&EventPayload::RuntimeDetached {
                name: AttachmentName::new(format!("{id}_work")).unwrap(),
            });
    }

    fn call_on(source: ToolSource) -> ToolCallState {
        ToolCallState::started(
            cookie_agent_protocol::ToolCallId::new_v7(),
            AssistantToolCallRef {
                model_turn_seq: 1,
                content_index: 0,
                model_call_id: cookie_agent_protocol::ModelCallId::new("call").unwrap(),
                provider_item_id: None,
            },
            ToolPresentation::new(
                PreparedPresentation::new(SafeDisplayText::new("bash").unwrap(), None),
                ToolIdentity {
                    name: "bash".into(),
                    published_name: "bash".into(),
                    source: Some(Open::Known(source)),
                },
            ),
            "{}".into(),
        )
    }

    const TWO: &[(&str, &str)] = &[("local", "yunhao-labserver"), ("unicorn", "unicorn")];

    #[test]
    fn nothing_is_labelled_until_a_listing_answers() {
        let mut state = SessionState::default();
        attach(&mut state, "local");
        attach(&mut state, "unicorn");
        assert_eq!(
            MachineLabels::of_session(&state, None),
            MachineLabels::default()
        );
        assert!(
            MachineLabels::of_session(&state, Some(&machines(TWO)))
                .runtime(&runtime("local"))
                .is_some()
        );
    }

    #[test]
    fn one_machine_is_not_labelled_however_many_runtimes_it_has() {
        let mut state = SessionState::default();
        attach(&mut state, "local");
        attach(&mut state, "second");
        attach(&mut state, "global");
        let same = machines(&[("local", "labserver"), ("second", "labserver")]);
        assert_eq!(
            MachineLabels::of_session(&state, Some(&same)),
            MachineLabels::default()
        );
    }

    #[test]
    fn detaching_down_to_one_keeps_the_labels_of_a_machine_that_was_used() {
        let listing = machines(TWO);
        let mut state = SessionState::default();
        attach(&mut state, "local");
        attach(&mut state, "unicorn");
        state.call_runtimes.insert(runtime("unicorn"));
        detach(&mut state, "unicorn");
        let labels = MachineLabels::of_session(&state, Some(&listing));
        assert_eq!(labels.runtime(&runtime("unicorn")).unwrap().name, "unicorn");
        assert_eq!(
            labels.runtime(&runtime("local")).unwrap().name,
            "yunhao-labserver"
        );
    }

    #[test]
    fn a_machine_detached_unused_leaves_nothing_behind() {
        let listing = machines(TWO);
        let mut state = SessionState::default();
        attach(&mut state, "local");
        attach(&mut state, "unicorn");
        detach(&mut state, "unicorn");
        assert_eq!(
            MachineLabels::of_session(&state, Some(&listing)),
            MachineLabels::default()
        );
    }

    #[test]
    fn an_acp_agents_host_counts_as_attached() {
        let listing = machines(TWO);
        let mut state = SessionState::default();
        attach(&mut state, "unicorn");
        state.acp.runtime_id = Some(runtime("local"));
        assert!(
            MachineLabels::of_session(&state, Some(&listing))
                .runtime(&runtime("local"))
                .is_some()
        );
    }

    #[test]
    fn a_runtime_the_listing_does_not_know_goes_by_its_id() {
        let mut state = SessionState::default();
        attach(&mut state, "local");
        attach(&mut state, "gone");
        let labels = MachineLabels::of_session(&state, Some(&machines(TWO)));
        assert_eq!(labels.runtime(&runtime("gone")).unwrap().name, "gone");
    }

    #[test]
    fn only_calls_on_a_machine_have_a_runtime() {
        let attachment = AttachmentName::new("work").unwrap();
        let on = |id: &str| ToolSource::Runtime {
            runtime_id: runtime(id),
            attachment: attachment.clone(),
            capability: CapabilityName::coding(),
        };
        assert_eq!(
            call_runtime(&call_on(on("unicorn"))),
            Some(&runtime("unicorn"))
        );
        assert_eq!(call_runtime(&call_on(on(RuntimeId::GLOBAL))), None);
        assert_eq!(call_runtime(&call_on(ToolSource::Base)), None);
        assert_eq!(
            call_runtime(&call_on(ToolSource::Plugin {
                plugin: "issues".into()
            })),
            None
        );
        assert_eq!(
            call_runtime(&call_on(ToolSource::Mcp {
                server: "docs".into(),
                runtime_id: None
            })),
            None
        );
        assert_eq!(
            call_runtime(&call_on(ToolSource::Mcp {
                server: "docs".into(),
                runtime_id: Some(runtime("unicorn")),
            })),
            Some(&runtime("unicorn"))
        );
        assert_eq!(
            call_runtime(&call_on(ToolSource::Acp {
                agent: cookie_agent_protocol::AgentId::new("claude").unwrap(),
                runtime_id: runtime("labpc"),
            })),
            Some(&runtime("labpc"))
        );
    }

    #[test]
    fn an_approval_names_an_acp_agents_own_server_and_not_cookie() {
        let mut state = SessionState::default();
        state.acp.agent = Some(cookie_agent_protocol::AgentId::new("claude").unwrap());
        let bridged = call_on(ToolSource::Runtime {
            runtime_id: runtime("unicorn"),
            attachment: AttachmentName::new("work").unwrap(),
            capability: CapabilityName::coding(),
        });
        assert_eq!(crate::acp::tool_source_label(&state, &bridged), None);
        let mut own = call_on(ToolSource::Acp {
            agent: cookie_agent_protocol::AgentId::new("claude").unwrap(),
            runtime_id: runtime("labpc"),
        });
        own.presentation.title = SafeDisplayText::new("mcp__playwright__click").unwrap();
        assert_eq!(
            crate::acp::tool_source_label(&state, &own).as_deref(),
            Some("playwright")
        );
    }

    #[test]
    fn short_names_drop_only_what_is_shared() {
        let shorts = |names: &[&'static str]| {
            let set = names.iter().copied().collect::<BTreeSet<_>>();
            names
                .iter()
                .map(|name| short_names(&set)[name])
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shorts(&["yunhao-labpc", "yunhao-labserver", "unicorn"]),
            ["labpc", "labserver", "unicorn"]
        );
        // No separator inside what they share: nothing is cut mid-word.
        assert_eq!(shorts(&["labpc", "labserver"]), ["labpc", "labserver"]);
        // Two machines the cut would make alike stay whole.
        assert_eq!(
            shorts(&["a-foo", "a-bar", "b-foo", "b-bar"]),
            ["a-foo", "a-bar", "b-foo", "b-bar"]
        );
        // A name that is the shared prefix itself stays whole.
        assert_eq!(shorts(&["a-", "a-b"]), ["a-", "b"]);
    }

    #[test]
    fn a_work_row_names_each_machine_once_then_counts() {
        let listing = machines(&[
            ("a", "one"),
            ("b", "one"),
            ("c", "two"),
            ("d", "three"),
            ("e", "four"),
        ]);
        let mut state = SessionState::default();
        for id in ["a", "b", "c", "d", "e"] {
            attach(&mut state, id);
        }
        let labels = MachineLabels::of_session(&state, Some(&listing));
        let order = ["b", "a", "c", "d", "e"].map(runtime);
        let (shown, more) = labels.work_row(&order);
        assert_eq!(
            shown
                .iter()
                .map(|label| label.name.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
        assert_eq!(more, 1);
    }

    #[test]
    fn the_key_changes_with_what_the_labels_show() {
        let mut state = SessionState::default();
        attach(&mut state, "local");
        attach(&mut state, "unicorn");
        let before = MachineLabels::of_session(&state, Some(&machines(TWO))).key();
        let renamed = machines(&[("local", "labserver"), ("unicorn", "unicorn")]);
        assert_ne!(
            before,
            MachineLabels::of_session(&state, Some(&renamed)).key()
        );
    }
}
