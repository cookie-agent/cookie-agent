//! The new session's runtime cards: what they offer from `runtimes.list`,
//! the selection's defaults, the machine groups the user opened, and Start,
//! which opens every selected workspace (`runtimes.reconnect`) at once and
//! leaves the draft to compose only once all of them opened.

use std::collections::HashMap;

use cookie_agent_client_state::{paths::RuntimeHomes, runtimes::DraftRuntime};
use cookie_agent_protocol::{
    RuntimeAttachRequest, RuntimeAttachment, RuntimeId, RuntimesReconnectParams, SessionId,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::{
    browser,
    cards::{
        Card, Group, SELECTION_KEY, Selection, cards, daemon_machine, group_key, groups,
        recent_from_storage, recent_key, remember, suggestions,
    },
};

impl Workspace {
    /// Every runtime's home, from `runtimes.list` (none before it answered,
    /// or from a daemon older than client protocol 1.7).
    pub fn homes(self) -> RuntimeHomes {
        self.runtimes
            .with(|listing| listing.as_ref().map(RuntimeHomes::from_listing))
            .unwrap_or_default()
    }

    /// The attachments of `session_id`'s root session, as the session list
    /// has them.
    pub(super) fn root_attachments(self, session_id: SessionId) -> Option<Vec<RuntimeAttachment>> {
        let root = self.tree_root.get_untracked().unwrap_or(session_id);
        self.sessions.with_untracked(|sessions| {
            sessions
                .iter()
                .find(|session| session.session_id == root)
                .map(|session| session.attachments.clone())
                .filter(|attachments| !attachments.is_empty())
        })
    }

    /// What a new session starts with: `open`'s attachments, else the
    /// browser's last selection, else `global` and `local` to fill in.
    pub(super) fn default_selection(self, open: Option<&[RuntimeAttachment]>) -> Selection {
        if let Some(attachments) = open {
            return Selection::of_session(attachments, &untrack(|| self.homes()));
        }
        browser::load(SELECTION_KEY)
            .as_deref()
            .and_then(Selection::from_storage)
            .unwrap_or_default()
    }

    /// The cards `runtimes.list` offers, in the groups' order.
    pub fn cards(self) -> Vec<Card> {
        self.card_groups()
            .into_iter()
            .flat_map(|group| group.cards)
            .collect()
    }

    /// The cards grouped by machine, this daemon's first.
    pub fn card_groups(self) -> Vec<Group> {
        self.runtimes
            .with(|listing| {
                listing
                    .as_ref()
                    .map(|listing| groups(&cards(listing), daemon_machine(listing).as_ref()))
            })
            .unwrap_or_default()
    }

    /// Whether `group` shows its cards.
    pub fn group_shown(self, group: &Group) -> bool {
        let name = group.name();
        let stored = self
            .group_open
            .with(|open| open.get(&name).copied())
            .map(|open| if open { "open" } else { "closed" }.to_owned())
            .or_else(|| browser::load(&group_key(&name)));
        self.selection
            .with(|selection| group.open(stored.as_deref(), selection))
    }

    /// Opens or closes `group`, as the browser remembers.
    pub fn toggle_group(self, group: &Group) {
        let open = !untrack(|| self.group_shown(group));
        let name = group.name();
        browser::store(&group_key(&name), if open { "open" } else { "closed" });
        self.group_open.update(|groups| {
            groups.insert(name, open);
        });
    }

    /// Selects or deselects `card`.
    pub fn toggle_card(self, card: &Card) {
        self.selection.update(|selection| {
            selection.toggle(card);
        });
        let key = card.key();
        self.card_errors
            .update(|errors| errors.retain(|(failed, _), _| *failed != key));
    }

    /// The workspaces a field of `runtime_id` suggests, as the fields show
    /// them: this browser's recent ones, then the sessions'.
    pub fn workspace_suggestions(self, runtime_id: &RuntimeId) -> Vec<String> {
        let recent =
            recent_from_storage(browser::load(&recent_key(runtime_id.as_str())).as_deref());
        let home = self.homes();
        self.sessions.with(|sessions| {
            suggestions(runtime_id.as_str(), &recent, sessions, home.get(runtime_id))
        })
    }

    /// Start: opens every selected workspace at once. A refusal shows on
    /// its card and nothing is created; once all of them opened, the
    /// draft composes with them, in card order.
    pub fn start_cards(self) {
        let Some(client) = self.client() else {
            return;
        };
        let cards = untrack(|| self.cards());
        let selection = self.selection.get_untracked();
        if let Some(blocker) = selection.blocker(&cards) {
            self.status.set(blocker);
            return;
        }
        let (global, requests) = selection.keyed_requests(&cards);
        self.card_errors.set(HashMap::new());
        self.starting.set(true);
        spawn_local(async move {
            let opened = futures_util::future::join_all(requests.iter().map(|(_, _, request)| {
                client.reconnect_runtime(RuntimesReconnectParams {
                    runtime_id: request.runtime_id.clone(),
                    capability: request.capability.clone(),
                    workspace: request.workspace.clone(),
                })
            }))
            .await;
            if !self.new_setup.get_untracked() {
                return;
            }
            let mut errors = HashMap::new();
            let mut instances = Vec::new();
            for ((key, index, request), result) in requests.into_iter().zip(opened) {
                match result {
                    Ok(result) => instances.push((request, result.instance)),
                    Err(error) => {
                        errors.insert((key, index), crate::errors::describe(&error));
                    }
                }
            }
            self.starting.set(false);
            if !errors.is_empty() {
                self.status.set(match errors.len() {
                    1 => "a runtime refused its workspace; see its card".into(),
                    count => format!("{count} workspaces did not open; see their cards"),
                });
                self.card_errors.set(errors);
                return;
            }
            let mut entries = Vec::new();
            if global {
                entries.push(self.draft_entry(
                    RuntimeAttachRequest {
                        runtime_id: RuntimeId::global(),
                        capability: Some(cookie_agent_protocol::CapabilityName::global()),
                        workspace: None,
                    },
                    None,
                ));
            }
            for (request, instance) in instances {
                if let Some(workspace) = &instance.workspace {
                    let key = recent_key(request.runtime_id.as_str());
                    let recent = recent_from_storage(browser::load(&key).as_deref());
                    if let Ok(stored) = serde_json::to_string(&remember(&recent, workspace)) {
                        browser::store(&key, &stored);
                    }
                }
                let request = RuntimeAttachRequest {
                    capability: Some(instance.capability.clone()),
                    workspace: instance.workspace.clone(),
                    ..request
                };
                // The same instance twice (two spellings of one workspace)
                // attaches once.
                if !entries
                    .iter()
                    .any(|entry: &DraftRuntime| entry.same_instance(&request))
                {
                    entries.push(self.draft_entry(request, Some(instance)));
                }
            }
            browser::store(SELECTION_KEY, &selection.to_storage());
            self.draft_runtimes.set(Some(entries));
            self.new_setup.set(false);
            self.status
                .set("the workspaces opened; the first message starts the session".into());
        });
    }

    fn draft_entry(
        self,
        request: RuntimeAttachRequest,
        instance: Option<cookie_agent_protocol::RuntimeInstanceInfo>,
    ) -> DraftRuntime {
        self.next_draft_runtime.update_value(|next| *next += 1);
        DraftRuntime {
            id: self.next_draft_runtime.get_value(),
            request,
            opened: instance.map(Ok),
        }
    }

    /// Back from the draft to the cards, keeping the selection.
    pub fn edit_cards(self) {
        self.draft_runtimes.set(None);
        self.card_errors.set(HashMap::new());
        self.new_setup.set(true);
    }

    /// The runtimes the bottom bar's workspaces are on, each once, in
    /// attach order: their colours mark the label.
    pub fn places_runtimes(self) -> Vec<String> {
        let mut runtimes = Vec::<String>::new();
        let mut add = |runtime_id: &RuntimeId| {
            if !runtimes.iter().any(|known| known == runtime_id.as_str()) {
                runtimes.push(runtime_id.to_string());
            }
        };
        if self.new_session.get() {
            self.draft_runtimes.with(|entries| {
                for entry in entries.iter().flatten() {
                    if entry.workspace().is_some() {
                        add(&entry.request.runtime_id);
                    }
                }
            });
            return runtimes;
        }
        let Some(session_id) = self.tree_root.get().or(self.selected.get()) else {
            return runtimes;
        };
        self.sessions.with(|sessions| {
            let attachments = sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(|session| session.attachments.as_slice())
                .unwrap_or_default();
            for attachment in attachments {
                if attachment.workspace.is_some() {
                    add(&attachment.runtime_id);
                }
            }
        });
        runtimes
    }

    /// What the bottom bar says of where the viewed session (or the draft)
    /// works: its workspaces under their runtimes' homes; `None` before it
    /// has any runtimes.
    pub fn places_label(self) -> Option<String> {
        let homes = self.homes();
        if self.new_session.get() {
            return self.draft_runtimes.with(|entries| {
                let entries = entries.as_ref()?;
                let shown = entries
                    .iter()
                    .filter_map(|entry| {
                        let workspace = entry.workspace()?;
                        let path = homes.display(&entry.request.runtime_id, workspace);
                        Some(if entry.request.runtime_id.as_str() == RuntimeId::LOCAL {
                            path
                        } else {
                            format!("{}:{path}", entry.request.runtime_id)
                        })
                    })
                    .collect::<Vec<_>>();
                Some(if shown.is_empty() {
                    "no workspace".to_owned()
                } else {
                    shown.join(", ")
                })
            });
        }
        let session_id = self.tree_root.get().or(self.selected.get())?;
        self.sessions.with(|sessions| {
            sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(|session| {
                    crate::places::places_label(session, &homes, self.lists_attachments.get())
                })
        })
    }
}
