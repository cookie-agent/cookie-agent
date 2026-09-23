//! Agent, model, and variant drafts plus permission-mode state.

use super::*;

impl App {
    /// Root-selectable agents: exactly the descriptors with
    /// `runnable_as_root = true`.
    pub(in crate::ui) fn selectable_agents(&self) -> Vec<&AgentDescriptor> {
        let preset = self
            .draft
            .as_ref()
            .and_then(|draft| draft.preset.as_deref())
            .or_else(|| {
                self.selected_session_meta()
                    .and_then(|session| session.creation_selection.preset.as_deref())
            });
        self.root_agents_for_preset(preset)
    }

    pub(super) fn new_session_selectable_agents(&self) -> Vec<&AgentDescriptor> {
        self.root_agents_for_preset(self.selected_preset.as_deref())
    }

    pub(super) fn agent_picker_candidates(&self) -> Vec<&AgentDescriptor> {
        if self.new_session_draft.is_some() {
            self.new_session_selectable_agents()
        } else {
            self.selectable_agents()
        }
    }

    pub(in crate::ui) fn filtered_agent_picker_candidates(&self) -> Vec<&AgentDescriptor> {
        self.agent_picker_candidates()
            .into_iter()
            .filter(|agent| agent_matches(agent, self.agent_search.query()))
            .collect()
    }

    pub(super) fn root_agents_for_preset(&self, preset: Option<&str>) -> Vec<&AgentDescriptor> {
        self.agents
            .iter()
            .filter(|agent| {
                agent.runnable_as_root
                    && agent.mode != cookie_agent_protocol::AgentMode::Internal
                    && agent.preset.as_deref() == preset
            })
            .collect()
    }

    pub(in crate::ui) fn preset_names(&self) -> Vec<String> {
        let mut names = self
            .agents
            .iter()
            .filter_map(|agent| agent.preset.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    pub(super) fn selected_preset_label(&self) -> &str {
        self.selected_preset.as_deref().unwrap_or("shared")
    }

    pub(super) fn draft_selection_for_preset(
        &self,
        preset: Option<&str>,
        preferred_agent: Option<&AgentId>,
    ) -> Option<RunSelection> {
        let candidates = self
            .agents
            .iter()
            .filter(|agent| {
                agent.runnable_as_root
                    && agent.mode != cookie_agent_protocol::AgentMode::Internal
                    && agent.preset.as_deref() == preset
            })
            .collect::<Vec<_>>();
        let agent = preferred_agent
            .and_then(|id| candidates.iter().find(|agent| agent.id == *id).copied())
            .or_else(|| {
                candidates
                    .iter()
                    .find(|agent| agent.id.as_str() == "primary")
                    .copied()
            })
            .or_else(|| candidates.first().copied())?;
        let model = agent
            .resolved_fallback
            .iter()
            .find(|selection| self.selection_is_live(selection))
            .cloned()
            .or_else(|| self.models.first().map(Self::default_model_selection))?;
        Some(RunSelection {
            agent: agent.id.clone(),
            model,
            preset: preset.map(str::to_owned),
        })
    }

    pub(in crate::ui) fn default_draft_selection(&self) -> Option<RunSelection> {
        let agents = self.selectable_agents();
        let agent = agents
            .iter()
            .find(|agent| agent.id.as_str() == "primary")
            .or_else(|| agents.first())?;
        let model = agent
            .resolved_fallback
            .iter()
            .find(|selection| self.selection_is_live(selection))
            .cloned()
            .or_else(|| self.models.first().map(Self::default_model_selection))?;
        Some(RunSelection {
            agent: agent.id.clone(),
            model,
            preset: agent.preset.clone(),
        })
    }

    /// The metadata of the currently watched session, from the session list
    /// or the delegation tree.
    pub(in crate::ui) fn selected_session_meta(&self) -> Option<&SessionMeta> {
        let session_id = self.selected?;
        self.sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .or_else(|| {
                self.tree
                    .as_ref()
                    .and_then(|tree| find_session(tree, session_id))
            })
    }

    /// True while the watched session is a delegation root. Root sessions may
    /// draft/select any currently root-runnable primary/all agent between
    /// runs; delegated sessions are pinned to their frozen child agent.
    pub(in crate::ui) fn watching_root_session(&self) -> bool {
        self.selected_session_meta()
            .is_none_or(|meta| matches!(meta.origin, cookie_agent_protocol::SessionOrigin::Root))
    }

    /// Agent switching is allowed only for root sessions; delegated
    /// sessions are pinned to their frozen child agent. This gate is
    /// independent of the active run: draft changes affect the next run
    /// only, and active-run attribution stays frozen.
    pub(in crate::ui) fn agent_switching_allowed(&self) -> bool {
        self.watching_root_session()
    }

    /// Model draft changes are allowed whenever a draft exists — for
    /// delegated sessions within their frozen agent's persisted suffix.
    /// This gate is independent of the active run.
    pub(in crate::ui) fn model_selection_allowed(&self) -> bool {
        self.new_session_draft.is_some() || self.draft.is_some()
    }

    /// The frozen child agent a delegated session is pinned to, and the
    /// non-color reason the selector stays disabled.
    pub(in crate::ui) fn delegated_pin_reason(&self) -> Option<String> {
        let meta = self.selected_session_meta()?;
        if matches!(meta.origin, cookie_agent_protocol::SessionOrigin::Root) {
            return None;
        }
        Some(format!(
            "delegated session pinned to frozen child agent {}",
            meta.creation_selection.agent
        ))
    }

    /// Open a draft selector modal when allowed; otherwise surface the exact
    /// non-color reason it stays disabled. Agent switching is root-only;
    /// model selection stays available for delegated sessions inside their
    /// frozen agent's fallback chain. Neither is run-gated: drafts
    /// affect the next run only.
    pub(in crate::ui) fn open_selection_modal(&mut self, modal: Modal) {
        match modal {
            Modal::Agents
                if self.new_session_draft.is_none() && !self.agent_switching_allowed() =>
            {
                self.status = self
                    .delegated_pin_reason()
                    .unwrap_or_else(|| "agent switching requires a root session".into());
            }
            Modal::Models if !self.model_selection_allowed() => {
                self.status = if self.runtime.is_empty() {
                    EMPTY_RUNTIME_GUIDANCE.into()
                } else {
                    "no draft model is available for this session".into()
                };
            }
            _ => {
                match modal {
                    Modal::Agents => {
                        self.agent_search.reset();
                        let active_agent = self
                            .new_session_draft
                            .as_ref()
                            .or(self.draft.as_ref())
                            .map(|draft| &draft.agent);
                        let row = active_agent
                            .and_then(|agent| {
                                self.agent_picker_candidates()
                                    .iter()
                                    .position(|candidate| &candidate.id == agent)
                            })
                            .unwrap_or(0);
                        self.picker_state.select(Some(row));
                    }
                    Modal::Models => {
                        self.model_search.reset();
                        self.picker_state.select(Some(0));
                    }
                    _ => {
                        self.picker_state.select(Some(0));
                    }
                }
                self.modal = modal;
            }
        }
    }

    /// Revalidate a root draft against the current coherent descriptors while
    /// retaining every still-valid agent/model/variant choice. The producing
    /// agent of an active run is never reinterpreted.
    pub(in crate::ui) fn revalidate_draft(&mut self) {
        if !self.agent_switching_allowed() {
            return;
        }
        let Some(mut draft) = self.draft.clone() else {
            self.draft = self.default_draft_selection();
            return;
        };
        if !self.agents.iter().any(|agent| {
            agent.runnable_as_root && agent.id == draft.agent && agent.preset == draft.preset
        }) {
            self.draft = self.default_draft_selection();
            return;
        }
        let Some(descriptor) = self.model_descriptor(&draft.model.model) else {
            draft.model = self
                .preferred_model_for_agent(&draft.agent, draft.preset.as_deref())
                .or_else(|| self.models.first().map(Self::default_model_selection))
                .unwrap_or(draft.model);
            self.draft = Some(draft);
            return;
        };
        if !Self::variant_is_valid(descriptor, draft.model.variant.as_ref()) {
            draft.model.variant = descriptor.default_variant.clone();
        }
        self.draft = Some(draft);
    }

    pub(super) fn revalidate_new_session_draft(&mut self) {
        let Some(mut draft) = self.new_session_draft.clone() else {
            return;
        };
        let Some(agent) = self.agents.iter().find(|agent| {
            agent.runnable_as_root
                && agent.mode != cookie_agent_protocol::AgentMode::Internal
                && agent.id == draft.agent
                && agent.preset == draft.preset
        }) else {
            let preset = self
                .selected_preset
                .as_deref()
                .filter(|preset| self.preset_names().iter().any(|name| name == preset));
            self.new_session_draft = self.draft_selection_for_preset(preset, None);
            self.selected_preset = self
                .new_session_draft
                .as_ref()
                .and_then(|draft| draft.preset.clone());
            return;
        };
        if let Some(descriptor) = self.model_descriptor(&draft.model.model) {
            if !Self::variant_is_valid(descriptor, draft.model.variant.as_ref()) {
                draft.model.variant = descriptor.default_variant.clone();
            }
        } else {
            let Some(model) = agent
                .resolved_fallback
                .iter()
                .find(|selection| self.selection_is_live(selection))
                .cloned()
                .or_else(|| self.models.first().map(Self::default_model_selection))
            else {
                self.new_session_draft = None;
                self.selected_preset = None;
                return;
            };
            draft.model = model;
        }
        self.selected_preset = draft.preset.clone();
        self.new_session_draft = Some(draft);
    }

    pub(super) fn validated_draft_selection(&mut self) -> Option<RunSelection> {
        self.draft.as_ref()?;
        self.revalidate_draft();
        self.draft.clone()
    }

    pub(super) fn setup_status(&self) -> String {
        if self.runtime.is_empty() {
            return EMPTY_RUNTIME_GUIDANCE.into();
        }
        if self.runtime.phase() == RuntimePhase::Loading {
            return "loading runtime snapshot".into();
        }
        if self.runtime.phase() == RuntimePhase::ErrorRetry {
            return self
                .runtime
                .durable_explanation()
                .unwrap_or("runtime snapshot unavailable; retry")
                .into();
        }
        if self.agents.is_empty() {
            "No agents are configured; no session was created. Add an agent document, then restart or connect a provider."
                .into()
        } else {
            "No root-runnable agent is available; no session was created. Connect a provider for unresolved models or enable an agent with its own fallback chain."
                .into()
        }
    }

    pub(super) fn model_descriptor(&self, key: &ModelKey) -> Option<&AvailableModelDescriptor> {
        self.models.iter().find(|model| &model.key == key)
    }

    pub(super) fn default_model_selection(descriptor: &AvailableModelDescriptor) -> ModelSelection {
        ModelSelection {
            model: descriptor.key.clone(),
            variant: descriptor.default_variant.clone(),
        }
    }

    pub(super) fn variant_is_valid(
        descriptor: &AvailableModelDescriptor,
        variant: Option<&VariantId>,
    ) -> bool {
        variant.is_none_or(|variant| {
            descriptor
                .variants
                .iter()
                .any(|candidate| candidate.id == *variant)
        })
    }

    pub(super) fn selection_is_live(&self, selection: &ModelSelection) -> bool {
        self.model_descriptor(&selection.model)
            .is_some_and(|descriptor| {
                Self::variant_is_valid(descriptor, selection.variant.as_ref())
            })
    }

    pub(super) fn preferred_model_for_agent(
        &self,
        agent: &AgentId,
        preset: Option<&str>,
    ) -> Option<ModelSelection> {
        self.agents
            .iter()
            .find(|candidate| candidate.id == *agent && candidate.preset.as_deref() == preset)
            .and_then(|descriptor| {
                descriptor
                    .resolved_fallback
                    .iter()
                    .find(|selection| self.selection_is_live(selection))
                    .cloned()
            })
    }

    /// The authoritative exact selections for the watched delegated
    /// session: the retained `RunStarted.selected_suffix` directly (after
    /// any run-selection head-variant override), falling back to the
    /// creation snapshot's resolved suffix only before any run. Empty-chain
    /// inherited children carry the inherited exact suffix frozen at
    /// delegation admission. Live descriptors are never consulted.
    pub(super) fn persisted_chain(&self) -> Option<Vec<ModelSelection>> {
        if self.watching_root_session() {
            return None;
        }
        let session_id = self.selected?;
        let state = self.store.sessions.get(&session_id)?;
        if let Some(suffix) = &state.run_selected_suffix {
            return Some(
                suffix
                    .iter()
                    .map(|binding| binding.selection.clone())
                    .collect(),
            );
        }
        let snapshot = state.creation_agent.as_ref()?;
        let start = snapshot.selected_suffix_start as usize;
        Some(
            snapshot
                .fallback_chain
                .get(start..)
                .unwrap_or(&snapshot.fallback_chain)
                .iter()
                .map(|binding| binding.selection.clone())
                .collect(),
        )
    }

    /// The exact frozen variant selection for a model in the persisted
    /// delegated chain.
    pub(super) fn persisted_chain_selection(&self, model: &ModelKey) -> Option<ModelSelection> {
        self.persisted_chain().and_then(|chain| {
            chain
                .into_iter()
                .find(|selection| &selection.model == model)
        })
    }

    /// Models listed for the draft: every coherent global descriptor for root
    /// sessions; the persisted frozen suffix for delegated sessions.
    pub(in crate::ui) fn draft_models(&self) -> Vec<ModelSelection> {
        if self.new_session_draft.is_none() && !self.watching_root_session() {
            return self.persisted_chain().unwrap_or_default();
        }
        let draft = self.new_session_draft.as_ref().or(self.draft.as_ref());
        self.models
            .iter()
            .map(|descriptor| {
                draft
                    .filter(|draft| draft.model.model == descriptor.key)
                    .map_or_else(
                        || Self::default_model_selection(descriptor),
                        |draft| draft.model.clone(),
                    )
            })
            .collect()
    }

    pub(in crate::ui) fn filtered_draft_models(&self) -> Vec<ModelSelection> {
        self.draft_models()
            .into_iter()
            .filter(|selection| {
                model_matches(
                    selection,
                    self.model_descriptor(&selection.model),
                    self.model_search.query(),
                )
            })
            .collect()
    }

    /// Variant cycle for the selected draft model. Root order is exact base,
    /// then the descriptor's declared named-variant order. Delegated sessions
    /// expose only their exact persisted selection, so cycling cannot escape
    /// the suffix.
    pub(in crate::ui) fn draft_variants(&self) -> Vec<Option<VariantId>> {
        let Some(draft) = self.new_session_draft.as_ref().or(self.draft.as_ref()) else {
            return Vec::new();
        };
        self.variants_for(&draft.model.model)
    }

    /// The variant choices for `model` in the current draft context, in
    /// [`Self::draft_variants`] order.
    pub(in crate::ui) fn variants_for(&self, model: &ModelKey) -> Vec<Option<VariantId>> {
        if self.new_session_draft.is_none() && !self.watching_root_session() {
            return self
                .persisted_chain_selection(model)
                .map(|selection| vec![selection.variant])
                .unwrap_or_default();
        }
        let mut variants = vec![None];
        if let Some(descriptor) = self.model_descriptor(model) {
            let mut named = descriptor
                .variants
                .iter()
                .map(|variant| variant.id.clone())
                .collect::<Vec<_>>();
            named.sort();
            let mut declared = descriptor.variant_order.clone();
            let mut declared_sorted = declared.clone();
            declared_sorted.sort();
            if declared_sorted != named {
                declared = named;
            }
            variants.extend(declared.into_iter().map(Some));
        }
        variants
    }

    /// The producing agent of the active run, frozen by the accepted
    /// `RunStarted` event. Draft changes never alter it.
    pub(in crate::ui) fn active_run_agent(&self) -> Option<&AgentId> {
        let session_id = self.selected?;
        self.store
            .sessions
            .get(&session_id)
            .filter(|state| state.active_run.is_some())
            .and_then(|state| state.run_agent.as_ref())
    }

    pub(in crate::ui) fn set_draft_agent(&mut self, agent: AgentId) {
        let targets_new_session = self.new_session_draft.is_some();
        let current = if targets_new_session {
            self.new_session_draft.as_ref()
        } else {
            self.draft.as_ref()
        };
        let preset = current.and_then(|draft| draft.preset.as_deref());
        let Some(descriptor) = self.agents.iter().find(|candidate| {
            candidate.id == agent
                && candidate.runnable_as_root
                && candidate.preset.as_deref() == preset
        }) else {
            return;
        };
        let model = descriptor
            .resolved_fallback
            .iter()
            .find(|selection| self.selection_is_live(selection))
            .cloned()
            .or_else(|| self.models.first().map(Self::default_model_selection));
        let Some(model) = model else {
            return;
        };
        let selection = RunSelection {
            agent,
            model,
            preset: descriptor.preset.clone(),
        };
        if targets_new_session {
            self.status = format!("New session agent: {}", draft_title(&selection));
            self.new_session_draft = Some(selection);
        } else {
            self.draft = Some(selection);
            self.set_draft_reset_intent(true);
            self.status = self.draft_status("Draft run agent");
        }
    }

    pub(in crate::ui) fn set_draft_model(&mut self, model: ModelKey) {
        let targets_new_session = self.new_session_draft.is_some();
        let Some(draft) = self
            .new_session_draft
            .as_ref()
            .or(self.draft.as_ref())
            .cloned()
        else {
            return;
        };
        if draft.model.model == model {
            if (targets_new_session || self.watching_root_session())
                && draft.model.variant.is_none()
                && let Some(selection) = self
                    .model_descriptor(&model)
                    .map(Self::default_model_selection)
            {
                let updated = RunSelection {
                    agent: draft.agent,
                    model: selection,
                    preset: draft.preset,
                };
                if targets_new_session {
                    self.new_session_draft = Some(updated);
                } else {
                    self.draft = Some(updated);
                }
            }
            if !targets_new_session {
                self.set_draft_reset_intent(true);
            }
            self.status = self.draft_status("Draft run model");
            return;
        }
        // Delegated sessions resolve only against the persisted frozen
        // suffix; root sessions use the complete live catalog and select the
        // chosen model's resolved default variant.
        let selection = if targets_new_session
            || self.watching_root_session()
            || self.selected.is_none()
            || self.persisted_chain_selection(&draft.model.model).is_none()
            || self
                .agents
                .iter()
                .find(|agent| agent.id == draft.agent)
                .is_some_and(|agent| {
                    agent
                        .resolved_fallback
                        .iter()
                        .any(|candidate| candidate.model == model)
                }) {
            self.model_descriptor(&model)
                .map(Self::default_model_selection)
        } else {
            self.persisted_chain_selection(&model)
        };
        let Some(selection) = selection else {
            self.status = format!("model {model} is not available for agent {}", draft.agent);
            return;
        };
        let updated = RunSelection {
            agent: draft.agent,
            model: selection,
            preset: draft.preset,
        };
        if targets_new_session {
            self.new_session_draft = Some(updated);
        } else {
            self.draft = Some(updated);
        }
        if !targets_new_session {
            self.set_draft_reset_intent(true);
        }
        self.status = self.draft_status("Draft run model");
    }

    pub(in crate::ui) fn set_draft_variant(&mut self, variant: Option<VariantId>) {
        let targets_new_session = self.new_session_draft.is_some();
        let Some(draft) = self
            .new_session_draft
            .as_ref()
            .or(self.draft.as_ref())
            .cloned()
        else {
            return;
        };
        if !targets_new_session
            && !self.watching_root_session()
            && self
                .persisted_chain_selection(&draft.model.model)
                .is_none_or(|selection| selection.variant != variant)
        {
            return;
        }
        let updated = RunSelection {
            agent: draft.agent,
            model: ModelSelection {
                model: draft.model.model,
                variant,
            },
            preset: draft.preset,
        };
        if targets_new_session {
            self.new_session_draft = Some(updated);
        } else {
            self.draft = Some(updated);
        }
        if !targets_new_session {
            self.set_draft_reset_intent(true);
        }
        self.status = self.draft_status("Draft run variant");
    }

    pub(in crate::ui) fn cycle_draft_variant(&mut self) {
        let variants = self.draft_variants();
        if variants.len() <= 1 {
            return;
        }
        let Some(current) = self
            .new_session_draft
            .as_ref()
            .or(self.draft.as_ref())
            .map(|draft| draft.model.variant.clone())
        else {
            return;
        };
        let index = variants
            .iter()
            .position(|variant| *variant == current)
            .unwrap_or(0);
        self.set_draft_variant(variants[(index + 1) % variants.len()].clone());
    }

    pub(super) fn cycle_event_level_filter(&mut self) {
        self.tui_config.minimum_event_level = match self.tui_config.minimum_event_level {
            crate::state::EventLevel::Debug => crate::state::EventLevel::Info,
            crate::state::EventLevel::Info => crate::state::EventLevel::Warning,
            crate::state::EventLevel::Warning => crate::state::EventLevel::Error,
            crate::state::EventLevel::Error => crate::state::EventLevel::Debug,
        };
        self.status = format!(
            "Event level: {}",
            self.tui_config.minimum_event_level.name()
        );
    }

    pub(super) fn permission_mode_root(&self, session_id: SessionId) -> SessionId {
        let meta = self
            .sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .or_else(|| {
                self.tree
                    .as_ref()
                    .and_then(|tree| find_session(tree, session_id))
            });
        match meta.map(|meta| &meta.origin) {
            Some(cookie_agent_protocol::SessionOrigin::Delegated {
                root_session_id, ..
            }) => *root_session_id,
            _ => session_id,
        }
    }

    pub(super) fn permission_mode(&self, session_id: SessionId) -> PermissionMode {
        let root = self.permission_mode_root(session_id);
        self.permission_modes
            .get(&root)
            .copied()
            .unwrap_or_default()
    }

    pub(super) fn next_permission_mode_generation(&mut self, session_id: SessionId) -> u64 {
        let generation = self
            .permission_mode_generations
            .entry(session_id)
            .or_default();
        *generation = generation.wrapping_add(1);
        *generation
    }

    pub(super) fn cycle_permission_mode(&mut self) {
        let Some(selected) = self.selected else {
            return;
        };
        let session_id = self.permission_mode_root(selected);
        let previous = self.permission_mode(session_id);
        let mode = match previous {
            PermissionMode::AutoApprove => PermissionMode::AutoApproveN,
            PermissionMode::AutoApproveN => PermissionMode::AutoApproveY,
            PermissionMode::AutoApproveY => PermissionMode::Ask,
            PermissionMode::Ask => PermissionMode::Yolo,
            PermissionMode::Yolo => PermissionMode::AutoApprove,
        };
        let generation = self.next_permission_mode_generation(session_id);
        self.permission_modes.insert(session_id, mode);
        self.status = format!(
            "Permission mode: {} — applies to subsequent approvals in this session tree",
            permission_mode_label(mode)
        );
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .set_permission_mode(SessionSetPermissionModeParams { session_id, mode })
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            let _ = updates.send(RpcUpdate::PermissionModeMutationFinished {
                session_id,
                generation,
                result,
            });
        });
    }

    /// The coherent descriptor revision label projected from one runtime snapshot.
    pub(in crate::ui) fn descriptor_revisions_label(&self) -> String {
        match (&self.agent_revision, &self.model_revision) {
            (Some(agents), Some(models)) => {
                format!("agent revision {agents} · model revision {models}")
            }
            _ => "revisions unavailable".into(),
        }
    }

    pub(super) fn draft_status(&self, action: &str) -> String {
        let Some(draft) = self.new_session_draft.as_ref().or(self.draft.as_ref()) else {
            return "no draft selection".into();
        };
        let preset = draft.preset.as_deref().unwrap_or("shared");
        if self.new_session_draft.is_none() && self.active_run_agent().is_some() {
            format!(
                "{action}: {} · preset {preset}; applies to the next run — the active run is unchanged",
                draft_title(draft),
            )
        } else {
            format!("{action}: {} · preset {preset}", draft_title(draft))
        }
    }

    pub(in crate::ui) fn cycle_agent(&mut self, backward: bool) {
        if self.new_session_draft.is_none() && !self.agent_switching_allowed() {
            self.status = self
                .delegated_pin_reason()
                .unwrap_or_else(|| "agent switching requires a root session".into());
            return;
        }
        let selectable = self
            .agent_picker_candidates()
            .into_iter()
            .map(|agent| agent.id.clone())
            .collect::<Vec<_>>();
        if selectable.is_empty() {
            self.status = "no root-runnable agent is available".into();
            return;
        }
        let current = self
            .new_session_draft
            .as_ref()
            .or(self.draft.as_ref())
            .map(|draft| draft.agent.clone());
        let index = current.and_then(|id| selectable.iter().position(|agent| *agent == id));
        let next = match (index, backward) {
            (Some(index), true) => (index + selectable.len() - 1) % selectable.len(),
            (Some(index), false) => (index + 1) % selectable.len(),
            (None, true) => selectable.len() - 1,
            (None, false) => 0,
        };
        self.set_draft_agent(selectable[next].clone());
    }

    /// Warning rows from strict descendants of the viewed session, attributed
    /// to their owning session. Ownership stays durable in the child's own
    /// projection; this is a read-only aggregate for the current view.
    /// Warning rows from descendant sessions paired with the durable time of
    /// the event so the transcript can splice them at their chronological
    /// position in the viewed conversation.
    pub(in crate::ui) fn descendant_warnings(
        &self,
        viewed: SessionId,
    ) -> Vec<(jiff::Timestamp, String)> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let Some(node) = find_node(tree, viewed) else {
            return Vec::new();
        };
        let mut members = Vec::new();
        collect_subtree_sessions(node, &mut members);
        let mut warnings = Vec::new();
        for meta in members.into_iter().filter(|meta| meta.session_id != viewed) {
            let Some(state) = self.store.sessions.get(&meta.session_id) else {
                continue;
            };
            let source = meta
                .title
                .as_ref()
                .map(SessionTitle::to_string)
                .unwrap_or_else(|| meta.creation_selection.agent.to_string());
            for item in &state.transcript {
                if let TranscriptItem::Event {
                    level: crate::state::EventLevel::Warning,
                    text,
                    ..
                } = item
                {
                    // Pre-date rows (from before insertion times were tracked)
                    // keep their historical bottom-of-transcript position by
                    // sorting after every anchored item.
                    let time = state.item_time(item.id()).unwrap_or(jiff::Timestamp::MAX);
                    warnings.push((time, format!("from {source} ({}): {text}", short_id(&meta))));
                }
            }
        }
        warnings.sort_by_key(|(time, _)| *time);
        warnings
    }
}
