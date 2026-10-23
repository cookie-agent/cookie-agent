//! When each memory was last read (`docs/site/specs/memory.md` section
//! 16.9.6), kept outside the memory files in `memory/.reads.json`, a map
//! from memory URL to time. A model's `memory_read` and a snapshot that
//! shows a memory whole count as reads; a client's does not.
//!
//! The times are advisory: the file is read once, kept in memory, and
//! rewritten through a temporary file after each change, without the
//! memory lock. Two daemons sharing a data root can each drop the other's
//! latest times, and a URL whose memory moves or goes keeps its entry.

use std::{collections::BTreeMap, fs};

use jiff::Timestamp;

use super::{MemoryRoot, READS_FILE, file::to_second, store::write_file, url::MemoryUrl};

/// The last-read times, loaded from the file on first use.
#[derive(Debug, Default)]
pub(crate) struct ReadTimes(std::sync::Mutex<Option<BTreeMap<String, Timestamp>>>);

impl MemoryRoot {
    fn with_reads<T>(&self, work: impl FnOnce(&mut BTreeMap<String, Timestamp>) -> T) -> T {
        let mut guard = self
            .reads
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let times = guard.get_or_insert_with(|| {
            fs::read(self.root.join(READS_FILE))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default()
        });
        work(times)
    }

    /// Records that `urls` were read at `now`. Nothing is written while the
    /// memory folder does not exist; a failure to write is reported on
    /// standard error, since the times are advisory.
    pub(crate) fn record_reads<'a>(
        &self,
        urls: impl IntoIterator<Item = &'a MemoryUrl>,
        now: Timestamp,
    ) {
        let now = to_second(now);
        let text = self.with_reads(|times| {
            let mut changed = false;
            for url in urls {
                changed |= times.insert(url.to_string(), now) != Some(now);
            }
            changed.then(|| serde_json::to_vec(times).expect("read times serialize"))
        });
        if let Some(text) = text
            && self.root.is_dir()
            && let Err(error) = write_file(&self.root.join(READS_FILE), &text)
        {
            eprintln!("cookie-agent: memory read times: {error}");
        }
    }

    /// When the memory at `url` was last read, if the daemon knows.
    pub(crate) fn last_read(&self, url: &MemoryUrl) -> Option<Timestamp> {
        self.with_reads(|times| times.get(&url.to_string()).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_times_persist_beside_the_memories() {
        let data = tempfile::tempdir().unwrap();
        let memory = MemoryRoot::new(data.path());
        assert!(memory.create_defaults_if_absent().unwrap());
        let url = MemoryUrl::parse("user://feedback").unwrap();
        assert_eq!(memory.last_read(&url), None);
        let at = Timestamp::from_second(1_000_000_000).unwrap();
        memory.record_reads([&url], at);
        assert_eq!(memory.last_read(&url), Some(at));
        let reopened = MemoryRoot::new(data.path());
        assert_eq!(reopened.last_read(&url), Some(at));
        // The file is not a memory, nor something a listing reports.
        let listing = reopened.list(&super::super::store::ListScope::All).unwrap();
        assert!(listing.ignored.is_empty(), "{:?}", listing.ignored);
    }
}
