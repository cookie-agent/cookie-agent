//! What a long session's log costs: `docs/site/specs/forever-sessions.md` section
//! 12, phase 5. Each command runs in a process of its own, so what it measures is
//! one open:
//!
//!     forever_measure generate <dir> <times> [<delegations a run> [native]]
//!     forever_measure open <dir> [parts]
//!
//! `native` makes it a native model's session that compacts (section 5).
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
                            "harvest" | "once" | "approvals" | "acp" | "run" | "calls"
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
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    // The reference checks parse each log whole at every open: a check runs them.
    if arguments[0] != "check" {
        skip_reference_fold();
    }
    let dir = Path::new(&arguments[1]);
    match arguments[0].as_str() {
        "generate" => {
            let times: f64 = arguments[2].parse().expect("how many times");
            let log = store(dir).root_dir_for_test(session()).join("events.jsonl");
            let start = Instant::now();
            let mut mix = Mix::largest_times(times);
            if let Some(rate) = arguments.get(3) {
                mix.delegations = rate.parse().expect("delegations a run");
            }
            if arguments.get(4).is_some_and(|kind| kind == "native") {
                mix = mix.native();
            }
            let (bytes, records) = write_file(&mix, &log).expect("a log");
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
        // A store's copy, converted to this build's format, each log opened a record
        // at a time and then from its state.json, each compared with a parse by
        // the reference checks: what a fold let go of never refuses a record.
        "check" => check(dir),
        other => panic!("unknown command {other}"),
    }
}

/// Every session log under `<dir>/data/sessions`, as `(id, path)`.
fn session_logs(dir: &Path) -> Vec<(SessionId, std::path::PathBuf)> {
    fn walk(path: &Path, logs: &mut Vec<(SessionId, std::path::PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<SessionId>().ok())
                && path.join("events.jsonl").is_file()
            {
                logs.push((id, path.join("events.jsonl")));
            }
            walk(&path, logs);
        }
    }
    let mut logs = Vec::new();
    walk(&dir.join("data").join("sessions"), &mut logs);
    logs.sort();
    logs
}

fn check(dir: &Path) {
    let upgrades = cookie_agent_engine_store::session::upgrade_sessions(&dir.join("data"))
        .expect("an upgrade");
    println!("trees: {}", upgrades.len());
    let logs = session_logs(dir);
    let (mut folded, mut damaged, mut refused, mut from_state, mut windowed) = (0, 0, 0, 0, 0);
    for (id, path) in &logs {
        let log = cookie_agent_engine_store::events::EventLog::open_read_only(path.clone(), *id)
            .expect("a log opens");
        if log.opened_without_parse_for_test() {
            folded += 1;
        } else if log.diagnostics().is_empty() && !log.is_converted() {
            refused += 1;
            println!("refused by a fold: {} records", log.record_count());
        } else {
            damaged += 1;
        }
        windowed += usize::from(log.window_for_test().is_some());
    }
    for (id, _) in &logs {
        let store = store(dir);
        if let Err(error) = store.open_for_write(*id) {
            if from_state == 0 {
                println!("an adoption failed: {error}");
            }
            continue;
        }
        if let Err(error) = store.evict(*id) {
            println!("an eviction failed: {error}");
        }
        drop(store);
        let store = store_again(dir);
        if let Ok(projection) = store.open_for_write(*id) {
            from_state += usize::from(projection.log.opened_from_state_for_test());
        }
    }
    println!(
        "logs: {}, opened a record at a time: {folded}, windowed: {windowed}, damaged or older: {damaged}, refused by a fold: {refused}, reopened from state.json: {from_state}",
        logs.len()
    );
}

fn store_again(dir: &Path) -> std::sync::Arc<SessionStore> {
    store(dir)
}
