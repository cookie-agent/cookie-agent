//! A session that runs forever costs no more for its log being longer
//! (`docs/site/specs/forever-sessions.md` section 1): what a windowed log holds,
//! its `state.json`, and opening it from that, at a length and at four times it.
//!
//! A test binary of its own, as it counts the heap with its own allocator. Bytes
//! are compared closely, and time loosely: a test machine's load moves it.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use cookie_agent_engine_store::{
    events::{
        state_file::STATE_FILE,
        synthetic::{Mix, SESSION, write_file},
    },
    session::{SessionStore, skip_reference_fold},
};
use cookie_agent_protocol::SessionId;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        LIVE.fetch_add(size, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// What a log of a length costs, once opened from its `state.json`.
struct Cost {
    log_bytes: u64,
    live: usize,
    state_file: u64,
    open: Duration,
}

fn store(directory: &Path) -> Arc<SessionStore> {
    let cwd = directory.join("workspace");
    std::fs::create_dir_all(&cwd).expect("a workspace");
    SessionStore::open(&directory.join("data"), &cwd).expect("a store")
}

fn cost_of(runs: u64) -> Cost {
    let directory = tempfile::tempdir().expect("a directory");
    let session: SessionId = SESSION.parse().expect("a session");
    let dir = store(directory.path()).root_dir_for_test(session);
    // Without delegations: the registry a tree load rebuilds takes each one's
    // records, which a log keeps for as long as it lasts (section 12).
    let mix = Mix {
        delegations: 0.0,
        ..Mix::small(runs)
    };
    let (log_bytes, _) = write_file(&mix, &dir.join("events.jsonl")).expect("a synthetic log");
    // The first open folds the log from its start; leaving memory writes the file.
    {
        let store = store(directory.path());
        store.open_for_write(session).expect("an open");
        assert!(store.evict(session).expect("an eviction"));
    }
    let state_file = std::fs::metadata(dir.join(STATE_FILE))
        .expect("a state file")
        .len();
    let store = store(directory.path());
    let before = LIVE.load(Ordering::Relaxed);
    let start = Instant::now();
    let projection = store.open_for_write(session).expect("an open");
    let open = start.elapsed();
    let live = LIVE.load(Ordering::Relaxed).saturating_sub(before);
    assert!(
        projection.log.opened_from_state_for_test(),
        "the second open reads state.json"
    );
    drop(projection);
    Cost {
        log_bytes,
        live,
        state_file,
        open,
    }
}

#[test]
fn a_windowed_log_costs_no_more_at_four_times_its_length() {
    // The reference checks parse the whole log at each open, which is what this
    // measures the absence of; the store's unit tests run them.
    skip_reference_fold();
    let short = cost_of(150);
    let long = cost_of(600);
    for (name, cost) in [("short", &short), ("long", &long)] {
        eprintln!(
            "{name}: log {} B, held {} B, state.json {} B, open {:?}",
            cost.log_bytes, cost.live, cost.state_file, cost.open
        );
    }
    assert!(
        long.log_bytes > 3 * short.log_bytes,
        "the long log is about four times the short one"
    );
    // A producer message's key stays for as long as the log, 8 bytes and an ID,
    // and so does each ACP session the log records; everything else is bounded.
    let bytes_within = |long: u64, short: u64| long <= short + short / 10 + 64 * 1024;
    assert!(
        bytes_within(long.state_file, short.state_file),
        "state.json grew from {} to {} bytes",
        short.state_file,
        long.state_file
    );
    assert!(
        bytes_within(long.live as u64, short.live as u64),
        "what an open holds grew from {} to {} bytes",
        short.live,
        long.live
    );
    assert!(
        long.open <= short.open * 3 + Duration::from_millis(250),
        "an open from state.json took {:?}, against {:?}",
        long.open,
        short.open
    );
}
