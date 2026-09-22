//! Frame composition and panel rendering for [`App`].

use super::*;

impl App {
    /// The exact Message panel title `Agent • Model[Variant]` with separate
    /// structured agent, model, and bracketed variant hit regions. Only the
    /// agent name is bold — typographic emphasis, never a color marker. The
    /// separator, model, and variant subtract the bold the focused border
    /// would otherwise lend them, so they stay regular.
    pub(super) fn message_title_spans(&self) -> Vec<Span<'static>> {
        let regular = Style::default().remove_modifier(Modifier::BOLD);
        match self.new_session_draft.as_ref().or(self.draft.as_ref()) {
            Some(draft) => vec![
                Span::styled(
                    draft.agent.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" • ", regular),
                Span::styled(draft.model.model.to_string(), regular),
                Span::styled(
                    format!(
                        "[{}]",
                        draft
                            .model
                            .variant
                            .as_ref()
                            .map_or_else(|| "base".to_owned(), |variant| variant.to_string())
                    ),
                    regular,
                ),
            ],
            None => {
                let text = match self.runtime.phase() {
                    RuntimePhase::Loading => "loading runtime snapshot",
                    RuntimePhase::Empty => EMPTY_RUNTIME_GUIDANCE,
                    RuntimePhase::ErrorRetry => "runtime error — retry",
                    RuntimePhase::Ready | RuntimePhase::Stale | RuntimePhase::Bootstrap => {
                        "select an agent and model"
                    }
                };
                // Raw text keeps the border accent inheritance, exactly as
                // the styled composition does for its regular segments.
                vec![Span::raw(text.to_owned())]
            }
        }
    }

    pub(in crate::ui) fn draw(&mut self, frame: &mut ratatui::Frame) {
        // The warm cream surface is painted beneath everything first so
        // unstyled cells still land on the light theme; overlays then paint
        // their own panels over it instead of clearing to the terminal.
        frame.render_widget(Block::default().style(self.theme.surface()), frame.area());
        self.hit_map.clear();
        // The composer takes one text row by default and grows with its
        // soft-wrapped content up to the ceiling; the layout reclaims those
        // rows from the conversation pane. The row count comes from the same
        // scrollbar-aware wrap width the renderer uses, so the box never
        // changes height (or re-wraps) because the track appeared.
        let input_text_rows = u16::try_from(
            self.input
                .composer_rows(frame.area().width.saturating_sub(2)),
        )
        .unwrap_or(u16::MAX)
        .clamp(1, crate::ui::input::MAX_TEXT_ROWS);
        let tree_entries = self.tree_entries();
        let tree_rows = if self.agent_panel_visible() {
            tree_entries.len().max(2)
        } else {
            0
        };
        let layout = crate::ui::terminal_layout_with_tree_rows(
            frame.area(),
            tree_rows,
            self.queue_strip_height(),
            self.goal_bar_visible(),
            input_text_rows,
        );
        self.render_tree(frame, layout.agent, &tree_entries);
        self.render_conversation(frame, layout.conversation);
        self.render_queue_strip(frame, layout.queue);
        self.render_goal_bar(frame, layout.goal);
        let title_spans = self.message_title_spans();
        let rendered_input = crate::ui::input::render(
            frame,
            layout.input,
            &mut self.input,
            self.input_focused
                && self.goal_focus.is_none()
                && self.modal == Modal::None
                && self.selected.is_none_or(|session| {
                    !self.read_only_sessions.contains(&session) || self.new_session_draft.is_some()
                }),
            Line::from(title_spans.clone()),
            Some(
                if self
                    .selected
                    .is_some_and(|session| self.read_only_sessions.contains(&session))
                    && self.new_session_draft.is_none()
                {
                    "Read-only snapshot"
                } else {
                    "Type a message · / for commands"
                },
            ),
            &self.theme,
        );
        // Agent, Model, and the complete bracketed Variant suffix are separate
        // clickable regions inside the canonical title. The bullet is decoration.
        self.hit_map.title_segments = if self.new_session_draft.is_none() && self.draft.is_none() {
            Vec::new()
        } else {
            let segments = [
                Some(TitleSegment::Agent),
                None,
                Some(TitleSegment::Model),
                Some(TitleSegment::Variant),
            ];
            rendered_input
                .title_rect
                .map_or_else(Vec::new, |title_rect| {
                    let mut hits = Vec::new();
                    let mut column = title_rect.x;
                    let visible_end = title_rect.x.saturating_add(title_rect.width);
                    for (span, segment) in title_spans.iter().zip(segments) {
                        let width = UnicodeWidthStr::width(span.content.as_ref())
                            .min(usize::from(u16::MAX)) as u16;
                        let visible_width = visible_end.saturating_sub(column).min(width);
                        if let Some(segment) = segment
                            && visible_width > 0
                        {
                            hits.push(TitleSegmentHit {
                                rect: Rect::new(column, title_rect.y, visible_width, 1),
                                segment,
                            });
                        }
                        column = column.saturating_add(width);
                    }
                    hits
                })
        };
        self.hit_map.input = Some(InputHit {
            rect: layout.input,
            text_rect: rendered_input.text_rect,
            scrollbar: rendered_input.scrollbar,
        });
        let mut base_status = if self.pending_approval.is_some() {
            "Approval submitting…".to_owned()
        } else {
            self.status.clone()
        };
        if let Some(explanation) = self.runtime.durable_explanation()
            && !base_status.contains(explanation)
        {
            base_status = format!("{explanation} · {base_status}");
        }
        // The scroll-follow state lives in the Conversation title, not here.
        let status = truncate_with_ellipsis(&base_status, usize::from(layout.status.width));
        // Status and bottom bar share the one cream surface with the input.
        frame.render_widget(
            Paragraph::new(Span::styled(status, self.theme.muted())).style(self.theme.panel()),
            layout.status,
        );
        let bottom_bar = self.bottom_bar_line(layout.bar.width);
        let span_rect = |target_span| {
            let mut column = layout.bar.x;
            for (index, span) in bottom_bar.line.spans.iter().enumerate() {
                let width =
                    UnicodeWidthStr::width(span.content.as_ref()).min(usize::from(u16::MAX)) as u16;
                if index == target_span {
                    let visible = layout
                        .bar
                        .x
                        .saturating_add(layout.bar.width)
                        .saturating_sub(column)
                        .min(width);
                    return (visible > 0).then(|| Rect::new(column, layout.bar.y, visible, 1));
                }
                column = column.saturating_add(width);
            }
            None
        };
        self.hit_map.permission_mode = bottom_bar.mode_span.and_then(span_rect);
        self.hit_map.session_cost = bottom_bar.cost_span.and_then(span_rect);
        frame.render_widget(
            Paragraph::new(bottom_bar.line).style(self.theme.panel()),
            layout.bar,
        );
        if let Some(approval) = self.current_approval().cloned() {
            let area = centered(frame.area(), 76, 40);
            self.hit_map.approval = Some(area);
            self.hit_map.approval_actions = self.render_approval(frame, &approval, area);
        }
        match self.modal {
            Modal::GoalDetail => self.render_goal_detail(frame),
            Modal::Sessions => {
                self.render_session_search(frame, centered(frame.area(), 72, 60));
            }
            Modal::Agents => {
                // Normal delegated-session selection is pinned to its frozen
                // child agent. `/new` always owns an independent root draft.
                if self.new_session_draft.is_none()
                    && let Some(pin) = self.delegated_pin_reason()
                {
                    let agent = self
                        .selected_session_meta()
                        .map(|meta| meta.creation_selection.agent.to_string())
                        .unwrap_or_default();
                    let description = self
                        .agents
                        .iter()
                        .find(|candidate| {
                            candidate.id.as_str() == agent
                                && candidate.preset
                                    == self
                                        .selected_session_meta()
                                        .and_then(|meta| meta.creation_selection.preset.clone())
                        })
                        .map(|candidate| candidate.description.clone())
                        .unwrap_or_else(|| "frozen child agent".into());
                    self.render_picker(
                        frame,
                        "Agent — fixed (delegated session)",
                        vec![format!("{agent} — {description}"), pin],
                        None,
                        centered(frame.area(), 56, 44),
                        Some("esc: close"),
                    );
                } else {
                    self.render_agent_picker(frame, centered(frame.area(), 56, 44));
                }
            }
            Modal::Presets => {
                let mut entries = vec!["None (shared)".to_owned()];
                entries.extend(self.preset_names());
                self.render_picker(
                    frame,
                    "Agent preset — next root run and future new sessions",
                    entries,
                    None,
                    centered(frame.area(), 48, 40),
                    Some("↑↓ move · enter: select · esc: close"),
                );
            }
            Modal::Models => {
                self.render_model_picker(frame, centered(frame.area(), 56, 44));
            }
            Modal::ConnectProviders => {
                let match_count = self.filtered_providers().len();
                let provider_count = self.providers.len();
                let title =
                    format!("Connect provider ({match_count}/{provider_count}) · Enter: details");
                let area = centered(frame.area(), 78, 64);
                paint_panel(frame, area, &self.theme);
                let input_height = 3.min(area.height);
                let input_area = Rect::new(area.x, area.y, area.width, input_height);
                let rendered_input = crate::ui::pickers::render_search_input(
                    frame,
                    input_area,
                    &mut self.provider_search,
                    &self.theme,
                );
                self.hit_map.picker_input = Some(InputHit {
                    rect: input_area,
                    text_rect: rendered_input.text_rect,
                    // Single-row search inputs never reach the composer
                    // ceiling, so they never carry a scrollbar.
                    scrollbar: None,
                });
                let remaining = Rect::new(
                    area.x,
                    area.y.saturating_add(input_height),
                    area.width,
                    area.height.saturating_sub(input_height),
                );
                let copy_height = 3.min(remaining.height);
                let copy = Rect::new(remaining.x, remaining.y, remaining.width, copy_height);
                frame.render_widget(
                    Paragraph::new(DURABLE_PROVIDER_COPY)
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(self.theme.panel_border())
                                .title("Global provider store"),
                        ),
                    copy,
                );
                let picker = Rect::new(
                    remaining.x,
                    remaining.y.saturating_add(copy_height),
                    remaining.width,
                    remaining.height.saturating_sub(copy_height),
                );
                self.render_picker(
                    frame,
                    &title,
                    self.filtered_providers()
                        .iter()
                        .map(|provider| {
                            row_label(
                                provider,
                                &self.models,
                                self.provider_operations.get(&provider.id),
                            )
                        })
                        .collect(),
                    if self.providers.is_empty() {
                        Some("No providers are available in the runtime snapshot.")
                    } else if match_count == 0 {
                        Some("No providers match the filter.")
                    } else {
                        None
                    },
                    picker,
                    Some("↑↓ move · enter: details · esc: close"),
                );
            }
            Modal::ConnectDetails => {
                self.render_connect_details(frame, centered(frame.area(), 80, 62));
            }
            Modal::ConnectSetup => {
                self.render_connect_setup(frame, centered(frame.area(), 86, 86));
            }
            Modal::ConnectError => {
                self.render_connect_error(frame, centered(frame.area(), 86, 80));
            }
            Modal::DisconnectConfirm => {
                self.render_disconnect_confirm(frame, centered(frame.area(), 72, 42));
            }
            Modal::UserMessage => {
                self.render_user_menu(frame, centered(frame.area(), 52, 26));
            }
            Modal::RevertConfirm => {
                self.render_revert_confirm(frame, centered(frame.area(), 64, 30));
            }
            Modal::Mcp => {
                crate::ui::management::render_mcp(
                    frame,
                    centered(frame.area(), 88, 82),
                    &mut self.mcp_panel,
                    &self.theme,
                );
            }
            Modal::Permissions => {
                crate::ui::management::render_permissions(
                    frame,
                    centered(frame.area(), 82, 76),
                    &mut self.permission_panel,
                    &self.theme,
                );
            }
            Modal::Skills => {
                crate::ui::management::render_skills(
                    frame,
                    centered(frame.area(), 88, 76),
                    &mut self.skill_panel,
                    &self.theme,
                );
            }
            Modal::Usage => {
                crate::ui::management::render_usage(
                    frame,
                    centered(frame.area(), 86, 78),
                    &mut self.usage_panel,
                    &self.theme,
                );
            }
            Modal::None => {}
        }
        if self.command_palette_visible() {
            self.render_command_palette(frame, centered(frame.area(), 68, 60));
        }
        // Selection sits beneath hover: both are pure cell-style patches,
        // and the hover affordance always wins where they overlap.
        self.apply_selection(frame);
        // Hover is the very last pass: a pure cell-style patch over whatever
        // was rendered, so it can never change layout or hit geometry.
        self.apply_hover(frame);
        narrow_emoji_presentation(frame.buffer_mut());
        fence_emoji_spill(frame.buffer_mut());
    }

    pub(super) fn bottom_bar_line(&self, width: u16) -> BottomBarRender {
        let width = usize::from(width);
        if width == 0 {
            return BottomBarRender {
                line: Line::default(),
                mode_span: None,
                cost_span: None,
            };
        }

        let cwd = self
            .selected_session_meta()
            .map(|meta| meta.cwd_identity.as_str())
            .or_else(|| {
                self.selected
                    .and_then(|session_id| self.store.sessions.get(&session_id))
                    .and_then(|state| state.cwd_identity.as_ref())
                    .map(cookie_agent_protocol::CwdIdentity::as_str)
            })
            .map(shorten_home)
            .unwrap_or_else(|| "—".into());

        let cwd = self.working_state().map_or(cwd, |working| {
            let glyph = ['◐', '◓', '◑', '◒'][usize::from(self.clock_bucket())];
            let label = match working {
                WorkingState::Working => "working".to_owned(),
                WorkingState::Queued(count) => format!("{count} queued"),
            };
            format!("{glyph} {label}")
        });

        let state = self
            .selected
            .and_then(|session_id| self.store.sessions.get(&session_id));
        let context_tokens = state.and_then(|state| state.context_tokens);
        // No token data → no context segment at all; a bare dash would be
        // noise. The percentage degrades away before the count does.
        let context = context_tokens.map(|tokens| format!("ctx {}", format_token_count(tokens)));
        let context_limit = self
            .draft
            .as_ref()
            .and_then(|draft| self.model_descriptor(&draft.model.model))
            .or_else(|| {
                state
                    .and_then(latest_resolved_model_key)
                    .and_then(|key| self.model_descriptor(key))
            })
            .map(|descriptor| descriptor.capabilities.context_tokens);
        let context_with_percentage = match (context_tokens, context_limit) {
            (Some(tokens), Some(limit)) => {
                let percentage = tokens.saturating_mul(100).saturating_add(limit / 2) / limit;
                context
                    .as_ref()
                    .map(|context| format!("{context} ({percentage}%)"))
            }
            _ => context.clone(),
        };
        let cost = state
            .and_then(|state| state.estimated_cost_usd)
            .map(format_cost_usd);
        let mode = self
            .selected
            .map(|session_id| self.permission_mode(session_id))
            .unwrap_or_default();
        let mode = permission_mode_label(mode);
        let hint = "`ctrl+p` commands";
        #[derive(Clone, Copy)]
        struct Candidate<'a> {
            cost: Option<&'a str>,
            context: Option<&'a str>,
            hint: bool,
        }
        let render_candidate = |candidate: Candidate<'_>| {
            let mut rendered = mode.to_owned();
            if let Some(cost) = candidate.cost {
                rendered.push_str("    ");
                rendered.push_str(cost);
            }
            if let Some(context) = candidate.context {
                rendered.push_str("    ");
                rendered.push_str(context);
            }
            if candidate.hint {
                rendered.push_str("    ");
                rendered.push_str(hint);
            }
            rendered
        };
        let mut candidates = Vec::with_capacity(5);
        candidates.push(Candidate {
            cost: cost.as_deref(),
            context: context_with_percentage.as_deref(),
            hint: true,
        });
        candidates.push(Candidate {
            cost: cost.as_deref(),
            context: context_with_percentage.as_deref(),
            hint: false,
        });
        if cost.is_some() && context_with_percentage.is_some() {
            candidates.push(Candidate {
                cost: None,
                context: context_with_percentage.as_deref(),
                hint: false,
            });
        }
        if context_with_percentage != context {
            candidates.push(Candidate {
                cost: None,
                context: context.as_deref(),
                hint: false,
            });
        }
        candidates.push(Candidate {
            cost: None,
            context: None,
            hint: false,
        });
        let selected = candidates.into_iter().find(|candidate| {
            UnicodeWidthStr::width(render_candidate(*candidate).as_str()) <= width
        });
        let right = selected.map_or_else(|| truncate_with_ellipsis(mode, width), render_candidate);
        let right_width = UnicodeWidthStr::width(right.as_str()).min(width);
        let right_start = width.saturating_sub(right_width);
        let left_width = right_start.saturating_sub(4);
        let left = truncate_with_ellipsis(&cwd, left_width);
        let padding = right_start.saturating_sub(UnicodeWidthStr::width(left.as_str()));
        let mut spans = vec![Span::styled(
            format!("{left}{}", " ".repeat(padding)),
            self.theme.muted(),
        )];
        let mode_text = selected.map_or(right.as_str(), |_| mode);
        let mode_span = (!mode_text.is_empty()).then_some(spans.len());
        spans.push(Span::styled(mode_text.to_owned(), self.theme.link()));
        let mut cost_span = None;
        if let Some(candidate) = selected {
            if let Some(cost) = candidate.cost {
                spans.push(Span::styled("    ", self.theme.muted()));
                cost_span = Some(spans.len());
                spans.push(Span::styled(cost.to_owned(), self.theme.muted()));
            }
            if let Some(context) = candidate.context {
                spans.push(Span::styled("    ", self.theme.muted()));
                spans.push(Span::styled(context.to_owned(), self.theme.muted()));
            }
            if candidate.hint {
                spans.push(Span::styled("    ", self.theme.muted()));
                spans.push(Span::styled(hint, self.theme.muted()));
            }
        }
        BottomBarRender {
            line: Line::from(spans),
            mode_span,
            cost_span,
        }
    }

    #[cfg(test)]
    pub(crate) fn draw_for_test(&mut self, frame: &mut ratatui::Frame) {
        self.draw(frame);
    }

    pub(in crate::ui) fn render_tree(
        &mut self,
        frame: &mut ratatui::Frame,
        area: Rect,
        entries: &[(SessionId, SessionMeta, usize)],
    ) {
        if area.height == 0 {
            self.tree_viewport_height = 0;
            self.hit_map.tree = None;
            self.hit_map.tree_rows.clear();
            return;
        }
        // The Agents panel has exactly clamp(visible row count, 1,
        // MAX_AGENT_PANEL_ROWS) text rows, with its borders outside that
        // count; a longer tree scrolls within those rows.
        let text_rows = entries.len().clamp(1, crate::ui::MAX_AGENT_PANEL_ROWS) as u16;
        let panel_height = text_rows.saturating_add(2).min(area.height);
        let panel = Rect::new(area.x, area.y, area.width, panel_height);
        let inner = inner_rect(panel);
        self.tree_viewport_height = usize::from(inner.height);
        if self.tree_cursor.is_none() {
            self.tree_cursor = self
                .selected
                .filter(|selected| entries.iter().any(|(id, _, _)| id == selected))
                .or_else(|| entries.first().map(|(id, _, _)| *id));
        }
        self.clamp_tree_view_with(entries);
        let cursor_index = self.tree_cursor_index(entries);
        self.hit_map.tree = Some(inner);
        self.hit_map.tree_rows = entries
            .iter()
            .enumerate()
            .skip(self.tree_offset)
            .take(usize::from(inner.height))
            .enumerate()
            .map(|(row, (_, (session_id, _, depth)))| {
                // Expand geometry is projected from immutable hierarchy data,
                // never from cursor/watch prefixes that change with focus.
                let expand_rect = (*depth > 0
                    && self
                        .tree
                        .as_ref()
                        .and_then(|tree| find_node(tree, *session_id))
                        .is_some_and(|node| !node.children.is_empty()))
                .then(|| {
                    let indent_column = 2usize.saturating_mul(depth.saturating_sub(1));
                    Rect::new(
                        inner
                            .x
                            .saturating_add(2)
                            .saturating_add(u16::try_from(indent_column).unwrap_or(u16::MAX)),
                        inner.y + u16::try_from(row).unwrap_or(u16::MAX),
                        1,
                        1,
                    )
                });
                TreeRowHit {
                    rect: Rect::new(
                        inner.x,
                        inner.y + u16::try_from(row).unwrap_or(u16::MAX),
                        inner.width,
                        1,
                    ),
                    session_id: *session_id,
                    expand_rect,
                }
            })
            .collect();
        let rows = entries
            .iter()
            .enumerate()
            .skip(self.tree_offset)
            .take(usize::from(inner.height))
            .map(|(index, entry)| {
                let label = self.tree_row_label(entry, cursor_index == Some(index));
                // Rows render in plain body styling: the watched session is
                // marked only by its `●` glyph, never a persistent color.
                // The keyboard cursor keeps its `>` marker plus assistant
                // accent, and click-action hover keeps the glaze patch.
                let mut line = Line::from(Span::styled(label, self.theme.body()));
                if cursor_index == Some(index) {
                    line = line.style(self.theme.assistant());
                }
                line
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            frame.render_widget(
                List::new(vec!["No sessions yet · /new starts one"])
                    .style(self.theme.muted())
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(self.theme.panel_border())
                            .title("Agents"),
                    ),
                panel,
            );
            return;
        }
        frame.render_widget(
            List::new(rows).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.theme.panel_border())
                    .title("Agents"),
            ),
            panel,
        );
    }

    /// One tree row: exactly `agent-id:session-title` with the shortened ID
    /// as subdued secondary metadata. The watched session gets a `●` marker
    /// and the cursor a `>` marker; the expand marker sits at the row's
    /// indent depth so its hit region is stable.
    pub(in crate::ui) fn tree_row_label(
        &self,
        (session_id, session, depth): &(SessionId, SessionMeta, usize),
        cursor: bool,
    ) -> String {
        let has_children = self
            .tree
            .as_ref()
            .and_then(|tree| find_node(tree, *session_id))
            .is_some_and(|node| !node.children.is_empty());
        let marker = if !has_children {
            " "
        } else if self.collapsed_sessions.contains(session_id) {
            "+"
        } else {
            "-"
        };
        let indent = if *depth == 0 {
            String::new()
        } else {
            format!("{}{} ", "  ".repeat(depth - 1), marker)
        };
        let watched = if self.selected == Some(*session_id) {
            "● "
        } else {
            // Keep the watch-marker column reserved on every row. Without
            // this padding, selecting the root removes two leading cells from
            // every unselected descendant and visually cancels one depth.
            "  "
        };
        let cursor = if cursor { "> " } else { "  " };
        let status = match session.status {
            SessionStatus::Running => "⏳ ",
            SessionStatus::Idle
            | SessionStatus::Completed
            | SessionStatus::Failed
            | SessionStatus::Cancelled
            | SessionStatus::Interrupted => "   ",
        };
        let title = session
            .title
            .as_ref()
            .map(SessionTitle::to_string)
            .unwrap_or_else(|| "untitled".to_owned());
        let degraded = if session.skipped_events.is_empty() {
            ""
        } else {
            " !"
        };
        // Primary text is exactly `agent-id:session-title`; hierarchy,
        // cursor, and watch markers live in prefix cells only, and the row
        // shows no session ID.
        let agent = if *depth == 0 && self.selected == Some(*session_id) {
            self.draft
                .as_ref()
                .map(|draft| draft.agent.clone())
                .unwrap_or_else(|| session.creation_selection.agent.clone())
        } else {
            session.creation_selection.agent.clone()
        };
        format!("{cursor}{indent}{watched}{status}{agent}:{title}{degraded}",)
    }

    pub(in crate::ui) fn tree_entries(&self) -> Vec<(SessionId, SessionMeta, usize)> {
        let mut entries = Vec::new();
        if let Some(tree) = &self.tree {
            flatten_tree(
                tree,
                0,
                &self.collapsed_sessions,
                &self.store.sessions,
                &mut entries,
            );
        }
        // A session whose event log still holds only `SessionCreated`
        // (`last_event_seq == 1`, so no `UserInputSubmitted` yet) is an
        // empty, memory-only ghost on the engine side: it never renders as
        // a panel row. The watched session stays fully usable while hidden
        // and its row appears normally once its first message lands. A
        // rename bumps the sequence via `SessionTitleCommitted`, so a named
        // empty session stays visible — an explicit user act, not a ghost.
        entries.retain(|(_, meta, _)| meta.last_event_seq > 1);
        entries
    }

    pub(super) fn tree_has_live_delegated_agents(&self) -> bool {
        fn has_live_descendant(tree: &SessionTree) -> bool {
            tree.children.iter().any(|child| {
                matches!(
                    child.session.status,
                    SessionStatus::Idle | SessionStatus::Running
                ) || has_live_descendant(child)
            })
        }

        self.tree.as_ref().is_some_and(has_live_descendant)
    }

    pub(super) fn agent_panel_visible(&self) -> bool {
        match self.agent_panel_mode {
            AgentPanelMode::Auto => {
                self.tree_has_live_delegated_agents() || !self.watching_root_session()
            }
            AgentPanelMode::Shown => true,
            AgentPanelMode::Hidden => false,
        }
    }

    pub(super) fn command_is_available(&self, spec: &CommandSpec) -> bool {
        match spec.name {
            "show" => !self.agent_panel_visible(),
            "hide" => self.agent_panel_visible(),
            _ => true,
        }
    }

    pub(in crate::ui) fn render_picker(
        &mut self,
        frame: &mut ratatui::Frame,
        title: &str,
        entries: Vec<String>,
        empty_message: Option<&str>,
        area: Rect,
        hint: Option<&str>,
    ) {
        self.clamp_picker_selection();
        self.hit_map.picker = Some(area);
        self.hit_map.picker_rows = crate::ui::pickers::render(
            frame,
            crate::ui::pickers::PickerChrome {
                title,
                empty_message,
                hint,
            },
            entries,
            area,
            &mut self.picker_state,
            &self.theme,
        )
        .into_iter()
        .map(|(rect, index)| PickerRowHit { rect, index })
        .collect();
    }

    pub(super) fn render_agent_picker(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let input_height = 3.min(area.height);
        let input_area = Rect::new(area.x, area.y, area.width, input_height);
        let rendered_input = crate::ui::pickers::render_search_input(
            frame,
            input_area,
            &mut self.agent_search,
            &self.theme,
        );
        self.hit_map.picker_input = Some(InputHit {
            rect: input_area,
            text_rect: rendered_input.text_rect,
            scrollbar: None,
        });

        let picker = Rect::new(
            area.x,
            area.y.saturating_add(input_height),
            area.width,
            area.height.saturating_sub(input_height),
        );
        self.clamp_picker_selection();
        let selected = self.picker_state.selected();
        let total = self.agent_picker_candidates().len();
        let agents = self.filtered_agent_picker_candidates();
        let match_count = agents.len();
        let row_width = usize::from(inner_rect(picker).width.saturating_sub(2));
        let entries = agents
            .iter()
            .enumerate()
            .map(|(index, agent)| {
                agent_picker_row(agent, row_width, selected == Some(index), &self.theme)
            })
            .collect();
        let context = if self.new_session_draft.is_some() {
            format!(
                "preset: {} · {}",
                self.selected_preset_label(),
                self.descriptor_revisions_label()
            )
        } else {
            self.descriptor_revisions_label()
        };
        let title = format!("Agent ({match_count}/{total}) — {context}");
        let empty_message = if total == 0 {
            Some("No root-runnable agents are available.")
        } else if match_count == 0 {
            Some("No agents match the filter.")
        } else {
            None
        };

        self.hit_map.picker = Some(picker);
        self.hit_map.picker_rows = crate::ui::pickers::render_lines(
            frame,
            crate::ui::pickers::PickerChrome {
                title: &title,
                empty_message,
                hint: Some("↑↓ move · enter: select · esc: search"),
            },
            entries,
            picker,
            &mut self.picker_state,
            &self.theme,
            self.theme.selected_overlay(),
        )
        .into_iter()
        .map(|(rect, index)| PickerRowHit { rect, index })
        .collect();
    }

    pub(super) fn render_model_picker(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let input_height = 3.min(area.height);
        let input_area = Rect::new(area.x, area.y, area.width, input_height);
        let rendered_input = crate::ui::pickers::render_search_input(
            frame,
            input_area,
            &mut self.model_search,
            &self.theme,
        );
        self.hit_map.picker_input = Some(InputHit {
            rect: input_area,
            text_rect: rendered_input.text_rect,
            scrollbar: None,
        });

        let picker = Rect::new(
            area.x,
            area.y.saturating_add(input_height),
            area.width,
            area.height.saturating_sub(input_height),
        );
        let models = self.filtered_draft_models();
        let total = self.draft_models().len();
        let title = format!("Model ({}/{total})", models.len());
        let row_width = usize::from(inner_rect(picker).width.saturating_sub(2));
        self.clamp_picker_selection();
        let selected = self.picker_state.selected();
        let entries = models
            .iter()
            .enumerate()
            .map(|(index, selection)| {
                model_picker_row(
                    selection,
                    self.model_descriptor(&selection.model)
                        .map(|descriptor| descriptor.display_name.as_str()),
                    row_width,
                    selected == Some(index),
                    &self.theme,
                )
            })
            .collect();
        let empty_message = if total == 0 {
            Some("No models are available for this draft.")
        } else if models.is_empty() {
            Some("No models match the filter.")
        } else {
            None
        };

        self.hit_map.picker = Some(picker);
        self.hit_map.picker_rows = crate::ui::pickers::render_lines(
            frame,
            crate::ui::pickers::PickerChrome {
                title: &title,
                empty_message,
                hint: Some("↑↓ move · enter: select · esc: search"),
            },
            entries,
            picker,
            &mut self.picker_state,
            &self.theme,
            self.theme.selected_overlay(),
        )
        .into_iter()
        .map(|(rect, index)| PickerRowHit { rect, index })
        .collect();
    }

    pub(super) fn render_session_search(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let input_height = 3.min(area.height);
        let input_area = Rect::new(area.x, area.y, area.width, input_height);
        let rendered_input = crate::ui::pickers::render_search_input(
            frame,
            input_area,
            &mut self.session_search,
            &self.theme,
        );
        self.hit_map.picker_input = Some(InputHit {
            rect: input_area,
            text_rect: rendered_input.text_rect,
            // Single-row search inputs never reach the composer ceiling, so
            // they never carry a scrollbar.
            scrollbar: None,
        });

        let picker = Rect::new(
            area.x,
            area.y.saturating_add(input_height),
            area.width,
            area.height.saturating_sub(input_height),
        );
        self.hit_map.picker = Some(picker);
        self.clamp_picker_selection();
        self.refresh_session_search_rows_cache();
        let picker_total = self.picker_sessions().count();
        let rows = &self.session_search_rows_cache.rows;
        let session_count = rows.iter().filter(|row| row.session_id().is_some()).count();
        let title = format!("Sessions ({session_count}/{picker_total})");
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(self.theme.panel_border())
                .title(title)
                .style(self.theme.panel()),
            picker,
        );
        let inner = inner_rect(picker);
        self.hit_map.picker_rows.clear();
        if session_count == 0 {
            frame.render_widget(
                Paragraph::new("No sessions match the filter.").style(self.theme.muted()),
                inner,
            );
            return;
        }

        let selected = self
            .picker_state
            .selected()
            .unwrap_or(0)
            .min(session_count - 1);
        let selected_visual = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.session_id().is_some())
            .nth(selected)
            .map_or(0, |(index, _)| index);
        let viewport_height = usize::from(inner.height);
        let max_start = rows.len().saturating_sub(viewport_height);
        let start = selected_visual
            .saturating_add(1)
            .saturating_sub(viewport_height)
            .min(max_start);
        let mut selectable_index = rows[..start]
            .iter()
            .filter(|row| row.session_id().is_some())
            .count();
        for (line, row) in rows.iter().skip(start).take(viewport_height).enumerate() {
            let row_area = Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(line).unwrap_or(u16::MAX)),
                inner.width,
                1,
            );
            match row {
                SessionSearchRow::Header(label) => {
                    frame.render_widget(
                        Paragraph::new(format!(
                            "  {}",
                            truncate_with_ellipsis(label, inner.width.saturating_sub(2).into())
                        ))
                        .style(self.theme.muted()),
                        row_area,
                    );
                }
                SessionSearchRow::Session { label, .. } => {
                    let is_selected = selectable_index == selected;
                    let prefix = if is_selected { "> " } else { "  " };
                    // Long titles ellipsize instead of hard-clipping at the
                    // panel edge; the agent/id metadata yields first.
                    let label = truncate_with_ellipsis(label, inner.width.saturating_sub(2).into());
                    let paragraph = Paragraph::new(format!("{prefix}{label}"));
                    frame.render_widget(
                        if is_selected {
                            paragraph.style(self.theme.selected())
                        } else {
                            paragraph
                        },
                        row_area,
                    );
                    self.hit_map.picker_rows.push(PickerRowHit {
                        rect: row_area,
                        index: selectable_index,
                    });
                    selectable_index += 1;
                }
            }
        }
    }

    pub(super) fn render_connect_details(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let Some(provider) = self.connect_provider.as_ref() else {
            return;
        };
        let state = row_state(
            provider,
            &self.models,
            self.provider_operations.get(&provider.id),
        );
        let reason = provider
            .support
            .reason
            .as_ref()
            .map_or("none", |reason| reason.as_str());
        let quarantine = provider
            .quarantine
            .as_ref()
            .map_or("none".into(), |diagnostic| {
                format!("{}: {}", diagnostic.code, diagnostic.message)
            });
        let action = if let Some(ProviderOperation::InProgress(operation)) =
            self.provider_operations.get(&provider.id)
        {
            format!("{} in progress… · Esc: close", action_name(*operation))
        } else {
            match state {
                ProviderRowState::ConnectedReconnect if provider.durable_connection.is_some() => {
                    "R: reconnect/update · D: disconnect · Esc: close".into()
                }
                ProviderRowState::ConnectedReconnect => {
                    "R: reconnect/update · Esc: close".into()
                }
                ProviderRowState::Removed => {
                    "Removed from the current catalog; retained recipe matching permits reconnect/update. Frozen session models remain available through exact manifest rehydration. Esc: close".into()
                }
                ProviderRowState::Unsupported
                | ProviderRowState::Disconnected
                | ProviderRowState::ErrorRetry => "Enter: details only · Esc: close".into(),
            }
        };
        let content = format!(
            "{DURABLE_PROVIDER_COPY}\n\nProvider: {} ({})\nState: {:?}\nPresence: {:?}\nSupport: {:?}\nTyped reason: {reason}\nConfiguration: {:?}\nEffective auth: {:?}\nQuarantine: {quarantine}\nSetup fields: {}\nAuth methods: {}\n\n{action}",
            provider.display_name,
            provider.id,
            state,
            provider.presence,
            provider.support.state,
            provider.configuration,
            provider.effective_auth_state,
            provider.setup_fields.len(),
            provider.auth_methods.len(),
        );
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.theme.panel_border())
                    .title("Provider details"),
            ),
            area,
        );
    }

    pub(super) fn render_connect_setup(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let Some(form) = self.provider_form.as_mut() else {
            return;
        };
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(self.theme.panel_border())
            .title("Connect provider");
        let inner = outer.inner(area);
        frame.render_widget(outer, area);
        let auth_label = form.selected_auth().map_or_else(
            || form.auth_method.to_string(),
            |method| format!("{} ({})", method.display_name, method.id),
        );
        let focus = form.focus();
        let instructions = if form.can_disconnect {
            "Tab/Down: next · Shift-Tab/Up: previous · Enter: activate/submit · Esc: cancel · Ctrl-D: disconnect"
        } else {
            "Tab/Down: next · Shift-Tab/Up: previous · Enter: activate/submit · Esc: cancel"
        };
        // A validation failure renders inline above the fields instead of
        // taking over the panel; editing any value clears it.
        let header_height = if form.error.is_some() { 5 } else { 4 }.min(inner.height);
        let mut header = vec![
            Line::from(DURABLE_PROVIDER_COPY),
            Line::from(format!(
                "Provider: {} ({})",
                form.provider.display_name, form.provider.id
            )),
            Line::from(instructions),
        ];
        if let Some(error) = form.error.as_deref() {
            header.push(Line::from(Span::styled(
                error.to_owned(),
                self.theme.error(),
            )));
        }
        frame.render_widget(
            Paragraph::new(header).wrap(Wrap { trim: false }),
            Rect::new(inner.x, inner.y, inner.width, header_height),
        );
        let mut y = inner.y.saturating_add(header_height);
        if form.has_auth_selector() {
            let auth_area = Rect::new(inner.x, y, inner.width, 3.min(inner.bottom() - y));
            frame.render_widget(
                Paragraph::new(format!("← {auth_label} →")).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(
                            self.theme
                                .input_border(focus == ProviderFormFocus::AuthMethod),
                        )
                        .title("Authentication method · Left/Right/Space: change"),
                ),
                auth_area,
            );
            self.hit_map.provider_fields.push(ProviderFieldHit {
                rect: auth_area,
                text_rect: Rect::new(
                    auth_area.x.saturating_add(1),
                    auth_area.y.saturating_add(1),
                    auth_area.width.saturating_sub(2),
                    auth_area.height.saturating_sub(2),
                ),
                focus: ProviderFormFocus::AuthMethod,
            });
            y = y.saturating_add(3);
        } else {
            let height = 1.min(inner.bottom() - y);
            frame.render_widget(
                Paragraph::new(format!("Authentication method: {auth_label} · read-only")),
                Rect::new(inner.x, y, inner.width, height),
            );
            y = y.saturating_add(height);
        }
        for (index, field) in form.secrets.iter_mut().enumerate() {
            let height = 3.min(inner.bottom().saturating_sub(y));
            let field_area = Rect::new(inner.x, y, inner.width, height);
            let title = format!(
                "Credential: {} [{}]{} · {}",
                field.descriptor.display_name,
                field.descriptor.id,
                if field.descriptor.required {
                    " required"
                } else {
                    ""
                },
                field.descriptor.help
            );
            // The renderer owns the text area (minus any reserved scrollbar
            // column), so hover and cursor math follow the painted geometry.
            let rendered = input::render_masked(
                frame,
                field_area,
                &mut field.input,
                focus == ProviderFormFocus::Credential(index),
                &title,
                &self.theme,
            );
            self.hit_map.provider_fields.push(ProviderFieldHit {
                rect: field_area,
                text_rect: rendered.text_rect,
                focus: ProviderFormFocus::Credential(index),
            });
            y = y.saturating_add(height);
            let helper_height = 1.min(inner.bottom().saturating_sub(y));
            frame.render_widget(
                Paragraph::new("Credentials are verified on first use."),
                Rect::new(
                    inner.x.saturating_add(1),
                    y,
                    inner.width.saturating_sub(1),
                    helper_height,
                ),
            );
            y = y.saturating_add(helper_height);
        }
        for (index, field) in form.setup.iter_mut().enumerate() {
            let height = 3.min(inner.bottom().saturating_sub(y));
            let field_area = Rect::new(inner.x, y, inner.width, height);
            let secret = !field.descriptor.safe_to_project;
            let title = format!(
                "Setup: {} [{}]{}{} · {}",
                field.descriptor.display_name,
                field.descriptor.id,
                if field.descriptor.required {
                    " required"
                } else {
                    ""
                },
                if secret { " secret" } else { "" },
                field.descriptor.help
            );
            let focused = focus == ProviderFormFocus::Setup(index);
            let rendered = if secret {
                input::render_masked(
                    frame,
                    field_area,
                    &mut field.input,
                    focused,
                    &title,
                    &self.theme,
                )
            } else {
                input::render(
                    frame,
                    field_area,
                    field.input.state_mut(),
                    focused,
                    title.clone(),
                    // Setup fields carry their display name and help text;
                    // a placeholder could read as a prefilled default.
                    None,
                    &self.theme,
                )
            };
            self.hit_map.provider_fields.push(ProviderFieldHit {
                rect: field_area,
                text_rect: rendered.text_rect,
                focus: ProviderFormFocus::Setup(index),
            });
            y = y.saturating_add(height);
        }
        let submit_label = if form.reconnect {
            "Reconnect"
        } else {
            "Connect"
        };
        let buttons_height = 3.min(inner.bottom().saturating_sub(y));
        // Two compact buttons centered as one group, never a panel-wide
        // strip. Width is the label plus a one-column border each side; a
        // two-cell gutter separates the frames.
        let button_width = |text: &str| {
            (u16::try_from(text.len()).unwrap_or(u16::MAX))
                .saturating_add(4)
                .min(inner.width)
        };
        let submit_width = button_width(submit_label);
        let cancel_width = button_width("Cancel");
        let group_width = submit_width.saturating_add(2).saturating_add(cancel_width);
        let group_x = inner
            .x
            .saturating_add(inner.width.saturating_sub(group_width) / 2);
        let submit_area = Rect::new(group_x, y, submit_width, buttons_height);
        let cancel_x = group_x
            .saturating_add(submit_width)
            .saturating_add(2)
            .min(inner.right().saturating_sub(cancel_width));
        let cancel_area = Rect::new(cancel_x, y, cancel_width, buttons_height);
        let submit_style = self.theme.input_border(focus == ProviderFormFocus::Submit);
        let cancel_style = self.theme.input_border(focus == ProviderFormFocus::Cancel);
        render_connect_button(frame, submit_area, submit_label, submit_style);
        render_connect_button(frame, cancel_area, "Cancel", cancel_style);
        self.hit_map.provider_submit = Some(submit_area);
        self.hit_map.provider_cancel = Some(cancel_area);
    }

    pub(super) fn render_connect_error(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let Some(form) = self.provider_form.as_ref() else {
            return;
        };
        let error = form.error.as_deref().unwrap_or("Unknown connect error.");
        let content = format!(
            "{DURABLE_PROVIDER_COPY}\n\nProvider: {} ({})\nAuthentication method: {}\n\nConnect failed:\n{error}\n\nNo credentials were verified. Credentials are verified on first use.\n\nPress Esc to return to the form, edit values, and retry.",
            form.provider.display_name, form.provider.id, form.auth_method
        );
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.theme.panel_border())
                    .title("Provider connection error"),
            ),
            area,
        );
    }

    pub(super) fn render_disconnect_confirm(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        paint_panel(frame, area, &self.theme);
        let Some(provider) = self.connect_provider.as_ref() else {
            return;
        };
        let content = format!(
            "{DURABLE_PROVIDER_COPY}\n\nDisconnect {} ({})?\nThis removes both stored public setup and stored credentials. Authored configuration is unchanged.\n\nPress Enter/Y to disconnect or Esc/N to cancel.",
            provider.display_name, provider.id
        );
        frame.render_widget(
            Paragraph::new(content).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(self.theme.panel_border())
                    .title("Confirm provider disconnect"),
            ),
            area,
        );
    }

    pub(in crate::ui) fn render_command_palette(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.clamp_palette_selection();
        let entries = self.palette_entries();
        let query = self.input.as_str().strip_prefix('/').unwrap_or_default();
        let labels = entries
            .iter()
            .map(|entry| entry.label())
            .collect::<Vec<_>>();
        self.hit_map.palette = Some(area);
        self.hit_map.palette_rows = crate::ui::slash::render(
            frame,
            query,
            labels,
            area,
            &mut self.palette_state,
            &self.theme,
        )
        .into_iter()
        .map(|(rect, index)| PaletteRowHit { rect, index })
        .collect();
    }
}

/// Drops VS16 (U+FE0F) from glyphs that are one cell wide without it (`✏️`,
/// `⚠️`, `❤️`). The buffer counts such a sequence as two cells, but VTE,
/// xterm, and Alacritty draw it in one, so the shadow cell behind it is never
/// painted and whatever the terminal showed there last lingers until
/// something restyles it. Rewritten as the bare one-cell glyph, the shadow
/// cell becomes an ordinary blank the diff repaints, and the layout keeps its
/// two columns on every terminal.
fn narrow_emoji_presentation(buffer: &mut ratatui::buffer::Buffer) {
    for cell in &mut buffer.content {
        if !cell.symbol().contains('\u{FE0F}') {
            continue;
        }
        let bare = cell.symbol().replace('\u{FE0F}', "");
        if UnicodeWidthStr::width(bare.as_str()) == 1 {
            cell.set_symbol(&bare);
        }
    }
}

/// Marks the cells a joined emoji (`👩‍💻`, `👍🏽`, `🏳️‍🌈`) may spill over as
/// always-redrawn. The buffer gives such a grapheme two cells, but a terminal
/// without grapheme clustering draws each of its parts, so it runs over the
/// cells after it; the diff never resends those while they are unchanged, and
/// the spill lingers. Resent every frame at explicit positions, they clip the
/// emoji back to the two cells the layout reserved: on such terminals it may
/// lose its tail, but the text beside it stays put. Only emoji sequences are
/// fenced — CJK is wide on every terminal and never spills.
fn fence_emoji_spill(buffer: &mut ratatui::buffer::Buffer) {
    use ratatui::buffer::{CellDiffOption, CellWidth};
    use unicode_properties::UnicodeEmoji;
    use unicode_width::UnicodeWidthChar;

    let width = usize::from(buffer.area.width);
    if width == 0 {
        return;
    }
    let mut fences = Vec::new();
    for (index, cell) in buffer.content.iter().enumerate() {
        let symbol = cell.symbol();
        if symbol.chars().nth(1).is_none() || !symbol.chars().any(UnicodeEmoji::is_emoji_char) {
            continue;
        }
        // The widest the terminal may draw it: every part on its own, a
        // presentation selector widening the part before it, and one more
        // cell for a wide part straddling the fence's end.
        let spill = symbol
            .chars()
            .map(|character| character.width().unwrap_or(0))
            .sum::<usize>()
            + symbol.matches('\u{FE0F}').count()
            + 1;
        // The fence stops at the row's end: a spill that autowraps onto the
        // next row is rare enough to leave alone.
        let row_end = (index / width + 1) * width;
        let start = (index + usize::from(cell.cell_width().max(1))).min(row_end);
        let end = (index + spill).min(row_end);
        if start < end {
            fences.push(start..end);
        }
    }
    for range in fences {
        for cell in &mut buffer.content[range] {
            if matches!(cell.diff_option, CellDiffOption::None) {
                cell.set_diff_option(CellDiffOption::AlwaysUpdate);
            }
        }
    }
}

#[cfg(test)]
mod emoji_width_tests {
    use ratatui::{buffer::Buffer, layout::Rect, style::Style};

    #[test]
    fn text_default_glyphs_lose_vs16_and_their_shadow_cell_is_a_plain_blank() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        buffer.set_string(0, 0, "✏️x💻y", Style::default());
        super::narrow_emoji_presentation(&mut buffer);
        let symbols = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>();
        // `✏` keeps its two columns as glyph + blank; `💻` is wide everywhere
        // and is left alone.
        assert_eq!(symbols, ["✏", " ", "x", "💻", " ", "y", " ", " "]);
    }

    #[test]
    fn cells_after_a_joined_emoji_are_resent_even_when_unchanged() {
        let mut previous = Buffer::empty(Rect::new(0, 0, 8, 2));
        previous.set_string(0, 0, "👩‍💻abcdef", Style::default());
        previous.set_string(0, 1, "界abcdef", Style::default());
        let mut next = previous.clone();
        super::fence_emoji_spill(&mut next);
        // `👩` and `💻` drawn apart take four cells, plus one for a straddling
        // part: columns 2..5 are fenced. The CJK row, wide everywhere, is not.
        let resent = previous
            .diff(&next)
            .into_iter()
            .map(|(x, y, _)| (x, y))
            .collect::<Vec<_>>();
        assert_eq!(resent, [(2, 0), (3, 0), (4, 0)]);
    }
}
