//! Threads (`docs/site/specs/webhooks.md` sections 7.4, 7.5, and 8): the
//! sessions an owner sends its fires or events into by key.
//!
//! An owner (a cron job or a webhook with a new-session target)
//! keeps a [`ThreadMap`] beside its definition, in its own store. A
//! delivery whose key has a live session goes into it; one whose key has
//! none creates a session from the owner's template and records it
//! ([`crate::runtime::threads`]). Every owner's sessions may get its
//! thread prompt, a plain user message the engine submits at a session's
//! start and again after each compaction; [`ThreadOwner`] names the owner
//! a prompt and a producer message belong to.

use std::collections::HashSet;

use cookie_agent_protocol::{
    CronJobId, CronThread, EventOrigin, ProducerOwner, SessionId, ThreadOwnerRef, WebhookId,
    thread_prompt_origin, thread_prompt_owner,
};
use jiff::{Timestamp, tz::TimeZone};
use serde::{Deserialize, Serialize};

/// The most threads an owner remembers. Recording one more forgets the
/// thread used least recently; its session stays, and its key's next
/// delivery starts a new one.
pub(crate) const THREAD_LIMIT: usize = 1024;

/// What threads belong to.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ThreadOwner {
    /// A cron job with a new-session target.
    Cron(CronJobId),
    /// A webhook with a new-session target.
    Webhook(WebhookId),
}

impl ThreadOwner {
    /// The owner the protocol names.
    pub(crate) fn reference(&self) -> ThreadOwnerRef {
        match self {
            Self::Cron(job) => ThreadOwnerRef::Cron(job.clone()),
            Self::Webhook(webhook) => ThreadOwnerRef::Webhook(webhook.clone()),
        }
    }

    /// The origin of the thread prompts this owner sends.
    pub(crate) fn origin(&self) -> EventOrigin {
        thread_prompt_origin(&self.reference())
    }

    /// The owner of the thread prompt `origin` marks.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn from_origin(origin: &EventOrigin) -> Option<Self> {
        match thread_prompt_owner(origin)? {
            ThreadOwnerRef::Cron(job) => Some(Self::Cron(job)),
            ThreadOwnerRef::Webhook(webhook) => Some(Self::Webhook(webhook)),
            _ => None,
        }
    }

    /// The owner whose fires or events `owner`'s producer messages carry.
    pub(crate) fn of_producer(owner: &ProducerOwner) -> Option<Self> {
        let source = owner.source()?;
        if let Some(webhook) = source.webhook_id() {
            return WebhookId::new(webhook.to_owned()).ok().map(Self::Webhook);
        }
        let job = source.cron_job()?;
        CronJobId::new(job.to_owned()).ok().map(Self::Cron)
    }
}

/// One remembered thread.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadEntry {
    pub(crate) key: String,
    pub(crate) session: SessionId,
    pub(crate) last_used: Timestamp,
}

/// An owner's threads, the most recently used last, at most
/// [`THREAD_LIMIT`].
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct ThreadMap(Vec<ThreadEntry>);

impl ThreadMap {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The session `key`'s thread is.
    pub(crate) fn session(&self, key: &str) -> Option<SessionId> {
        self.0
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.session)
    }

    /// Records that `key`'s thread is `session`, used at `at`: the entry
    /// moves to the end, and the least recently used one goes past
    /// [`THREAD_LIMIT`].
    pub(crate) fn record(&mut self, key: &str, session: SessionId, at: Timestamp) {
        self.0.retain(|entry| entry.key != key);
        self.0.push(ThreadEntry {
            key: key.to_owned(),
            session,
            last_used: at,
        });
        let excess = self.0.len().saturating_sub(THREAD_LIMIT);
        self.0.drain(..excess);
    }

    /// Forgets the threads whose sessions are among `deleted`. Whether any
    /// was.
    pub(crate) fn forget_sessions(&mut self, deleted: &HashSet<SessionId>) -> bool {
        let before = self.0.len();
        self.0.retain(|entry| !deleted.contains(&entry.session));
        self.0.len() != before
    }

    /// Whether any thread is one of `sessions`.
    pub(crate) fn names_any(&self, sessions: &HashSet<SessionId>) -> bool {
        self.0.iter().any(|entry| sessions.contains(&entry.session))
    }

    /// The threads as clients read them.
    pub(crate) fn wire(&self) -> Vec<CronThread> {
        self.0
            .iter()
            .map(|entry| CronThread {
                key: entry.key.clone(),
                session_id: entry.session,
                last_used: entry.last_used,
            })
            .collect()
    }
}

/// Checks a cron job's `thread_key`: a time format of at most
/// [`cookie_agent_protocol::CRON_THREAD_KEY_MAX_CHARS`] characters, without
/// control characters, that formats a time into a key that is not blank.
pub(crate) fn check_time_key_format(format: &str) -> Result<(), String> {
    if format.trim().is_empty() {
        return Err("must not be blank".into());
    }
    if format.chars().count() > cookie_agent_protocol::CRON_THREAD_KEY_MAX_CHARS {
        return Err(format!(
            "must be at most {} characters",
            cookie_agent_protocol::CRON_THREAD_KEY_MAX_CHARS
        ));
    }
    if format.chars().any(char::is_control) {
        return Err("must not contain control characters".into());
    }
    time_key(format, Timestamp::UNIX_EPOCH, &TimeZone::UTC).map(drop)
}

/// The key `format` gives for `at` in `zone`: `strftime`'s text, trimmed.
pub(crate) fn time_key(format: &str, at: Timestamp, zone: &TimeZone) -> Result<String, String> {
    let zoned = at.to_zoned(zone.clone());
    let key = jiff::fmt::strtime::format(format, &zoned)
        .map_err(|error| format!("is not a time format: {error}"))?;
    let key = key.trim();
    if key.is_empty() {
        return Err("formats a blank key".into());
    }
    if key.chars().count() > cookie_agent_protocol::THREAD_KEY_MAX_CHARS {
        return Err(format!(
            "formats a key longer than {} characters",
            cookie_agent_protocol::THREAD_KEY_MAX_CHARS
        ));
    }
    if key.chars().any(char::is_control) {
        return Err("formats a key with control characters".into());
    }
    Ok(key.to_owned())
}

/// Checks a `thread_prompt`: not blank, at most
/// [`cookie_agent_protocol::THREAD_PROMPT_MAX_BYTES`].
pub(crate) fn check_thread_prompt(prompt: &str) -> Result<(), String> {
    if prompt.trim().is_empty() {
        return Err("must not be blank".into());
    }
    if prompt.len() > cookie_agent_protocol::THREAD_PROMPT_MAX_BYTES {
        return Err(format!(
            "must be at most {} bytes",
            cookie_agent_protocol::THREAD_PROMPT_MAX_BYTES
        ));
    }
    Ok(())
}

/// Checks a new-session template's `title`
/// (`docs/site/specs/webhooks-view.md` section 6): not blank, at most
/// [`cookie_agent_protocol::SESSION_TITLE_TEMPLATE_MAX_CHARS`] characters,
/// and no control characters, as a rename's title.
pub(crate) fn check_session_title(title: &str) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err("must not be blank".into());
    }
    if title.chars().count() > cookie_agent_protocol::SESSION_TITLE_TEMPLATE_MAX_CHARS {
        return Err(format!(
            "must be at most {} characters",
            cookie_agent_protocol::SESSION_TITLE_TEMPLATE_MAX_CHARS
        ));
    }
    if title.chars().any(char::is_control) {
        return Err("must not hold a control character".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap()
    }

    #[test]
    fn a_daily_key_changes_at_local_midnight() {
        let berlin = zone("Europe/Berlin");
        // 22:59 UTC is 23:59 in Berlin in winter; a minute later is the
        // next day there, while UTC is still on the first.
        assert_eq!(
            time_key("%Y-%m-%d", at("2026-11-12T22:59:00Z"), &berlin).unwrap(),
            "2026-11-12"
        );
        assert_eq!(
            time_key("%Y-%m-%d", at("2026-11-12T23:00:00Z"), &berlin).unwrap(),
            "2026-11-13"
        );
    }

    #[test]
    fn an_iso_week_key_spans_the_new_year() {
        let utc = TimeZone::UTC;
        // 2026-12-31 (Thursday) and 2027-01-03 (Sunday) are both in ISO
        // week 53 of 2026; 2027-01-04 starts week 1 of 2027.
        assert_eq!(
            time_key("%G-W%V", at("2026-12-31T12:00:00Z"), &utc).unwrap(),
            "2026-W53"
        );
        assert_eq!(
            time_key("%G-W%V", at("2027-01-03T12:00:00Z"), &utc).unwrap(),
            "2026-W53"
        );
        assert_eq!(
            time_key("%G-W%V", at("2027-01-04T12:00:00Z"), &utc).unwrap(),
            "2027-W01"
        );
    }

    #[test]
    fn a_fall_back_fold_keeps_one_daily_key() {
        let berlin = zone("Europe/Berlin");
        // 2026-10-25 02:30 happens twice in Berlin: at 00:30 and 01:30 UTC.
        for instant in ["2026-10-25T00:30:00Z", "2026-10-25T01:30:00Z"] {
            assert_eq!(
                time_key("%Y-%m-%d", at(instant), &berlin).unwrap(),
                "2026-10-25"
            );
        }
    }

    #[test]
    fn a_format_without_directives_is_one_key() {
        let utc = TimeZone::UTC;
        assert_eq!(
            time_key("main", at("2026-01-01T00:00:00Z"), &utc).unwrap(),
            "main"
        );
        assert_eq!(
            time_key("main", at("2030-06-01T00:00:00Z"), &utc).unwrap(),
            "main"
        );
    }

    #[test]
    fn formats_are_checked() {
        assert!(check_time_key_format("%Y-%m-%d").is_ok());
        assert!(check_time_key_format("main").is_ok());
        for refused in ["", "   ", "%Y-%", "a\nb", &"x".repeat(65)] {
            assert!(check_time_key_format(refused).is_err(), "{refused:?}");
        }
        assert!(check_time_key_format(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn session_titles_are_checked() {
        assert!(check_session_title("Shop issues").is_ok());
        assert!(check_session_title("  ").is_err());
        assert!(check_session_title("line\nbreak").is_err());
        let limit = cookie_agent_protocol::SESSION_TITLE_TEMPLATE_MAX_CHARS;
        assert!(check_session_title(&"é".repeat(limit)).is_ok());
        assert!(check_session_title(&"é".repeat(limit + 1)).is_err());
    }

    #[test]
    fn thread_prompts_are_checked() {
        assert!(check_thread_prompt("Look after one issue.").is_ok());
        assert!(check_thread_prompt(" \n").is_err());
        let limit = cookie_agent_protocol::THREAD_PROMPT_MAX_BYTES;
        assert!(check_thread_prompt(&"x".repeat(limit)).is_ok());
        assert!(check_thread_prompt(&"x".repeat(limit + 1)).is_err());
    }

    fn session() -> SessionId {
        SessionId::new_v7()
    }

    #[test]
    fn recording_moves_a_thread_to_the_end_and_forgets_the_least_recent() {
        let mut map = ThreadMap::default();
        let (first, second) = (session(), session());
        map.record("a", first, at("2026-11-12T00:00:00Z"));
        map.record("b", second, at("2026-11-12T01:00:00Z"));
        map.record("a", first, at("2026-11-12T02:00:00Z"));
        assert_eq!(
            map.wire()
                .iter()
                .map(|thread| thread.key.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        for index in 0..THREAD_LIMIT {
            map.record(&format!("k{index}"), session(), at("2026-11-13T00:00:00Z"));
        }
        assert_eq!(map.wire().len(), THREAD_LIMIT);
        assert_eq!(map.session("b"), None);
        assert_eq!(map.session("a"), None);
    }

    #[test]
    fn deleted_sessions_leave_the_map() {
        let mut map = ThreadMap::default();
        let (kept, deleted) = (session(), session());
        map.record("kept", kept, at("2026-11-12T00:00:00Z"));
        map.record("gone", deleted, at("2026-11-12T00:00:00Z"));
        assert!(map.forget_sessions(&HashSet::from([deleted])));
        assert!(!map.forget_sessions(&HashSet::from([deleted])));
        assert_eq!(map.session("gone"), None);
        assert_eq!(map.session("kept"), Some(kept));
    }

    #[test]
    fn a_cron_owner_is_found_from_its_origin_and_its_producer() {
        let job = CronJobId::new("k3v9q2xa".to_owned()).unwrap();
        let owner = ThreadOwner::Cron(job.clone());
        assert_eq!(
            ThreadOwner::from_origin(&owner.origin()),
            Some(owner.clone())
        );
        let producer = ProducerOwner::external(
            cookie_agent_protocol::ProducerSource::cron(&job),
            cookie_agent_protocol::ProducerName::new("scheduled").unwrap(),
        );
        assert_eq!(ThreadOwner::of_producer(&producer), Some(owner));
    }
}
