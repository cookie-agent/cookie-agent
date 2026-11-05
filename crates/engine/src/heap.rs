//! Hands memory the process has freed back to the operating system.
//!
//! glibc returns little by itself: a freed chunk stays resident unless it
//! touches the top of the heap, so after a long session is paged out its
//! memory stays with the process. The janitor asks here on a pass that finds
//! no run going, since a release holds the allocator's locks while it
//! walks the free lists, and memory goes back once enough has been freed
//! since the last time to be worth that walk.

/// How many bytes must have been freed since the last release before the
/// next one. As much as this can stay resident and unused.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const RELEASE_AFTER_FREED: usize = 32 * 1024 * 1024;

/// What the janitor remembers between its passes.
#[derive(Clone, Copy, Default)]
pub(crate) struct FreedMemory {
    /// The allocator's free bytes after the last release, or less if the
    /// process has since allocated from them. Released pages still count as
    /// free, so only what is above this was freed since.
    settled: usize,
}

impl FreedMemory {
    /// Whether `free` bytes hold `threshold` more than last settled.
    #[cfg_attr(not(all(target_os = "linux", target_env = "gnu")), allow(dead_code))]
    fn worth_releasing(&mut self, free: usize, threshold: usize) -> bool {
        self.settled = self.settled.min(free);
        free - self.settled >= threshold
    }

    /// Releases freed memory if enough has built up. Run it off the async
    /// workers.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    pub(crate) fn release(&mut self) {
        // SAFETY: both only read and update the allocator's own state, under
        // its own locks, and take no pointers from the caller.
        unsafe {
            if self.worth_releasing(libc::mallinfo2().fordblks, RELEASE_AFTER_FREED) {
                libc::malloc_trim(0);
                self.settled = libc::mallinfo2().fordblks;
            }
        }
    }

    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    pub(crate) fn release(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::FreedMemory;

    #[test]
    fn released_pages_that_still_count_as_free_do_not_release_again() {
        let mut freed = FreedMemory { settled: 500 };
        assert!(!freed.worth_releasing(500, 32));
        assert!(!freed.worth_releasing(531, 32));
        assert!(freed.worth_releasing(532, 32));
    }

    #[test]
    fn allocating_from_free_memory_lowers_what_counts_as_settled() {
        let mut freed = FreedMemory { settled: 500 };
        assert!(!freed.worth_releasing(100, 32));
        assert_eq!(freed.settled, 100);
        assert!(freed.worth_releasing(132, 32));
    }

    #[test]
    fn a_fresh_process_counts_all_its_free_memory() {
        assert!(FreedMemory::default().worth_releasing(32, 32));
        assert!(!FreedMemory::default().worth_releasing(31, 32));
    }
}
