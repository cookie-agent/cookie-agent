//! The web client's Configuration form: `config.toml` shown as settings.
//!
//! [`SECTIONS`] says which keys the form knows and how to edit them,
//! [`MANAGED`] which tables have a panel of their own. [`ConfigForm::build`]
//! sorts the keys of a decoded `config.toml` into those; whatever it does not
//! recognize, or holds a type its field cannot show, becomes an
//! [`OtherEntry`], shown as it is written and changed in the file. The form
//! never needs to know every key: a key missing here only moves to a box.
//!
//! A change is one `config.apply` `set` per key, so the rest of the file and
//! its comments stay as they are.

use std::collections::BTreeMap;

use cookie_agent_protocol::{ConfigChange, ModelKey, OnboardingModelField, VariantId};
use serde_json::{Map, Value};

use crate::onboarding::{ModelChoice, field_name};

/// How a value shows when it must not be seen.
pub const MASK: &str = "••••••••";

/// What a field holds, and so how it is edited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldKind {
    Bool,
    Integer,
    Number,
    Text,
    /// One of these strings.
    Choice(&'static [&'static str]),
    /// A list of these strings.
    Choices(&'static [&'static str]),
    /// A credential: the form says whether it is set, and never shows it.
    Secret,
    /// A model or a fallback list of them, edited with the model picker;
    /// its text is the value as JSON.
    Models,
}

/// What an unset field means.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldDefault {
    /// The value the daemon uses, written as the field's text.
    Value(&'static str),
    /// What the daemon does, in words: no key value says the same.
    Note(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigField {
    /// The path of table and key names.
    pub key: &'static [&'static str],
    pub label: &'static str,
    pub help: &'static str,
    pub kind: FieldKind,
    pub default: FieldDefault,
}

impl ConfigField {
    /// The dotted key, as the docs write it.
    #[must_use]
    pub fn path(&self) -> String {
        self.key.join(".")
    }

    /// Whether `value` is what this field can show.
    fn accepts(&self, value: &Value) -> bool {
        match self.kind {
            FieldKind::Bool => value.is_boolean(),
            FieldKind::Models => models_from_value(value).is_some(),
            FieldKind::Integer => value.is_i64() || value.is_u64(),
            FieldKind::Number => value.is_number(),
            FieldKind::Text | FieldKind::Secret => value.is_string(),
            FieldKind::Choice(options) => {
                value.as_str().is_some_and(|value| options.contains(&value))
            }
            FieldKind::Choices(options) => value.as_array().is_some_and(|values| {
                values
                    .iter()
                    .all(|value| value.as_str().is_some_and(|value| options.contains(&value)))
            }),
        }
    }

    /// `value` as this field's text, which [`Self::parse`] reads back. A
    /// credential has none.
    #[must_use]
    pub fn text(&self, value: &Value) -> String {
        match self.kind {
            FieldKind::Secret => String::new(),
            FieldKind::Models => value.to_string(),
            FieldKind::Choices(_) => value
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default(),
            FieldKind::Bool | FieldKind::Integer | FieldKind::Number => value.to_string(),
            FieldKind::Text | FieldKind::Choice(_) => value.as_str().unwrap_or_default().to_owned(),
        }
    }

    /// The value `text` stands for; blank is the key removed, `None`.
    ///
    /// # Errors
    ///
    /// What is wrong with `text`, to show beside the field.
    pub fn parse(&self, text: &str) -> Result<Option<Value>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let value = match self.kind {
            FieldKind::Bool => Value::Bool(text == "true"),
            FieldKind::Integer => {
                Value::from(text.parse::<i64>().map_err(|_| "must be a whole number")?)
            }
            FieldKind::Number => text
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or("must be a number")?,
            FieldKind::Text => Value::String(text.to_owned()),
            FieldKind::Choice(options) => {
                if !options.contains(&text) {
                    return Err(format!("must be one of {}", options.join(", ")));
                }
                Value::String(text.to_owned())
            }
            FieldKind::Choices(options) => {
                let mut chosen = Vec::new();
                for name in text.split(',').map(str::trim) {
                    if !options.contains(&name) {
                        return Err(format!("must be from {}", options.join(", ")));
                    }
                    chosen.push(Value::String(name.to_owned()));
                }
                Value::Array(chosen)
            }
            FieldKind::Secret => return Err("change this one in the file".to_owned()),
            FieldKind::Models => serde_json::from_str::<Value>(text)
                .ok()
                .filter(|value| models_from_value(value).is_some())
                .ok_or("is not a model list")?,
        };
        Ok(Some(value))
    }

    /// The text of the value that applies while the key is unset, when it
    /// has one.
    #[must_use]
    pub const fn default_text(&self) -> Option<&'static str> {
        match self.default {
            FieldDefault::Value(text) => Some(text),
            FieldDefault::Note(_) => None,
        }
    }
}

/// A group of settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigSection {
    pub id: &'static str,
    pub title: &'static str,
    /// The top-level keys it owns: what it does not know of them is shown
    /// in its boxes.
    pub tables: &'static [&'static str],
    pub fields: &'static [ConfigField],
}

/// The panel that manages a table of `config.toml`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedPanel {
    Providers,
    Mcp,
    Plugins,
    Machines,
}

/// A table of `config.toml` with a panel of its own: the form counts its
/// entries and points there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagedTable {
    pub key: &'static [&'static str],
    pub label: &'static str,
    pub panel: ManagedPanel,
}

/// The tables other panels manage.
pub const MANAGED: &[ManagedTable] = &[
    ManagedTable {
        key: &["providers"],
        label: "Model providers",
        panel: ManagedPanel::Providers,
    },
    ManagedTable {
        key: &["mcp", "servers"],
        label: "MCP servers",
        panel: ManagedPanel::Mcp,
    },
    ManagedTable {
        key: &["plugins"],
        label: "Plugins",
        panel: ManagedPanel::Plugins,
    },
    ManagedTable {
        key: &["runtimes"],
        label: "Runtime templates",
        panel: ManagedPanel::Machines,
    },
    ManagedTable {
        key: &["acp", "agents"],
        label: "ACP agents",
        panel: ManagedPanel::Machines,
    },
];

const fn field(
    key: &'static [&'static str],
    label: &'static str,
    help: &'static str,
    kind: FieldKind,
    default: FieldDefault,
) -> ConfigField {
    ConfigField {
        key,
        label,
        help,
        kind,
        default,
    }
}

const fn switch(
    key: &'static [&'static str],
    label: &'static str,
    help: &'static str,
    on: bool,
) -> ConfigField {
    let default = if on { "true" } else { "false" };
    field(
        key,
        label,
        help,
        FieldKind::Bool,
        FieldDefault::Value(default),
    )
}

const fn integer(
    key: &'static [&'static str],
    label: &'static str,
    help: &'static str,
    default: &'static str,
) -> ConfigField {
    field(
        key,
        label,
        help,
        FieldKind::Integer,
        FieldDefault::Value(default),
    )
}

const fn duration(
    key: &'static [&'static str],
    label: &'static str,
    help: &'static str,
    default: &'static str,
) -> ConfigField {
    field(
        key,
        label,
        help,
        FieldKind::Text,
        FieldDefault::Value(default),
    )
}

const fn text(
    key: &'static [&'static str],
    label: &'static str,
    help: &'static str,
    unset: &'static str,
) -> ConfigField {
    field(key, label, help, FieldKind::Text, FieldDefault::Note(unset))
}

/// What the form can edit, in the order it shows it.
pub const SECTIONS: &[ConfigSection] = &[
    ConfigSection {
        id: "models",
        title: "Default models",
        tables: &["default_model", "small_model", "utility_model"],
        fields: &[
            field(
                &["default_model"],
                "Default model",
                "Runs your agents. With several, the first available one runs and the next take over when it fails.",
                FieldKind::Models,
                FieldDefault::Note("not set"),
            ),
            field(
                &["small_model"],
                "Small model",
                "Runs subagents that carry out work, such as the worker and the explorer.",
                FieldKind::Models,
                FieldDefault::Note("the default model"),
            ),
            field(
                &["utility_model"],
                "Utility model",
                "Fast and cheap, for approvals and session titles.",
                FieldKind::Models,
                FieldDefault::Note("the session's own model"),
            ),
        ],
    },
    ConfigSection {
        id: "server",
        title: "Server",
        tables: &["server"],
        fields: &[
            field(
                &["server", "host"],
                "Host",
                "The address the daemon listens on.",
                FieldKind::Text,
                FieldDefault::Value("127.0.0.1"),
            ),
            integer(
                &["server", "port"],
                "Port",
                "The port the daemon listens on.",
                "7419",
            ),
            text(
                &["server", "url"],
                "Daemon URL",
                "The ws:// or wss:// URL ending in /ws that clients and runtimes on this machine connect to.",
                "this daemon's host and port",
            ),
            text(
                &["server", "page_url"],
                "Page URL",
                "Where the web page can be reached, such as a tunnel's address.",
                "none",
            ),
            duration(
                &["server", "session_idle_eviction_after"],
                "Evict idle sessions after",
                "How long an unused session tree stays in memory.",
                "30m",
            ),
            field(
                &["server", "token"],
                "Client token",
                "The bearer token clients present.",
                FieldKind::Secret,
                FieldDefault::Note("the daemon writes one"),
            ),
            field(
                &["server", "runtime_token"],
                "Runtime token",
                "The token dial-in runtimes present.",
                FieldKind::Secret,
                FieldDefault::Note("the daemon writes one"),
            ),
        ],
    },
    ConfigSection {
        id: "agents",
        title: "Agents and context",
        tables: &[
            "agents",
            "skills",
            "agent_md",
            "loop_warning",
            "deferred_tools",
        ],
        fields: &[
            text(
                &["agents", "default"],
                "Default agent",
                "The agent a new session starts with when neither its client nor its project chooses one.",
                "primary",
            ),
            switch(
                &["agents", "official"],
                "Official agents",
                "Keep the official agents up to date and offer them.",
                true,
            ),
            switch(
                &["skills", "official"],
                "Official skills",
                "Keep the official skills up to date and offer them.",
                true,
            ),
            switch(
                &["agent_md", "enabled"],
                "AGENTS.md context",
                "Give agents the AGENTS.md files of their workspace.",
                true,
            ),
            switch(
                &["loop_warning", "enabled"],
                "Loop warnings",
                "Nudge a model that repeats the same tool call.",
                true,
            ),
            field(
                &["deferred_tools", "always_load"],
                "Always load",
                "Built-in tool groups sent up front instead of deferred.",
                FieldKind::Choices(&["agents", "assets", "cron", "goal", "memory", "runtimes"]),
                FieldDefault::Note("none"),
            ),
        ],
    },
    ConfigSection {
        id: "context_compaction",
        title: "Context compaction",
        tables: &["context_compaction"],
        fields: &[
            switch(
                &["context_compaction", "auto"],
                "Compact automatically",
                "Summarize a session as its context fills.",
                true,
            ),
            integer(
                &["context_compaction", "max_summary_bytes"],
                "Largest summary",
                "In bytes.",
                "262144",
            ),
            integer(
                &["context_compaction", "keep_recent_tokens"],
                "Recent tokens kept",
                "How much of the end of the session stays verbatim.",
                "16384",
            ),
            field(
                &["context_compaction", "cache_expiry", "mode"],
                "Compact before the cache expires",
                "Which idle sessions compact shortly before their prompt cache expires.",
                FieldKind::Choice(&["off", "scheduled", "always"]),
                FieldDefault::Value("off"),
            ),
            duration(
                &["context_compaction", "cache_expiry", "slack"],
                "Start before expiry",
                "How long before expiry the compaction starts, at most a fifth of the TTL.",
                "5m",
            ),
            duration(
                &["context_compaction", "cache_expiry", "fallback_ttl"],
                "Fallback cache TTL",
                "The TTL of a provider whose TTL cookie neither sets nor reads.",
                "30m",
            ),
            integer(
                &["context_compaction", "cache_expiry", "min_tokens"],
                "Token floor",
                "Smaller sessions are left alone; 0 or less is off.",
                "40000",
            ),
            field(
                &["context_compaction", "cache_expiry", "min_cost_usd"],
                "Cost floor",
                "In dollars, for priced models; 0 or less is off.",
                FieldKind::Number,
                FieldDefault::Value("0"),
            ),
        ],
    },
    ConfigSection {
        id: "model_retry",
        title: "Model retries",
        tables: &["model_retry"],
        fields: &[
            integer(
                &["model_retry", "standard_retries"],
                "Standard retries",
                "Retries after a transient error.",
                "3",
            ),
            integer(
                &["model_retry", "overload_retries"],
                "Overload retries",
                "Retries while the provider is overloaded.",
                "5",
            ),
            integer(
                &["model_retry", "backoff_ceiling_ms"],
                "Longest backoff",
                "In milliseconds.",
                "60000",
            ),
        ],
    },
    ConfigSection {
        id: "approval",
        title: "Approval",
        tables: &["approval"],
        fields: &[
            field(
                &["approval", "timeout_ms"],
                "Answer within",
                "How long an escalated approval waits for you, in milliseconds; 0 or less waits until it is answered.",
                FieldKind::Integer,
                FieldDefault::Note("waits until answered"),
            ),
            field(
                &["approval", "cache_ttl"],
                "Reviewer cache",
                "How long the approval reviewer's conversation stays in its provider's prompt cache.",
                FieldKind::Choice(&["1h", "30m", "12h", "5m", "provider", "off"]),
                FieldDefault::Value("1h"),
            ),
        ],
    },
    ConfigSection {
        id: "output",
        title: "Output limits",
        tables: &["tool_output", "subagent_output"],
        fields: &[
            integer(
                &["tool_output", "max_lines"],
                "Tool output lines",
                "Longer tool output is truncated.",
                "500",
            ),
            integer(
                &["tool_output", "max_bytes"],
                "Tool output bytes",
                "Larger tool output is truncated.",
                "12800",
            ),
            field(
                &["tool_output", "head_lines"],
                "Tool output head",
                "Lines a truncated stream keeps from its start.",
                FieldKind::Integer,
                FieldDefault::Note("automatic"),
            ),
            field(
                &["tool_output", "tail_lines"],
                "Tool output tail",
                "Lines a truncated stream keeps from its end.",
                FieldKind::Integer,
                FieldDefault::Note("automatic"),
            ),
            integer(
                &["subagent_output", "max_lines"],
                "Subagent report lines",
                "Longer inline previews of a subagent's report are truncated.",
                "100",
            ),
            integer(
                &["subagent_output", "max_bytes"],
                "Subagent report bytes",
                "Larger inline previews are truncated.",
                "10240",
            ),
            field(
                &["subagent_output", "head_lines"],
                "Subagent report head",
                "Lines a truncated report keeps from its start.",
                FieldKind::Integer,
                FieldDefault::Note("automatic"),
            ),
            field(
                &["subagent_output", "tail_lines"],
                "Subagent report tail",
                "Lines a truncated report keeps from its end.",
                FieldKind::Integer,
                FieldDefault::Note("automatic"),
            ),
        ],
    },
    ConfigSection {
        id: "delegation",
        title: "Delegation",
        tables: &["delegation"],
        fields: &[
            integer(
                &["delegation", "max_depth"],
                "Deepest subagent",
                "How many levels of subagents a tree may have.",
                "3",
            ),
            integer(
                &["delegation", "max_concurrency"],
                "Subagents at once",
                "How many run at the same time.",
                "4",
            ),
            integer(
                &["delegation", "max_resident_subagents"],
                "Subagents in memory",
                "How many stay loaded.",
                "20",
            ),
            duration(
                &["delegation", "idle_eviction_after"],
                "Evict idle subagents after",
                "How long an idle subagent stays loaded.",
                "1h",
            ),
        ],
    },
    ConfigSection {
        id: "messaging",
        title: "Messaging",
        tables: &["messaging", "producers"],
        fields: &[
            switch(
                &["messaging", "enabled"],
                "Agent messaging",
                "Let agents send each other mail.",
                true,
            ),
            integer(
                &["messaging", "max_hops"],
                "Longest chain",
                "The deepest chain of agent mail; 0 or less turns the guard off.",
                "0",
            ),
            integer(
                &["messaging", "max_body_bytes"],
                "Largest message",
                "In bytes.",
                "32768",
            ),
            integer(
                &["messaging", "max_pending_per_session"],
                "Pending per session",
                "Mail one recipient may hold at once.",
                "32",
            ),
            integer(
                &["messaging", "max_inflight_per_pair"],
                "Unanswered per pair",
                "Unacknowledged mail between one sender and one recipient; 0 turns the guard off.",
                "4",
            ),
            integer(
                &["producers", "max_body_bytes"],
                "Largest external message",
                "What plugins and runtimes may send, in bytes.",
                "65536",
            ),
        ],
    },
    ConfigSection {
        id: "session_title",
        title: "Session titles",
        tables: &["session_title"],
        fields: &[
            switch(
                &["session_title", "generate_on_first_turn"],
                "Generate on the first turn",
                "Ask the utility model for a title.",
                true,
            ),
            switch(
                &["session_title", "fallback_to_input_excerpt"],
                "Fall back to the input",
                "Use the start of the first message when no title comes back.",
                true,
            ),
            integer(
                &["session_title", "max_chars"],
                "Longest title",
                "In characters.",
                "80",
            ),
            integer(
                &["session_title", "max_input_messages"],
                "Messages read",
                "How many messages the title is made from.",
                "4",
            ),
        ],
    },
    ConfigSection {
        id: "memory",
        title: "Memory",
        tables: &["memory"],
        fields: &[
            switch(
                &["memory", "enabled"],
                "Use memory",
                "Off, no run gets the memory tools or a snapshot; the notes stay and you can still edit them.",
                true,
            ),
            integer(
                &["memory", "user_max_bytes"],
                "User category",
                "Largest user:// category, in bytes.",
                "16384",
            ),
            integer(
                &["memory", "agent_max_bytes"],
                "Agent file",
                "Largest agent:// file, in bytes.",
                "16384",
            ),
            integer(
                &["memory", "machine_max_bytes"],
                "Machine memory",
                "Largest machine:// memory, in bytes.",
                "16384",
            ),
            integer(
                &["memory", "project_max_bytes"],
                "Project memory",
                "Largest project:// memory, in bytes.",
                "16384",
            ),
            integer(
                &["memory", "overview_max_bytes"],
                "Project overview",
                "Largest project overview, in bytes.",
                "16384",
            ),
            integer(
                &["memory", "snapshot_user_bytes"],
                "Snapshot: user",
                "Room for the user categories in a snapshot, in bytes.",
                "12288",
            ),
            integer(
                &["memory", "snapshot_index_bytes"],
                "Snapshot: machine index",
                "Room for the machine memory index in a snapshot, in bytes.",
                "4096",
            ),
        ],
    },
    ConfigSection {
        id: "cron",
        title: "Scheduled prompts",
        tables: &["cron"],
        fields: &[
            switch(
                &["cron", "enabled"],
                "Run scheduled prompts",
                "Off, each due fire is recorded as skipped.",
                true,
            ),
            integer(
                &["cron", "max_consecutive_failures"],
                "Failures before disabling",
                "In a row, unless a job sets its own.",
                "3",
            ),
            integer(
                &["cron", "history"],
                "Fires kept",
                "How many each job's history keeps, up to 1000.",
                "20",
            ),
            duration(
                &["cron", "missed_grace"],
                "Catch up within",
                "How late a fire missed while the daemon was down still runs; 0s catches nothing up.",
                "10m",
            ),
        ],
    },
    ConfigSection {
        id: "notifications",
        title: "Push notifications",
        tables: &["notifications"],
        fields: &[
            switch(
                &["notifications", "enabled"],
                "Send notifications",
                "Off, subscriptions are kept and nothing is sent.",
                true,
            ),
            field(
                &["notifications", "preview"],
                "Preview",
                "What a notification says of the session.",
                FieldKind::Choice(&["reply", "status"]),
                FieldDefault::Value("reply"),
            ),
            switch(
                &["notifications", "approvals"],
                "Approvals and questions",
                "Notify when one is waiting for you.",
                true,
            ),
            switch(
                &["notifications", "cron_sessions"],
                "Scheduled sessions",
                "Notify when a session a scheduled prompt created settles.",
                true,
            ),
            text(
                &["notifications", "contact"],
                "Contact",
                "The https: or mailto: URL push services may reach you at.",
                "the project's repository",
            ),
        ],
    },
    ConfigSection {
        id: "shell_environment",
        title: "Shell environment",
        tables: &["shell_environment"],
        fields: &[
            field(
                &["shell_environment", "capture"],
                "Capture",
                "How runtimes read your shell's environment for the processes they start.",
                FieldKind::Choice(&["interactive", "login", "off"]),
                FieldDefault::Value("interactive"),
            ),
            integer(
                &["shell_environment", "timeout_ms"],
                "Time limit",
                "How long a capture may take, in milliseconds; 0 or less waits indefinitely.",
                "10000",
            ),
            text(
                &["shell_environment", "shell"],
                "Shell",
                "The shell to capture with.",
                "the runtime's $SHELL",
            ),
        ],
    },
    ConfigSection {
        id: "connections",
        title: "ACP and MCP",
        tables: &["acp", "mcp"],
        fields: &[
            integer(
                &["acp", "idle_shutdown_ms"],
                "Stop idle ACP agents after",
                "How long an agent's process stays up with no live session, in milliseconds.",
                "300000",
            ),
            duration(
                &["mcp", "idle_stop_after"],
                "Stop unused MCP servers after",
                "How long a server no session wants stays connected; 0s keeps it.",
                "10m",
            ),
        ],
    },
    ConfigSection {
        id: "sharing",
        title: "Transfers and sharing",
        tables: &["transfer", "public", "share"],
        fields: &[
            integer(
                &["transfer", "max_bytes"],
                "Largest transfer",
                "The most one transfer call moves, in bytes; 0 or less means no limit.",
                "0",
            ),
            switch(
                &["share", "enabled"],
                "Share links",
                "Off, every link answers as gone, and links are kept.",
                true,
            ),
            text(
                &["public", "url"],
                "Public address",
                "The address share links and webhook URLs are built on.",
                "the page URL",
            ),
            text(
                &["public", "listen"],
                "Public-only listener",
                "An address, such as 127.0.0.1:7420, that serves share links and webhooks and nothing else.",
                "none",
            ),
        ],
    },
    ConfigSection {
        id: "webhooks",
        title: "Webhooks",
        tables: &["webhooks"],
        fields: &[
            switch(
                &["webhooks", "enabled"],
                "Accept webhooks",
                "Off, every webhook URL answers 404, and webhooks are kept.",
                true,
            ),
            integer(
                &["webhooks", "max_body_bytes"],
                "Largest request body",
                "In bytes; a larger request is refused.",
                "1048576",
            ),
            integer(
                &["webhooks", "inline_body_bytes"],
                "Body shown inline",
                "In bytes; a larger body is stored as an artifact and only its start shown.",
                "16384",
            ),
            integer(
                &["webhooks", "max_per_hour"],
                "Events per hour",
                "Per webhook, unless it sets its own; 0 or less means no limit.",
                "60",
            ),
            integer(
                &["webhooks", "max_consecutive_failures"],
                "Failures before disabling",
                "In a row, unless a webhook sets its own; 0 or less never disables.",
                "3",
            ),
            integer(
                &["webhooks", "backlog"],
                "Events waiting",
                "How many a new-session webhook holds while its sessions are busy.",
                "100",
            ),
            integer(
                &["webhooks", "history"],
                "Events kept",
                "How many each webhook's history keeps, up to 1000.",
                "50",
            ),
        ],
    },
];

/// One known field and what the file holds for it.
#[derive(Clone, Debug, PartialEq)]
pub struct FormField {
    pub field: &'static ConfigField,
    /// The file's value; a credential's is [`MASK`].
    pub value: Option<Value>,
}

impl FormField {
    /// The text the file's value stands for; blank when the key is unset.
    #[must_use]
    pub fn current_text(&self) -> String {
        self.value
            .as_ref()
            .map_or_else(String::new, |value| self.field.text(value))
    }

    /// The value this field shows given `draft`: the draft, the file's, or
    /// else the default, as text.
    #[must_use]
    pub fn shown_text(&self, draft: Option<&str>) -> String {
        let authored = draft.map_or_else(|| self.current_text(), str::to_owned);
        if authored.trim().is_empty() {
            self.field.default_text().unwrap_or_default().to_owned()
        } else {
            authored
        }
    }

    /// The change `draft` makes, if any: an unset key stays unset when the
    /// draft only restates its default, as toggling a switch back does.
    ///
    /// # Errors
    ///
    /// What is wrong with `draft`.
    pub fn change(&self, draft: &str) -> Result<Option<Option<Value>>, String> {
        let next = self.field.parse(draft)?;
        if next == self.value {
            return Ok(None);
        }
        if self.value.is_none()
            && let Some(default) = self.field.default_text()
            && next == self.field.parse(default)?
        {
            return Ok(None);
        }
        Ok(Some(next))
    }
}

/// A key the form has no control for, as the file holds it, credentials
/// masked.
#[derive(Clone, Debug, PartialEq)]
pub struct OtherEntry {
    pub key: Vec<String>,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FormSection {
    pub section: &'static ConfigSection,
    pub fields: Vec<FormField>,
    pub other: Vec<OtherEntry>,
}

impl FormSection {
    /// Whether the file sets anything of this section.
    #[must_use]
    pub fn authored(&self) -> bool {
        !self.other.is_empty() || self.fields.iter().any(|field| field.value.is_some())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FormManaged {
    pub table: &'static ManagedTable,
    /// The entries the file has under it.
    pub count: usize,
}

/// `config.toml` sorted into what the form shows.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfigForm {
    pub sections: Vec<FormSection>,
    pub managed: Vec<FormManaged>,
    /// Keys no section owns.
    pub other: Vec<OtherEntry>,
}

impl ConfigForm {
    /// Sorts `document`, a decoded `config.toml`.
    #[must_use]
    pub fn build(document: &Map<String, Value>) -> Self {
        let mut form = Self {
            sections: SECTIONS
                .iter()
                .map(|section| FormSection {
                    section,
                    fields: section
                        .fields
                        .iter()
                        .map(|field| FormField { field, value: None })
                        .collect(),
                    other: Vec::new(),
                })
                .collect(),
            managed: MANAGED
                .iter()
                .map(|table| FormManaged { table, count: 0 })
                .collect(),
            other: Vec::new(),
        };
        form.sort_table(&mut Vec::new(), document);
        form
    }

    fn sort_table(&mut self, prefix: &mut Vec<String>, table: &Map<String, Value>) {
        for (name, value) in table {
            prefix.push(name.clone());
            self.sort_value(prefix, value);
            prefix.pop();
        }
    }

    fn sort_value(&mut self, key: &mut Vec<String>, value: &Value) {
        if let Some(managed) = self
            .managed
            .iter_mut()
            .find(|managed| same_key(managed.table.key, key))
        {
            managed.count = value.as_object().map_or(1, Map::len);
            return;
        }
        let known = self
            .sections
            .iter_mut()
            .flat_map(|section| section.fields.iter_mut())
            .find(|field| same_key(field.field.key, key))
            .filter(|field| field.field.accepts(value));
        if let Some(field) = known {
            field.value = Some(if field.field.kind == FieldKind::Secret {
                Value::String(MASK.to_owned())
            } else {
                value.clone()
            });
            return;
        }
        if let Some(table) = value.as_object()
            && self.leads_somewhere(key)
        {
            self.sort_table(key, table);
            return;
        }
        // A field whose value it cannot show gives way to the box, which
        // shows it as written.
        for section in &mut self.sections {
            section
                .fields
                .retain(|field| !same_key(field.field.key, key));
        }
        let entry = OtherEntry {
            key: key.clone(),
            value: masked(key.last().map_or("", String::as_str), value),
        };
        let owner = self.sections.iter_mut().find(|section| {
            key.first()
                .is_some_and(|table| section.section.tables.contains(&table.as_str()))
        });
        match owner {
            Some(section) => section.other.push(entry),
            None => self.other.push(entry),
        }
    }

    /// Whether a known field or managed table lies under the table at `key`.
    fn leads_somewhere(&self, key: &[String]) -> bool {
        let under = |path: &[&str]| path.len() > key.len() && same_key(&path[..key.len()], key);
        self.managed.iter().any(|managed| under(managed.table.key))
            || self
                .sections
                .iter()
                .flat_map(|section| &section.fields)
                .any(|field| under(field.field.key))
    }

    /// The field at the dotted key `path`.
    #[must_use]
    pub fn field(&self, path: &str) -> Option<&FormField> {
        self.sections
            .iter()
            .flat_map(|section| &section.fields)
            .find(|field| field.field.path() == path)
    }

    /// What `drafts`, field text by dotted key, change: one `set` per key,
    /// in the form's order.
    ///
    /// # Errors
    ///
    /// Every draft that cannot be applied: its dotted key and why.
    pub fn changes(
        &self,
        drafts: &BTreeMap<String, String>,
    ) -> Result<Vec<ConfigChange>, Vec<(String, String)>> {
        let mut changes = Vec::new();
        let mut errors = Vec::new();
        for field in self.sections.iter().flat_map(|section| &section.fields) {
            let path = field.field.path();
            let Some(draft) = drafts.get(&path) else {
                continue;
            };
            match field.change(draft) {
                Ok(Some(value)) => changes.push(ConfigChange::Set {
                    key: field
                        .field
                        .key
                        .iter()
                        .map(|&name| name.to_owned())
                        .collect(),
                    value: value.unwrap_or(Value::Null),
                }),
                Ok(None) => {}
                Err(error) => errors.push((path, error)),
            }
        }
        if errors.is_empty() {
            Ok(changes)
        } else {
            Err(errors)
        }
    }
}

/// The model field an expression such as `${default_model}` names.
fn field_of_expression(text: &str) -> Option<OnboardingModelField> {
    [
        OnboardingModelField::DefaultModel,
        OnboardingModelField::SmallModel,
        OnboardingModelField::UtilityModel,
    ]
    .into_iter()
    .find(|field| {
        text.strip_prefix("${")
            .and_then(|rest| rest.strip_suffix('}'))
            == Some(field_name(*field))
    })
}

/// A value of `default_model`, `small_model`, or `utility_model` as the
/// picker's choices, in fallback order; `None` for what the picker cannot
/// hold, such as a variant on another field's variable.
#[must_use]
pub fn models_from_value(value: &Value) -> Option<Vec<ModelChoice>> {
    fn entry(value: &Value) -> Option<ModelChoice> {
        let (model, variant) = match value {
            Value::String(model) => (model.as_str(), None),
            Value::Object(table) => {
                if table.keys().any(|key| key != "model" && key != "variant") {
                    return None;
                }
                let variant = match table.get("variant") {
                    Some(variant) => Some(VariantId::new(variant.as_str()?).ok()?),
                    None => None,
                };
                (table.get("model")?.as_str()?, variant)
            }
            _ => return None,
        };
        if let Some(field) = field_of_expression(model) {
            return variant.is_none().then_some(ModelChoice::Field(field));
        }
        Some(ModelChoice::Model {
            model: model.parse::<ModelKey>().ok()?,
            variant,
        })
    }
    match value {
        Value::Array(values) if !values.is_empty() => values.iter().map(entry).collect(),
        Value::Array(_) => None,
        single => Some(vec![entry(single)?]),
    }
}

/// `choices` as the value of a model field: a list of one is the entry
/// alone, as the daemon writes it. `default_of` names a model's default
/// variant, which needs no key.
#[must_use]
pub fn models_to_value(
    choices: &[ModelChoice],
    default_of: impl Fn(&ModelKey) -> Option<VariantId>,
) -> Value {
    let mut values = choices
        .iter()
        .map(|choice| match choice {
            ModelChoice::Field(field) => Value::String(format!("${{{}}}", field_name(*field))),
            ModelChoice::Model { model, variant } => {
                match variant
                    .as_ref()
                    .filter(|variant| default_of(model).as_ref() != Some(*variant))
                {
                    Some(variant) => serde_json::json!({
                        "model": model.to_string(),
                        "variant": variant.to_string(),
                    }),
                    None => Value::String(model.to_string()),
                }
            }
        })
        .collect::<Vec<_>>();
    if values.len() == 1 {
        values.remove(0)
    } else {
        Value::Array(values)
    }
}

fn same_key(path: &[&str], key: &[String]) -> bool {
    path.len() == key.len() && path.iter().zip(key).all(|(path, key)| path == key)
}

/// Whether a key's name says its value is a credential.
fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace('-', "_");
    [
        "token",
        "secret",
        "password",
        "passwd",
        "api_key",
        "apikey",
        "authorization",
        "credential",
        "private_key",
    ]
    .iter()
    .any(|word| name.contains(word))
}

/// `value`, the value of the key `name`, with every credential in it masked.
fn masked(name: &str, value: &Value) -> Value {
    if is_secret_name(name) && !value.is_object() {
        return Value::String(MASK.to_owned());
    }
    match value {
        Value::Object(table) => Value::Object(
            table
                .iter()
                .map(|(name, value)| (name.clone(), masked(name, value)))
                .collect(),
        ),
        Value::Array(values) => {
            Value::Array(values.iter().map(|value| masked(name, value)).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn form(document: Value) -> ConfigForm {
        ConfigForm::build(document.as_object().expect("a table"))
    }

    fn section<'form>(form: &'form ConfigForm, id: &str) -> &'form FormSection {
        form.sections
            .iter()
            .find(|section| section.section.id == id)
            .expect("the section")
    }

    fn drafts(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
            .collect()
    }

    fn set(key: &[&str], value: Value) -> ConfigChange {
        ConfigChange::Set {
            key: key.iter().map(|&name| name.to_owned()).collect(),
            value,
        }
    }

    #[test]
    fn known_keys_fill_their_fields_at_any_depth() {
        let form = form(json!({
            "tool_output": { "max_lines": 9 },
            "context_compaction": { "cache_expiry": { "mode": "always" } },
        }));
        assert_eq!(
            form.field("tool_output.max_lines").unwrap().value,
            Some(json!(9))
        );
        assert_eq!(
            form.field("context_compaction.cache_expiry.mode")
                .unwrap()
                .value,
            Some(json!("always"))
        );
        assert_eq!(form.field("tool_output.max_bytes").unwrap().value, None);
        assert!(section(&form, "output").authored());
        assert!(!section(&form, "cron").authored());
        assert!(form.other.is_empty());
    }

    #[test]
    fn a_key_the_form_does_not_know_goes_to_its_sections_box_or_the_last_one() {
        let form = form(json!({
            "context_compaction": { "trigger": { "percent": 50 } },
            "headers": { "x-team": "core" },
            "default_model": { "model": "a/b", "extra": 1 },
        }));
        let compaction = section(&form, "context_compaction");
        assert_eq!(compaction.other[0].key, ["context_compaction", "trigger"]);
        assert_eq!(compaction.other[0].value, json!({ "percent": 50 }));
        assert!(compaction.authored());
        assert_eq!(section(&form, "models").other[0].key, ["default_model"]);
        assert_eq!(form.other[0].key, ["headers"]);
    }

    #[test]
    fn a_value_its_field_cannot_show_replaces_the_field_with_a_box() {
        // `mcp.idle_stop_after` also takes a number of seconds; a choice may
        // hold a value a newer daemon knows.
        let form = form(json!({
            "mcp": { "idle_stop_after": 600 },
            "approval": { "cache_ttl": "2h" },
        }));
        assert!(form.field("mcp.idle_stop_after").is_none());
        assert!(form.field("approval.cache_ttl").is_none());
        assert_eq!(section(&form, "connections").other[0].value, json!(600));
        assert_eq!(section(&form, "approval").other[0].value, json!("2h"));
    }

    #[test]
    fn managed_tables_are_counted_and_not_shown() {
        let form = form(json!({
            "providers": { "a": { "api_key": "hidden" }, "b": {} },
            "mcp": { "servers": { "exa": { "url": "x" } }, "idle_stop_after": "5m" },
            "acp": { "agents": { "claude": {} }, "idle_shutdown_ms": 5 },
        }));
        let count = |label: &str| {
            form.managed
                .iter()
                .find(|managed| managed.table.label == label)
                .unwrap()
                .count
        };
        assert_eq!(count("Model providers"), 2);
        assert_eq!(count("MCP servers"), 1);
        assert_eq!(count("ACP agents"), 1);
        assert_eq!(count("Plugins"), 0);
        assert_eq!(
            form.field("mcp.idle_stop_after").unwrap().value,
            Some(json!("5m"))
        );
        assert_eq!(
            form.field("acp.idle_shutdown_ms").unwrap().value,
            Some(json!(5))
        );
        assert!(form.other.is_empty());
        assert!(!format!("{form:?}").contains("hidden"));
    }

    #[test]
    fn credentials_never_reach_the_form() {
        let form = form(json!({
            "server": { "token": "sekret-1", "runtime_token": "sekret-2", "weird_token": "sekret-3" },
            "headers": { "Authorization": "sekret-4", "x-team": "core" },
            "extra": { "nested": [{ "api-key": "sekret-5" }] },
        }));
        let debug = format!("{form:?}");
        assert!(!debug.contains("sekret"), "{debug}");
        assert!(debug.contains("core"));
        let token = form.field("server.token").unwrap();
        assert!(token.value.is_some());
        assert_eq!(token.current_text(), "");
    }

    #[test]
    fn drafts_become_one_set_per_changed_key() {
        let form = form(json!({
            "tool_output": { "max_lines": 9, "max_bytes": 10 },
            "cron": { "enabled": false },
        }));
        let changes = form
            .changes(&drafts(&[
                ("tool_output.max_lines", "11"),
                ("tool_output.max_bytes", "10"),
                ("cron.enabled", ""),
                ("deferred_tools.always_load", "cron, memory"),
                ("no.such.key", "1"),
            ]))
            .unwrap();
        assert_eq!(changes.len(), 3);
        assert!(changes.contains(&set(&["tool_output", "max_lines"], json!(11))));
        assert!(changes.contains(&set(&["cron", "enabled"], Value::Null)));
        assert!(changes.contains(&set(
            &["deferred_tools", "always_load"],
            json!(["cron", "memory"])
        )));
    }

    #[test]
    fn restating_the_default_of_an_unset_key_changes_nothing_but_a_set_key_can_be_written() {
        let unset = form(json!({}));
        let on = unset.field("cron.enabled").unwrap();
        assert_eq!(on.change("true"), Ok(None));
        assert_eq!(on.change("false"), Ok(Some(Some(json!(false)))));
        assert_eq!(on.change(""), Ok(None));

        let set = form(json!({ "cron": { "enabled": false } }));
        let off = set.field("cron.enabled").unwrap();
        assert_eq!(off.change("true"), Ok(Some(Some(json!(true)))));
        assert_eq!(off.change(""), Ok(Some(None)));
        assert_eq!(off.change("false"), Ok(None));
    }

    #[test]
    fn a_draft_that_cannot_be_applied_is_named() {
        let form = form(json!({}));
        let errors = form
            .changes(&drafts(&[
                ("tool_output.max_lines", "many"),
                ("context_compaction.cache_expiry.min_cost_usd", "NaN"),
                ("approval.cache_ttl", "2h"),
                ("deferred_tools.always_load", "cron,nope"),
                ("server.port", "7000"),
            ]))
            .unwrap_err();
        let names = errors
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 4, "{names:?}");
        assert!(!names.contains(&"server.port"));
    }

    #[test]
    fn a_field_shows_the_draft_then_the_file_then_the_default() {
        let form = form(json!({ "cron": { "history": 7 } }));
        let history = form.field("cron.history").unwrap();
        assert_eq!(history.shown_text(None), "7");
        assert_eq!(history.shown_text(Some("9")), "9");
        let default = history.field.default_text().unwrap();
        assert_eq!(history.shown_text(Some("")), default);
        let unset = form.field("cron.enabled").unwrap();
        assert_eq!(unset.shown_text(None), unset.field.default_text().unwrap());
    }

    #[test]
    fn model_fields_hold_what_the_picker_can_and_box_the_rest() {
        let form = form(json!({
            "default_model": { "model": "a/b", "variant": "high" },
            "small_model": ["${default_model}", "c/d"],
            "utility_model": { "model": "${small_model}", "variant": "fast" },
        }));
        let key = |text: &str| text.parse::<ModelKey>().unwrap();
        let default = form.field("default_model").unwrap();
        assert_eq!(
            models_from_value(default.value.as_ref().unwrap()),
            Some(vec![ModelChoice::Model {
                model: key("a/b"),
                variant: Some(VariantId::new("high").unwrap()),
            }])
        );
        let small = form.field("small_model").unwrap();
        assert_eq!(
            models_from_value(small.value.as_ref().unwrap()),
            Some(vec![
                ModelChoice::Field(OnboardingModelField::DefaultModel),
                ModelChoice::Model {
                    model: key("c/d"),
                    variant: None
                },
            ])
        );
        assert!(form.field("utility_model").is_none());
        assert_eq!(section(&form, "models").other[0].key, ["utility_model"]);
    }

    #[test]
    fn a_model_list_writes_back_as_the_daemon_reads_it() {
        let key = |text: &str| text.parse::<ModelKey>().unwrap();
        let high = VariantId::new("high").unwrap();
        let model = |variant: Option<VariantId>| ModelChoice::Model {
            model: key("a/b"),
            variant,
        };
        let no_default = |_: &ModelKey| None;
        assert_eq!(models_to_value(&[model(None)], no_default), json!("a/b"));
        assert_eq!(
            models_to_value(&[model(Some(high.clone()))], no_default),
            json!({ "model": "a/b", "variant": "high" })
        );
        // The model's default variant needs no key.
        assert_eq!(
            models_to_value(&[model(Some(high.clone()))], |_| Some(high.clone())),
            json!("a/b")
        );
        let list = models_to_value(
            &[
                ModelChoice::Field(OnboardingModelField::SmallModel),
                model(None),
            ],
            no_default,
        );
        assert_eq!(list, json!(["${small_model}", "a/b"]));
        assert_eq!(
            models_from_value(&list).map(|choices| models_to_value(&choices, no_default)),
            Some(list)
        );
    }

    #[test]
    fn a_model_draft_is_a_change_only_when_the_value_differs() {
        let form = form(json!({ "default_model": "a/b" }));
        let field = form.field("default_model").unwrap();
        assert_eq!(field.change("\"a/b\""), Ok(None));
        assert_eq!(field.change("\"c/d\""), Ok(Some(Some(json!("c/d")))));
        assert_eq!(field.change(""), Ok(Some(None)));
        assert!(field.change("\"not a key\"").is_err());
    }

    #[test]
    fn every_field_belongs_to_its_section_and_appears_once() {
        let mut seen = std::collections::BTreeSet::new();
        for section in SECTIONS {
            for field in section.fields {
                assert!(section.tables.contains(&field.key[0]), "{}", field.path());
                assert!(seen.insert(field.path()), "{}", field.path());
            }
        }
    }
}
