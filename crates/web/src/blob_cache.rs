//! What the page keeps of files it read, bounded by bytes: each holds a `Blob`
//! the browser keeps in memory until its URL is revoked, so a page open for
//! days would otherwise hold every image it ever showed.

use std::collections::HashMap;

/// How many bytes one cache keeps, beyond what is on the page.
pub const BUDGET_BYTES: u64 = 64 * 1024 * 1024;

struct Entry<V> {
    value: V,
    bytes: u64,
    used: u64,
}

/// Entries by key, the least recently used going first past the budget.
pub struct BlobCache<V> {
    entries: HashMap<String, Entry<V>>,
    bytes: u64,
    clock: u64,
    budget: u64,
}

impl<V: Clone> BlobCache<V> {
    pub fn new(budget: u64) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            clock: 0,
            budget,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    pub fn get(&mut self, key: &str) -> Option<V> {
        let used = self.tick();
        let entry = self.entries.get_mut(key)?;
        entry.used = used;
        Some(entry.value.clone())
    }

    /// Keeps `value`, returning what left to make room, for the caller to
    /// release. An entry `in_use` stays whatever the budget, and so does the
    /// one just added.
    pub fn insert(
        &mut self,
        key: String,
        value: V,
        bytes: u64,
        in_use: impl Fn(&V) -> bool,
    ) -> Vec<V> {
        let used = self.tick();
        let mut left = Vec::new();
        if let Some(replaced) = self
            .entries
            .insert(key.clone(), Entry { value, bytes, used })
        {
            self.bytes -= replaced.bytes;
            left.push(replaced.value);
        }
        self.bytes += bytes;
        while self.bytes > self.budget {
            let Some(oldest) = self
                .entries
                .iter()
                .filter(|(other, entry)| **other != key && !in_use(&entry.value))
                .min_by_key(|(_, entry)| entry.used)
                .map(|(other, _)| other.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.bytes -= entry.bytes;
                left.push(entry.value);
            }
        }
        left
    }
}

/// Whether `url` is shown by an element on the page, so revoking it would break
/// what is on screen.
pub fn url_on_page(url: &str) -> bool {
    web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| {
            document
                .query_selector(&format!("[src=\"{url}\"],[href=\"{url}\"]"))
                .ok()
                .flatten()
        })
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::BlobCache;

    fn add(
        cache: &mut BlobCache<&'static str>,
        name: &'static str,
        bytes: u64,
    ) -> Vec<&'static str> {
        cache.insert(name.to_owned(), name, bytes, |_| false)
    }

    #[test]
    fn the_least_recently_used_goes_past_the_budget() {
        let mut cache = BlobCache::new(100);
        assert!(add(&mut cache, "a", 40).is_empty());
        assert!(add(&mut cache, "b", 40).is_empty());
        assert_eq!(cache.get("a"), Some("a"));
        assert_eq!(add(&mut cache, "c", 40), ["b"]);
        assert_eq!(cache.get("b"), None);
        assert_eq!(cache.get("a"), Some("a"));
    }

    #[test]
    fn what_is_on_the_page_stays_past_the_budget() {
        let mut cache = BlobCache::new(100);
        add(&mut cache, "shown", 60);
        add(&mut cache, "other", 30);
        let left = cache.insert("c".to_owned(), "c", 40, |value| *value == "shown");
        assert_eq!(left, ["other"]);
        assert_eq!(cache.get("shown"), Some("shown"));
    }

    #[test]
    fn a_file_larger_than_the_budget_stays_alone() {
        let mut cache = BlobCache::new(100);
        add(&mut cache, "a", 10);
        assert_eq!(add(&mut cache, "huge", 500), ["a"]);
        assert_eq!(cache.get("huge"), Some("huge"));
        // And it goes first when the next arrives.
        assert_eq!(add(&mut cache, "b", 10), ["huge"]);
    }

    #[test]
    fn replacing_an_entry_counts_it_once_and_hands_the_old_one_back() {
        let mut cache = BlobCache::new(100);
        cache.insert("a".to_owned(), "old", 90, |_| false);
        assert_eq!(cache.insert("a".to_owned(), "new", 90, |_| false), ["old"]);
        assert!(add(&mut cache, "b", 10).is_empty());
    }
}
