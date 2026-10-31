//! The `/memory` panel's rules every client shares: the memories as rows
//! by layer and group, what a new memory starts from and where, what
//! `memory.write` answered, how a delete's refusal reads, a memory's parts
//! as an editor shows them with those of an `agent://` file the daemon
//! owns, and a memory's stamps and size limit, which the daemon owns too
//! (memory spec sections 16.9.6 and 16.9.7).

use cookie_agent_protocol::{
    MemoryIgnoredEntry, MemoryInfo, MemoryLayer, MemoryListResult, MemoryReadResult,
    MemoryWriteResult, SemVer, errors, memory_age, memory_size,
};
use cookie_agent_protocol_transport::ClientError;
use jiff::Timestamp;

/// The text a new memory starts from.
pub const NEW_MEMORY: &str = "---\ndescription: \nkeywords: []\n---\n";

/// What `memory.write` answered.
#[derive(Debug, Eq, PartialEq)]
pub enum MemoryWriteOutcome {
    /// Written; `notice` says what the daemon kept in place of what the
    /// text gave (an `agent://` file's description and first keywords).
    Saved {
        notice: Option<String>,
    },
    /// The text is not a valid memory: why.
    Invalid(String),
    /// The memory changed, appeared, or went away since it was read.
    Changed,
    Failed(String),
}

impl MemoryWriteOutcome {
    pub fn from_result(result: Result<MemoryWriteResult, ClientError>) -> Self {
        match result {
            Ok(written) => Self::Saved {
                notice: written.notice,
            },
            Err(error) if error.is(&errors::MEMORY_INVALID) => Self::Invalid(message_of(&error)),
            Err(error)
                if error.is(&errors::MEMORY_REVISION_MISMATCH)
                    || error.is(&errors::MEMORY_EXISTS)
                    || error.is(&errors::MEMORY_NOT_FOUND) =>
            {
                Self::Changed
            }
            Err(error) => Self::Failed(message_of(&error)),
        }
    }
}

/// What the status line says once `url` is saved: `Created` for a new
/// memory, else `Saved`, then the daemon's notice when it gave one.
pub fn saved_status(url: &str, created: bool, notice: Option<&str>) -> String {
    let verb = if created { "Created" } else { "Saved" };
    match notice {
        Some(notice) => format!("{verb} {url}. {notice}"),
        None => format!("{verb} {url}."),
    }
}

/// The message of a daemon error, without its code.
pub fn message_of(error: &ClientError) -> String {
    match error {
        ClientError::Rpc(rpc) => rpc.message.clone(),
        error => error.to_string(),
    }
}

/// What a refused `memory.delete` of `url` says.
pub fn delete_refusal(url: &str, error: &ClientError) -> String {
    if error.is(&errors::MEMORY_REVISION_MISMATCH) {
        format!("{url} changed since it was listed; it was not deleted")
    } else {
        format!("{url} was not deleted: {}", message_of(error))
    }
}

/// One row of the panel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryRow {
    /// A layer's heading.
    Layer(MemoryLayer),
    /// A machine or project of the layer.
    Group {
        layer: MemoryLayer,
        name: String,
    },
    Memory(MemoryInfo),
    /// The heading of what is not a memory.
    IgnoredHeading,
    Ignored(MemoryIgnoredEntry),
}

impl MemoryRow {
    /// The URL prefix a new memory started from this row gets.
    pub fn new_prefix(&self) -> String {
        match self {
            Self::Layer(layer) => format!("{}://", layer.as_str()),
            Self::Group { layer, name } => format!("{}://{name}/", layer.as_str()),
            Self::Memory(memory) => match &memory.group {
                Some(group) => format!("{}://{group}/", memory.layer.as_str()),
                None => format!("{}://", memory.layer.as_str()),
            },
            Self::IgnoredHeading | Self::Ignored(_) => "project://".to_owned(),
        }
    }
}

/// The rows: each layer's heading, its memories grouped by machine or
/// project, then what is not a memory.
pub fn memory_rows(listing: &MemoryListResult) -> Vec<MemoryRow> {
    let mut rows = Vec::new();
    for layer in MemoryLayer::ALL {
        rows.push(MemoryRow::Layer(layer));
        let mut group: Option<&str> = None;
        for memory in listing
            .memories
            .iter()
            .filter(|memory| memory.layer == layer)
        {
            if let Some(name) = memory.group.as_deref()
                && group != Some(name)
            {
                rows.push(MemoryRow::Group {
                    layer,
                    name: name.to_owned(),
                });
                group = Some(name);
            }
            rows.push(MemoryRow::Memory(memory.clone()));
        }
    }
    if !listing.ignored.is_empty() {
        rows.push(MemoryRow::IgnoredHeading);
        rows.extend(listing.ignored.iter().cloned().map(MemoryRow::Ignored));
    }
    rows
}

/// Why `url` cannot be a new memory of `listing`, if it cannot. The daemon
/// checks the URL itself when the memory is saved.
pub fn new_memory_refusal(listing: &MemoryListResult, url: &str) -> Option<String> {
    if listing.memories.iter().any(|memory| memory.url == url) {
        return Some(format!("{url} exists; edit it instead"));
    }
    let complete = url
        .split_once("://")
        .is_some_and(|(_, rest)| !rest.is_empty() && !rest.ends_with('/'));
    (!complete).then(|| {
        "a memory URL is user://<category>, agent://<agent>, machine://<machine>/<name>, \
         or project://<project>/<name>"
            .to_owned()
    })
}

/// The memory's own name: what its URL says after its layer and group.
pub fn memory_name(memory: &MemoryInfo) -> &str {
    let rest = memory
        .url
        .split_once("://")
        .map_or(memory.url.as_str(), |(_, rest)| rest);
    rest.rsplit_once('/').map_or(rest, |(_, name)| name)
}

/// The keywords an agent's mode gives its memory, after its ID.
const MODE_KEYWORDS: [&str; 3] = ["primary", "subagent", "all"];

/// A memory as an editor shows it: its description, its keywords, and its
/// body, each a field of its own. The daemon owns an `agent://` file's
/// description and its first keywords (the agent's ID, then its mode) and
/// keeps them whatever a write gives; everything else is the user's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryParts {
    pub description: String,
    /// Whether the daemon owns the description: an `agent://` file's.
    pub owned_description: bool,
    /// The daemon's keywords: an `agent://` file's ID, then its mode;
    /// empty for any other memory.
    pub fixed: Vec<String>,
    pub extra: Vec<String>,
    pub body: String,
}

/// The parts of `memory`, whose whole text as read at `revision` is
/// `text`, from what `memory.list` says of it. `None`, so an editor shows
/// the text whole, when the listing is of another revision, the file is
/// invalid or has no frontmatter, or a keyword holds a comma, which the
/// keywords field separates them with.
pub fn memory_parts(memory: &MemoryInfo, revision: &str, text: &str) -> Option<MemoryParts> {
    if memory.revision != revision || memory.invalid.is_some() {
        return None;
    }
    let description = memory.description.clone()?;
    if memory.keywords.iter().any(|keyword| keyword.contains(',')) {
        return None;
    }
    let body = frontmatter_body(text)?.to_owned();
    let Some(agent) = memory
        .url
        .strip_prefix("agent://")
        .filter(|_| memory.layer == MemoryLayer::Agent)
    else {
        return Some(MemoryParts {
            description,
            owned_description: false,
            fixed: Vec::new(),
            extra: memory.keywords.clone(),
            body,
        });
    };
    let mut keywords = memory.keywords.iter();
    let mut fixed = Vec::new();
    let mut extra = Vec::new();
    for keyword in keywords.by_ref() {
        let owned = match fixed.len() {
            0 => keyword == agent,
            1 => agent != keyword && MODE_KEYWORDS.contains(&keyword.as_str()),
            _ => false,
        };
        if owned {
            fixed.push(keyword.clone());
        } else {
            extra.push(keyword.clone());
            break;
        }
    }
    extra.extend(keywords.cloned());
    Some(MemoryParts {
        description,
        owned_description: true,
        fixed,
        extra,
        body,
    })
}

/// The parts a new memory at `url` starts from, which are [`NEW_MEMORY`]:
/// all empty. `None` for an `agent://` file, whose description the daemon
/// gives, so an editor shows the template whole.
pub fn new_memory_parts(url: &str) -> Option<MemoryParts> {
    (!url.starts_with("agent://")).then(|| MemoryParts {
        description: String::new(),
        owned_description: false,
        fixed: Vec::new(),
        extra: Vec::new(),
        body: String::new(),
    })
}

/// What follows the frontmatter of `text`: the lines after the `---` that
/// closes the one that opens it.
fn frontmatter_body(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end() == "---" {
            return Some(&rest[offset..]);
        }
    }
    None
}

impl MemoryParts {
    /// The extra keywords of a comma-separated field.
    pub fn typed_keywords(typed: &str) -> Vec<String> {
        typed
            .split(',')
            .map(str::trim)
            .filter(|keyword| !keyword.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// The whole file to write: the frontmatter with the description and
    /// the keywords, the daemon's (which it keeps anyway) first, then the
    /// body. Scalars are double-quoted JSON strings,
    /// which YAML reads as the same text.
    pub fn text(&self) -> String {
        let quote = |value: &str| serde_json::to_string(value).unwrap_or_default();
        let mut text = format!("---\ndescription: {}\n", quote(&self.description));
        let keywords = self
            .fixed
            .iter()
            .chain(&self.extra)
            .map(|keyword| quote(keyword))
            .collect::<Vec<_>>();
        if !keywords.is_empty() {
            text.push_str(&format!("keywords: [{}]\n", keywords.join(", ")));
        }
        text.push_str("---\n");
        text.push_str(&self.body);
        text
    }
}

/// The client protocol minor from which a daemon stamps every memory with
/// `created` and `modified`, reports them with `last_read` and the size
/// limits in `memory.list` and `memory.read`, and owns the stamp lines of
/// a memory's frontmatter, rewriting them on every save.
pub const MEMORY_STAMPS_MINOR: u32 = 14;

/// Whether a daemon whose handshake reported `version` owns the stamp lines
/// of a memory's frontmatter, so an editor leaves them out. An older
/// daemon's files have none of its own, and whatever a user wrote there is
/// theirs to edit.
#[must_use]
pub fn daemon_stamps_memory(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= MEMORY_STAMPS_MINOR)
    })
}

/// The frontmatter keys the daemon owns on every memory.
const STAMP_KEYS: [&str; 2] = ["created", "modified"];

/// `text` without the stamp lines of its frontmatter (`created:` and
/// `modified:` at the top level), the rest kept byte for byte. The daemon
/// writes its own stamps on every save whatever the text says, so an editor
/// shows the text without them and saves it as it is left. A text without
/// frontmatter is returned whole.
pub fn without_stamps(text: &str) -> String {
    let Some(opening) = ["---\n", "---\r\n"]
        .into_iter()
        .find(|opening| text.starts_with(opening))
    else {
        return text.to_owned();
    };
    let mut kept = String::with_capacity(text.len());
    kept.push_str(opening);
    let mut lines = text[opening.len()..].split_inclusive('\n');
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            kept.push_str(line);
            break;
        }
        let stamp = STAMP_KEYS.iter().any(|key| {
            line.strip_prefix(key)
                .is_some_and(|rest| rest.starts_with(':'))
        });
        if !stamp {
            kept.push_str(line);
        }
    }
    for line in lines {
        kept.push_str(line);
    }
    kept
}

/// The text an editor opens for a memory read from a daemon of `version`:
/// without the stamp lines when the daemon owns them.
pub fn editor_text(text: &str, version: Option<SemVer>) -> String {
    if daemon_stamps_memory(version) {
        without_stamps(text)
    } else {
        text.to_owned()
    }
}

/// What a memory's note says when it is over its limit.
pub const OVER_LIMIT: &str = "over its limit";

/// What the daemon accepts of a memory over its limit.
pub const OVER_LIMIT_NOTE: &str =
    "It is over its limit: only changes that make it smaller are saved until it fits.";

/// A memory's stamps and size as `memory.list` or `memory.read` report
/// them; each is absent from a daemon older than client protocol 1.14.0.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MemoryStamps {
    pub created: Option<Timestamp>,
    pub modified: Option<Timestamp>,
    /// When a model last read it, if the daemon knows.
    pub last_read: Option<Timestamp>,
    /// The file's size in bytes, frontmatter included.
    pub size: Option<u64>,
    /// The `[memory]` limit of its kind, in bytes.
    pub max_size: Option<u64>,
}

/// One of a memory's times, for a line of its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryTime {
    /// `created`, `modified`, or `last read`.
    pub label: &'static str,
    pub at: Timestamp,
}

impl MemoryTime {
    /// `created: 2026-10-01 09:12:44 UTC (3 weeks ago)`.
    pub fn line(&self, now: Timestamp) -> String {
        format!(
            "{}: {} ({})",
            self.label,
            exact_time(self.at),
            ago(self.at, now)
        )
    }
}

impl MemoryStamps {
    pub fn of_info(memory: &MemoryInfo) -> Self {
        Self {
            created: memory.created,
            modified: memory.modified,
            last_read: memory.last_read,
            size: Some(memory.size),
            max_size: memory.max_size,
        }
    }

    /// The stamps of a read; `last_read` comes from the listing, which
    /// alone reports it.
    pub fn of_read(read: &MemoryReadResult, last_read: Option<Timestamp>) -> Self {
        Self {
            created: read.created,
            modified: read.modified,
            last_read,
            size: read.size,
            max_size: read.max_size,
        }
    }

    /// Whether the memory is over its limit.
    pub fn over_limit(&self) -> bool {
        matches!((self.size, self.max_size), (Some(size), Some(max)) if size > max)
    }

    /// The times known, in the order created, modified, last read.
    pub fn times(&self) -> Vec<MemoryTime> {
        [
            ("created", self.created),
            ("modified", self.modified),
            ("last read", self.last_read),
        ]
        .into_iter()
        .filter_map(|(label, at)| at.map(|at| MemoryTime { label, at }))
        .collect()
    }

    /// `812 bytes of 2 KB`, `2.5 KB of 2 KB, over its limit`, or only the
    /// size without a limit; `None` without a size.
    pub fn size_text(&self) -> Option<String> {
        let size = memory_size(self.size?);
        Some(match self.max_size {
            Some(max) if self.over_limit() => {
                format!("{size} of {}, {OVER_LIMIT}", memory_size(max))
            }
            Some(max) => format!("{size} of {}", memory_size(max)),
            None => size,
        })
    }

    /// The lines a detail view shows: each time known with its exact time
    /// and age, then the size and limit.
    pub fn detail_lines(&self, now: Timestamp) -> Vec<String> {
        let mut lines = self
            .times()
            .iter()
            .map(|time| time.line(now))
            .collect::<Vec<_>>();
        if let Some(size) = self.size_text() {
            lines.push(format!("size: {size}"));
        }
        lines
    }

    /// Every time known as an age, then the size and limit, such as
    /// `created 3 weeks ago · modified 1 day ago · last read 2 minutes ago
    /// · 812 bytes of 2 KB`.
    pub fn ages(&self, now: Timestamp) -> String {
        let mut parts = self
            .times()
            .iter()
            .map(|time| format!("{} {}", time.label, ago(time.at, now)))
            .collect::<Vec<_>>();
        parts.extend(self.size_text());
        parts.join(" · ")
    }

    /// One short line for a row: when it was modified, then its size and
    /// limit, such as `modified 2 days ago · 812 bytes of 2 KB`.
    pub fn summary(&self, now: Timestamp) -> String {
        let mut parts = Vec::new();
        if let Some(modified) = self.modified {
            parts.push(format!("modified {}", ago(modified, now)));
        }
        if let Some(size) = self.size_text() {
            parts.push(size);
        }
        parts.join(" · ")
    }

    /// The exact times, one per line, for a hover: `created: 2026-10-01
    /// 09:12:44 UTC`.
    pub fn exact_times(&self) -> String {
        self.times()
            .iter()
            .map(|time| format!("{}: {}", time.label, exact_time(time.at)))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// How long before `now` `then` was: `3 weeks ago`, `less than a minute
/// ago`, in the units memory reads ages in.
pub fn ago(then: Timestamp, now: Timestamp) -> String {
    format!("{} ago", memory_age(then, now))
}

/// `at` in UTC to the second, such as `2026-10-01 09:12:44 UTC`. Formatted
/// by hand: jiff's strftime would grow the web page.
pub fn exact_time(at: Timestamp) -> String {
    let time = jiff::tz::TimeZone::UTC.to_datetime(at);
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        time.year(),
        time.month(),
        time.day(),
        time.hour(),
        time.minute(),
        time.second()
    )
}

/// The heading of a layer.
pub const fn layer_title(layer: MemoryLayer) -> &'static str {
    match layer {
        MemoryLayer::User => "User",
        MemoryLayer::Agent => "Agents",
        MemoryLayer::Machine => "Machines",
        MemoryLayer::Project => "Projects",
        MemoryLayer::Unknown => "Other",
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::MemoryListResult;

    use super::{
        MemoryParts, MemoryRow, MemoryStamps, daemon_stamps_memory, editor_text, memory_parts,
        memory_rows, new_memory_parts, new_memory_refusal, saved_status, without_stamps,
    };

    fn listing() -> MemoryListResult {
        serde_json::from_value(serde_json::json!({
            "memories": [
                {"url": "project://shop/build", "layer": "project", "group": "shop",
                 "revision": "r1", "size": 10, "description": "how to build"}
            ],
            "ignored": []
        }))
        .expect("listing")
    }

    #[test]
    fn memories_group_under_their_layer_and_project() {
        let rows = memory_rows(&listing());
        let group = rows
            .iter()
            .position(|row| matches!(row, MemoryRow::Group { name, .. } if name == "shop"))
            .expect("the project's group");
        assert!(
            matches!(&rows[group + 1], MemoryRow::Memory(memory) if memory.url == "project://shop/build")
        );
        assert_eq!(rows[group + 1].new_prefix(), "project://shop/");
    }

    #[test]
    fn a_new_memory_needs_a_complete_free_url() {
        assert!(new_memory_refusal(&listing(), "project://shop/test").is_none());
        assert!(new_memory_refusal(&listing(), "project://shop/build").is_some());
        assert!(new_memory_refusal(&listing(), "project://shop/").is_some());
    }

    fn agent_memory(keywords: &[&str]) -> cookie_agent_protocol::MemoryInfo {
        serde_json::from_value(serde_json::json!({
            "url": "agent://reviewer", "layer": "agent", "revision": "r1", "size": 10,
            "description": "Memory of the `reviewer` agent: reviews diffs",
            "keywords": keywords
        }))
        .expect("memory")
    }

    #[test]
    fn an_agent_file_splits_the_daemons_keywords_from_the_users() {
        let text = "---\ndescription: x\nkeywords: [reviewer, subagent, rust]\n---\nPrefers small diffs.\n";
        let parts = memory_parts(&agent_memory(&["reviewer", "subagent", "rust"]), "r1", text)
            .expect("an agent file");
        assert!(parts.owned_description);
        assert_eq!(parts.fixed, ["reviewer", "subagent"]);
        assert_eq!(parts.extra, ["rust"]);
        assert_eq!(parts.body, "Prefers small diffs.\n");
        assert_eq!(
            parts.description,
            "Memory of the `reviewer` agent: reviews diffs"
        );
        // A file whose keywords lost the mode keeps the rest as the user's.
        let parts = memory_parts(&agent_memory(&["reviewer", "rust", "subagent"]), "r1", text)
            .expect("an agent file");
        assert_eq!(parts.fixed, ["reviewer"]);
        assert_eq!(parts.extra, ["rust", "subagent"]);
    }

    #[test]
    fn any_other_memory_is_all_the_users() {
        let mut project = agent_memory(&["reviewer", "subagent"]);
        project.url = "project://shop/reviewer".into();
        project.layer = cookie_agent_protocol::MemoryLayer::Project;
        let parts = memory_parts(&project, "r1", "---\ndescription: x\n---\nBody.\n")
            .expect("a project memory");
        assert!(!parts.owned_description);
        assert!(parts.fixed.is_empty());
        assert_eq!(parts.extra, ["reviewer", "subagent"]);
        assert_eq!(parts.body, "Body.\n");
    }

    #[test]
    fn a_memory_the_fields_cannot_hold_is_shown_whole() {
        let text = "---\ndescription: x\n---\nBody.\n";
        let memory = agent_memory(&["reviewer"]);
        assert!(memory_parts(&memory, "r1", "no frontmatter").is_none());
        // The listing is of another revision than the text read.
        assert!(memory_parts(&memory, "r2", text).is_none());
        let mut invalid = memory.clone();
        invalid.invalid = Some("bad".into());
        assert!(memory_parts(&invalid, "r1", text).is_none());
        let mut comma = memory;
        comma.keywords.push("a, b".into());
        assert!(memory_parts(&comma, "r1", text).is_none());
    }

    #[test]
    fn a_new_memory_starts_from_empty_parts_unless_an_agents() {
        let parts = new_memory_parts("project://shop/notes").expect("a project memory");
        assert_eq!(parts.text(), "---\ndescription: \"\"\n---\n");
        assert!(new_memory_parts("agent://reviewer").is_none());
    }

    #[test]
    fn an_agent_file_is_written_with_its_parts_quoted() {
        let parts = MemoryParts {
            description: "Memory of the `reviewer` agent: \"diffs\"".into(),
            owned_description: true,
            fixed: vec!["reviewer".into(), "subagent".into()],
            extra: MemoryParts::typed_keywords(" rust , ,a: b"),
            body: "Body.\n".into(),
        };
        assert_eq!(
            parts.text(),
            "---\ndescription: \"Memory of the `reviewer` agent: \\\"diffs\\\"\"\n\
             keywords: [\"reviewer\", \"subagent\", \"rust\", \"a: b\"]\n---\nBody.\n"
        );
    }

    #[test]
    fn an_editor_leaves_out_only_the_frontmatters_stamp_lines() {
        let text = "---\ndescription: How releases are cut\ncreated: 2026-10-01T09:12:44Z\n\
                    keywords: [release]\nmodified: 2026-10-20T16:03:10Z\ncreated_by: me\n  \
                    created: nested\n---\ncreated: in the body\nmodified: too\n";
        assert_eq!(
            without_stamps(text),
            "---\ndescription: How releases are cut\nkeywords: [release]\ncreated_by: me\n  \
             created: nested\n---\ncreated: in the body\nmodified: too\n"
        );
        let crlf = "---\r\ndescription: x\r\ncreated: 2026-10-01T09:12:44Z\r\n---\r\nbody\r\n";
        assert_eq!(
            without_stamps(crlf),
            "---\r\ndescription: x\r\n---\r\nbody\r\n"
        );
        assert_eq!(
            without_stamps("created: no frontmatter\n"),
            "created: no frontmatter\n"
        );
    }

    #[test]
    fn only_a_daemon_of_minor_fourteen_owns_the_stamp_lines() {
        use cookie_agent_protocol::SemVer;
        let text = "---\ndescription: x\ncreated: 2026-10-01T09:12:44Z\n---\n";
        assert!(!daemon_stamps_memory(None));
        assert!(!daemon_stamps_memory(Some(SemVer::new(1, 13, 0))));
        assert!(daemon_stamps_memory(Some(SemVer::new(1, 14, 0))));
        assert!(!daemon_stamps_memory(Some(SemVer::new(2, 14, 0))));
        assert_eq!(
            editor_text(text, Some(SemVer::new(1, 14, 2))),
            "---\ndescription: x\n---\n"
        );
        assert_eq!(editor_text(text, Some(SemVer::new(1, 13, 0))), text);
    }

    fn at(text: &str) -> jiff::Timestamp {
        text.parse().expect("timestamp")
    }

    #[test]
    fn stamps_read_as_ages_exact_times_and_a_size_against_the_limit() {
        let now = at("2026-10-22T12:00:00Z");
        let stamps = MemoryStamps {
            created: Some(at("2026-10-01T09:12:44Z")),
            modified: Some(at("2026-10-20T16:03:10Z")),
            last_read: Some(at("2026-10-22T11:58:00Z")),
            size: Some(812),
            max_size: Some(2048),
        };
        assert!(!stamps.over_limit());
        assert_eq!(
            stamps.detail_lines(now),
            [
                "created: 2026-10-01 09:12:44 UTC (3 weeks ago)",
                "modified: 2026-10-20 16:03:10 UTC (1 day ago)",
                "last read: 2026-10-22 11:58:00 UTC (2 minutes ago)",
                "size: 812 bytes of 2 KB",
            ]
        );
        assert_eq!(
            stamps.summary(now),
            "modified 1 day ago · 812 bytes of 2 KB"
        );
        assert_eq!(
            stamps.ages(now),
            "created 3 weeks ago · modified 1 day ago · last read 2 minutes ago · 812 bytes of 2 KB"
        );
        assert_eq!(
            stamps.exact_times(),
            "created: 2026-10-01 09:12:44 UTC\nmodified: 2026-10-20 16:03:10 UTC\n\
             last read: 2026-10-22 11:58:00 UTC"
        );

        let over = MemoryStamps {
            size: Some(2611),
            max_size: Some(2048),
            ..MemoryStamps::default()
        };
        assert!(over.over_limit());
        assert_eq!(
            over.size_text().as_deref(),
            Some("2.5 KB of 2 KB, over its limit")
        );
        assert_eq!(over.summary(now), "2.5 KB of 2 KB, over its limit");
        assert!(over.times().is_empty());

        // An older daemon reports a size and nothing else.
        let older = MemoryStamps {
            size: Some(64),
            ..MemoryStamps::default()
        };
        assert!(!older.over_limit());
        assert_eq!(older.detail_lines(now), ["size: 64 bytes"]);
    }

    #[test]
    fn a_listing_and_a_read_give_their_stamps() {
        let info: cookie_agent_protocol::MemoryInfo = serde_json::from_value(serde_json::json!({
            "url": "user://style", "layer": "user", "revision": "r1", "size": 3000,
            "created": "2026-10-01T09:12:44Z", "modified": "2026-10-20T16:03:10Z",
            "last_read": "2026-10-21T08:00:00Z", "max_size": 2048
        }))
        .expect("memory");
        let listed = MemoryStamps::of_info(&info);
        assert!(listed.over_limit());
        assert_eq!(listed.last_read, Some(at("2026-10-21T08:00:00Z")));
        let read: cookie_agent_protocol::MemoryReadResult =
            serde_json::from_value(serde_json::json!({
                "url": "user://style", "revision": "r1", "text": "---\n---\n",
                "created": "2026-10-01T09:12:44Z", "modified": "2026-10-20T16:03:10Z",
                "size": 3000, "max_size": 2048
            }))
            .expect("read");
        assert_eq!(MemoryStamps::of_read(&read, info.last_read), listed);
    }

    #[test]
    fn a_save_says_what_the_daemon_kept() {
        assert_eq!(
            saved_status("user://style", false, None),
            "Saved user://style."
        );
        assert_eq!(
            saved_status(
                "agent://reviewer",
                true,
                Some("The daemon kept its description.")
            ),
            "Created agent://reviewer. The daemon kept its description."
        );
    }
}
