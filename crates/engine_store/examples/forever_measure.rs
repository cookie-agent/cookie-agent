//! What a long session's log costs: `docs/site/specs/forever-sessions.md` section
//! 12, phase 5. Each command runs in a process of its own, so what it measures is
//! one open:
//!
//!     forever_measure generate <dir> <times>
//!     forever_measure open <dir>
//!
//! Live heap and peak are by a counting allocator. Scratch data goes under `<dir>`.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
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
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
        PEAK.fetch_max(live, Ordering::Relaxed);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn megabytes(bytes: usize) -> f64 {
    bytes as f64 / 1_000_000.0
}

fn store(dir: &Path) -> std::sync::Arc<SessionStore> {
    let cwd = dir.join("workspace");
    std::fs::create_dir_all(&cwd).expect("a workspace");
    SessionStore::open(&dir.join("data"), &cwd).expect("a store")
}

fn session() -> SessionId {
    SESSION.parse().expect("a session")
}

/// Sizes in bytes of what `state.json` holds, largest first, by the JSON of each
/// part, as deep as the parts that grow.
fn parts(bytes: &[u8]) -> Vec<(String, usize)> {
    fn size(value: &serde_json::Value) -> usize {
        serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).expect("a state file");
    let mut sizes = Vec::new();
    let folds = &value["folds"];
    let Some(folds) = folds.as_object() else {
        return sizes;
    };
    for (name, part) in folds {
        sizes.push((name.clone(), size(part)));
        if matches!(name.as_str(), "validation" | "projection" | "index")
            && let Some(inner) = part.as_object()
        {
            for (key, inner_part) in inner {
                sizes.push((format!("{name}.{key}"), size(inner_part)));
                if key == "state"
                    && let Some(state) = inner_part.as_object()
                {
                    for (key, state_part) in state {
                        sizes.push((format!("{name}.state.{key}"), size(state_part)));
                        if matches!(
                            key.as_str(),
                            "harvest" | "once" | "approvals" | "acp" | "run"
                        ) && let Some(deep) = state_part.as_object()
                        {
                            for (deep_key, deep_part) in deep {
                                sizes.push((
                                    format!("{name}.state.{key}.{deep_key}"),
                                    size(deep_part),
                                ));
                            }
                        }
                    }
                }
                if key == "fold"
                    && let Some(fold) = inner_part.as_object()
                {
                    for (key, fold_part) in fold {
                        sizes.push((format!("{name}.fold.{key}"), size(fold_part)));
                    }
                }
            }
        }
    }
    sizes.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
    sizes
}

fn main() {
    skip_reference_fold();
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let dir = Path::new(&arguments[1]);
    match arguments[0].as_str() {
        "generate" => {
            let times: f64 = arguments[2].parse().expect("how many times");
            let log = store(dir).root_dir_for_test(session()).join("events.jsonl");
            let start = Instant::now();
            let (bytes, records) = write_file(&Mix::largest_times(times), &log).expect("a log");
            println!(
                "generated {times}x: {:.1} MB, {records} records, {:.1} s",
                megabytes(bytes as usize),
                start.elapsed().as_secs_f64()
            );
        }
        "open" => {
            let store = store(dir);
            let directory = store.root_dir_for_test(session());
            let had_file = directory.join(STATE_FILE).is_file();
            let before = LIVE.load(Ordering::Relaxed);
            PEAK.store(before, Ordering::Relaxed);
            let start = Instant::now();
            let projection = store.open_for_write(session()).expect("an open");
            let elapsed = start.elapsed();
            let live = LIVE.load(Ordering::Relaxed) - before;
            let peak = PEAK.load(Ordering::Relaxed) - before;
            let diagnostics = projection.log.diagnostics();
            if !diagnostics.is_empty() {
                let mut reasons = std::collections::BTreeMap::new();
                for diagnostic in diagnostics {
                    *reasons.entry(diagnostic.reason.as_str()).or_insert(0) += 1;
                }
                println!("damage: {reasons:#?}");
            }
            let records = projection.log.record_count();
            let window = projection.log.recent().len();
            println!(
                "open {}: {:.0} ms, live {:.2} MB, peak {:.2} MB, {records} records, {window} held, {} runs",
                if had_file {
                    "from state.json"
                } else {
                    "by fold"
                },
                elapsed.as_secs_f64() * 1000.0,
                megabytes(live),
                megabytes(peak),
                projection.runs.len(),
            );
            drop(projection);
            let before = LIVE.load(Ordering::Relaxed);
            PEAK.store(before, Ordering::Relaxed);
            let start = Instant::now();
            assert!(store.evict(session()).expect("an eviction"));
            let elapsed = start.elapsed();
            let bytes = std::fs::read(directory.join(STATE_FILE)).expect("a state file");
            println!(
                "write: {:.0} ms, peak {:.2} MB, state.json {:.3} MB",
                elapsed.as_secs_f64() * 1000.0,
                megabytes(PEAK.load(Ordering::Relaxed) - before),
                megabytes(bytes.len()),
            );
            if arguments.get(2).is_some_and(|flag| flag == "parts") {
                for (name, size) in parts(&bytes) {
                    if size >= 2_000 {
                        println!("  {:>12} {name}", size);
                    }
                }
            }
        }
        other => panic!("unknown command {other}"),
    }
}
