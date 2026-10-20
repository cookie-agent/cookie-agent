//! A session's transcript items as HTML, from the shared reducer's
//! projection. Each item renders on its own, so the page re-renders only
//! the items whose [`item_key`] changed.

use std::{
    collections::HashSet,
    hash::{BuildHasher as _, Hash, Hasher as _},
};

use cookie_agent_client_state::{
    AssistantChild, EventLevel, ModelDisplayNames, SessionState, ToolCallState, ToolStatus,
    TranscriptItem,
};
use cookie_agent_protocol::ToolHint;

use super::{
    blocks::{BlockPlace, display_html},
    escape,
    markdown::document_html,
};

/// What the page shows besides the projection itself.
pub struct TranscriptView<'a> {
    pub state: &'a SessionState,
    pub names: &'a ModelDisplayNames,
    /// Diagnostic rows below this level are hidden (`/events`).
    pub minimum_level: EventLevel,
    /// Tool calls the user expanded.
    pub expanded: &'a HashSet<String>,
}

const NOTE: &str = "text-xs text-ink dark:text-sand";

/// The icon for a tool hint. Hints are open labels: one this build does not
/// document, or none, gets the generic icon. The tool's name is never
/// consulted.
pub fn hint_icon(hint: Option<&str>) -> &'static str {
    match hint.unwrap_or(ToolHint::GENERIC) {
        ToolHint::SHELL => "💻",
        ToolHint::FILE_READ => "📖",
        ToolHint::FILE_WRITE | ToolHint::FILE_EDIT => "📝",
        ToolHint::FETCH => "🌐",
        ToolHint::SEARCH => "🔍",
        ToolHint::DELEGATE => "🤖",
        ToolHint::MESSAGE => "📨",
        ToolHint::SKILL => "✨",
        ToolHint::GOAL => "🎯",
        _ => "🔨",
    }
}

/// A key that changes whenever the item's HTML would: its version, the
/// revisions of the tool calls it shows, and their expansion.
pub fn item_key(view: &TranscriptView<'_>, item: &TranscriptItem) -> u64 {
    let mut hasher = foldhash::fast::FixedState::default().build_hasher();
    item.id().hash(&mut hasher);
    item.version().hash(&mut hasher);
    std::mem::discriminant(item).hash(&mut hasher);
    if let TranscriptItem::Assistant { children, .. } = item {
        children.len().hash(&mut hasher);
        for child in children {
            child.version().hash(&mut hasher);
            if let AssistantChild::Tool { call_id } = child {
                let id = call_id.to_string();
                view.expanded.contains(&id).hash(&mut hasher);
                if let Some(tool) = view.state.tools.get(call_id) {
                    tool.revision.hash(&mut hasher);
                }
            }
        }
    }
    if let TranscriptItem::Event { level, .. } = item {
        (*level >= view.minimum_level).hash(&mut hasher);
    }
    hasher.finish()
}

/// The item as HTML; `None` for a diagnostic row the level filter hides.
pub fn item_html(view: &TranscriptView<'_>, item: &TranscriptItem) -> Option<String> {
    Some(match item {
        TranscriptItem::User { markdown, .. } => format!(
            "<div class=\"ml-auto max-w-[90%] rounded-lg bg-toasted px-3 py-2 dark:bg-ember\"><div class=\"markdown\">{}</div></div>",
            document_html(markdown)
        ),
        TranscriptItem::Assistant {
            attribution,
            children,
            ..
        } => {
            let mut html = format!(
                "<div class=\"{NOTE} font-mono\">{}</div>",
                escape(&attribution.header(view.names))
            );
            for child in children {
                html.push_str(&child_html(view, child));
            }
            format!("<div class=\"flex flex-col gap-1\">{html}</div>")
        }
        TranscriptItem::Event {
            level,
            text,
            repeat,
            ..
        } => {
            if *level < view.minimum_level {
                return None;
            }
            let class = match level {
                EventLevel::Error => "text-red-700 dark:text-red-400",
                EventLevel::Warning => "text-caramel dark:text-maple",
                EventLevel::Info | EventLevel::Debug => "text-ink dark:text-sand",
            };
            let repeat = if *repeat > 1 {
                format!(" ×{repeat}")
            } else {
                String::new()
            };
            format!(
                "<div class=\"font-mono text-xs whitespace-pre-wrap {class}\">{} {}{repeat}</div>",
                level.badge(),
                escape(text)
            )
        }
        TranscriptItem::Compacting { .. } => {
            format!("<div class=\"{NOTE}\">compacting the context…</div>")
        }
        TranscriptItem::Compaction { .. } => {
            format!("<div class=\"{NOTE}\">— context compacted —</div>")
        }
        TranscriptItem::PluginMessage { input, .. } => format!(
            "<div class=\"{NOTE} whitespace-pre-wrap\">plugin message: {}</div>",
            escape(input)
        ),
        TranscriptItem::AgentMd { entries, .. } => format!(
            "<div class=\"{NOTE}\">AGENTS.md loaded ({} files)</div>",
            entries.len()
        ),
        TranscriptItem::SkillLoaded { name, args, .. } => format!(
            "<div class=\"{NOTE}\">skill {} loaded {}</div>",
            escape(name),
            escape(args)
        ),
        TranscriptItem::Goal { goal, .. } => format!(
            "<div class=\"{NOTE}\">goal: {}</div>",
            escape(&goal.objective)
        ),
        TranscriptItem::ProducerMessage { body, .. } => format!(
            "<div class=\"{NOTE} whitespace-pre-wrap\">{}</div>",
            escape(body)
        ),
    })
}

fn child_html(view: &TranscriptView<'_>, child: &AssistantChild) -> String {
    match child {
        AssistantChild::Text { markdown, .. } => {
            format!("<div class=\"markdown\">{}</div>", document_html(markdown))
        }
        AssistantChild::Thinking { text, .. } => format!(
            "<details class=\"{NOTE}\"><summary class=\"cursor-pointer hover:text-espresso dark:hover:text-latte\">thinking</summary><div class=\"whitespace-pre-wrap\">{}</div></details>",
            escape(text)
        ),
        AssistantChild::Tool { call_id } => match view.state.tools.get(call_id) {
            Some(tool) => tool_html(view, tool),
            None => String::new(),
        },
        AssistantChild::CommittedTool { name, .. } => format!(
            "<div class=\"{NOTE} font-mono\">🔨 {} (not started)</div>",
            escape(name.as_str())
        ),
        AssistantChild::MediaFile { file, .. } => format!(
            "<div class=\"{NOTE}\">{}</div>",
            escape(&format!("media: {}", file.media_type))
        ),
        AssistantChild::Attribution { resolved_model } => format!(
            "<div class=\"{NOTE} font-mono\">→ {}</div>",
            escape(&view.names.selection_label(&resolved_model.selection))
        ),
        AssistantChild::Notice { text } => {
            format!("<div class=\"{NOTE} italic\">{}</div>", escape(text))
        }
    }
}

fn tool_html(view: &TranscriptView<'_>, tool: &ToolCallState) -> String {
    let id = tool.id.to_string();
    let (status, class) = match tool.status {
        ToolStatus::Running if tool.backgrounded => {
            ("in the background", "text-ink dark:text-sand")
        }
        ToolStatus::Running => ("running", "text-caramel dark:text-maple"),
        ToolStatus::Completed => ("done", "text-green-700 dark:text-green-400"),
        ToolStatus::Failed => ("failed", "text-red-700 dark:text-red-400"),
        ToolStatus::Cancelled => ("cancelled", "text-ink dark:text-sand"),
        ToolStatus::Interrupted => ("interrupted", "text-ink dark:text-sand"),
    };
    let expanded = view.expanded.contains(&id) || tool.status == ToolStatus::Running;
    let mut html = format!(
        "<div class=\"rounded border border-toasted dark:border-ember\" data-tool=\"{}\"><button type=\"button\" data-action=\"toggle-tool\" data-call=\"{}\" aria-expanded=\"{expanded}\" class=\"flex min-h-11 w-full items-center gap-2 px-2 py-1 text-left font-mono text-sm hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke\"><span>{}</span><span class=\"flex-1 truncate\">{}</span><span class=\"{class}\">{status}</span></button>",
        escape(&id),
        escape(&id),
        hint_icon(tool.presentation.hint.as_ref().map(|hint| hint.rendered())),
        escape(&tool.compact_title()),
    );
    if let Some(progress) = tool
        .progress
        .as_ref()
        .filter(|_| tool.status == ToolStatus::Running)
    {
        html.push_str(&format!(
            "<div class=\"px-2 {NOTE}\">{}</div>",
            escape(progress)
        ));
    }
    if expanded {
        html.push_str("<div class=\"flex flex-col gap-1 px-2 pb-2\">");
        if tool.presentation.arguments.is_empty() {
            if !tool.arguments.is_empty() {
                html.push_str(&format!(
                    "<pre class=\"overflow-x-auto whitespace-pre-wrap font-mono text-xs\">{}</pre>",
                    escape(&tool.arguments)
                ));
            }
        } else {
            html.push_str(&display_html(
                &tool.presentation.arguments,
                BlockPlace::plain,
            ));
        }
        if let Some(failure) = &tool.failure {
            html.push_str(&format!(
                "<div class=\"text-sm text-red-700 dark:text-red-400\">{}</div>",
                escape(failure)
            ));
        }
        html.push_str(&display_html(tool.shown_display(), |index| BlockPlace {
            call_id: Some(&id),
            index,
            full: tool.full_content(index),
        }));
        html.push_str("</div>");
    }
    html.push_str("</div>");
    html
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use cookie_agent_client_state::{EventLevel, ModelDisplayNames, SessionState, TranscriptItem};

    use super::{TranscriptView, hint_icon, item_html, item_key};

    #[test]
    fn rows_below_the_event_level_are_hidden_and_text_is_escaped() {
        let state = SessionState::default();
        let names = ModelDisplayNames::default();
        let expanded = HashSet::new();
        let view = |minimum_level| TranscriptView {
            state: &state,
            names: &names,
            minimum_level,
            expanded: &expanded,
        };
        let row = TranscriptItem::internal("<careful>");
        let shown = item_html(&view(EventLevel::Info), &row).expect("shown");
        assert!(shown.contains("[I] &lt;careful&gt;"), "{shown}");
        assert_eq!(item_html(&view(EventLevel::Warning), &row), None);
        assert_ne!(
            item_key(&view(EventLevel::Info), &row),
            item_key(&view(EventLevel::Warning), &row)
        );
        let user =
            item_html(&view(EventLevel::Info), &TranscriptItem::user("hi <b>")).expect("user");
        assert!(user.contains("hi &lt;b&gt;"), "{user}");
    }

    #[test]
    fn icons_follow_the_hint_not_the_tool() {
        assert_eq!(hint_icon(Some("shell")), "💻");
        assert_eq!(hint_icon(Some("file_edit")), "📝");
        assert_eq!(hint_icon(Some("bash")), "🔨");
        assert_eq!(hint_icon(None), "🔨");
    }
}
