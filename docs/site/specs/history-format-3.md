# History format 3: an ACP tool's images stored once

Status: **approved** by the user on 2026-11-06; implemented, not released. See
[section 9](#9-implementation-notes-2026-11-06).

## 1. Problem

An ACP tool result that returns an image stores it twice: as a raw file in the
tree's `blobs/`, which the transcript shows, and as base64 inside
`metadata.acp.raw_output`, a JSON blob the record names in `moved_values`.

Measured on 2026-11-06 on yunhao-labserver's store (761 MB: 344 MB of logs, 417
MB of blobs):

- **Disk.** `raw_output` blobs take 147 MB. 143.8 MB of that is base64, 19% of
  the store. Every byte of it decodes to a raw file already in the same `blobs/`.
- **Shape.** All of it is one shape: 634 whole strings at `[].source.data`.
  All of it re-encodes to the exact same string. None is embedded in a longer
  string or a `data:` URI.
- **Memory.** Records in memory are whole, so opening a session reads this
  base64 back. Opening one 11.5 MB log (27.5 MB of `raw_output`, release build,
  four runs each) moved the daemon's RSS by a median of 153 MB as stored (102 to
  170) and 51 MB with the base64 removed (26 to 85). Why 27.5 MB costs about 100
  MB has not been traced. This session is near the worst case: of the logs
  ranked, three carry over 25 MB of it, and many little or none.

## 2. Who reads the stored copy

Traced on 2026-11-06 at `b339c902e`. Every `raw_output` read in
`engine/src/runtime/acp/` is on the **live** call fields (`runner.rs`: 731,
2373-2381, 2915; `permissions.rs`: 302), before the record is written.

| Reader of a stored result's `metadata` | Sees `acp.raw_output`? | With this change |
|---|---|---|
| Native model context: `model_history.rs` `tool_result_part` and `tool_result_text`, 1796-1832 | **Yes**, as JSON the model reads, if a native model ever assembles an ACP session's log | The model reads a short reference, not base64: an improvement. Whether a session can go from ACP to native at all is **unconfirmed**; section 7 tests it |
| ACP bridge, `runner.rs:2601`, and background results, `background_tasks.rs:711, 949` | No for the bridge (cookie's own tool results). Background not confirmed | Section 7 checks it |
| Clients in the display view, `display_view.rs` `without_metadata` | No: the view leaves metadata out | None |
| `diagnostics.rs:207` | No: reads named top-level keys only | None |
| `compaction.rs:1870` | No: clears metadata | None |
| Clients and plugins reading whole events | Yes, as free-form JSON. In-tree clients read nothing of it | The string changes to a reference. `metadata` has no declared shape |
| Artifact liveness (forks, reverts, cleanup) | Sees `artifact://sha256/<digest>` strings | The reference keeps the raw file live, as the display block already does |
| Native model after conversion, on a session it already talked to | Prefix changes once | One cache miss, and possibly thinking-replay recovery, which exists. Only for ACP-then-native sessions |

## 3. Design

**A string in `raw_output` that is entirely canonical base64 of a file in the
tree's blobs is replaced by `artifact://sha256/<digest>`.**

The rule, for writer and converter alike, for each string at any depth of
`metadata.acp.raw_output`:

1. It is at least 1 KiB and is entirely standard base64.
2. Decoding and encoding it again gives the same string.
3. The SHA-256 of the decoded bytes names a file in the tree's blobs (the
   converter), or a media block of this same result (the writer).

Anything else stays as it is. That makes the change **lossless**: the original
string is exactly the base64 of the named file, and nothing else in the record
changes. The rest of `raw_output` (the 3 MB that is not image data), `raw_input`,
`display`, `output`, and the raw image files all stay.

Dropping `raw_output` outright was considered and rejected: it would lose the
2 MB of agent text in it, and could not be undone.

### 3.1 The writer

`end_call` (`runner.rs`, where `metadata` is built) applies the rule after
`call_content` has retained the images, using the digests in `blocks`. An image
the agent returned only in `raw_output`, which no media block shows, stays base64:
that was none of the 634 measured.

### 3.2 The converter

`v2_to_v3.rs` reads a `tool_call_terminated` whose `moved_values` names
`/metadata/acp` or anything under it. It reads the record whole, applies the rule,
and moves values out again with `blobs::externalize`, as a format-3 writer would.
Every other line is copied unchanged.

Format 2's values are in blob files, and converters so far only wrote blobs, so
the framework needs one change: the convert step gets a way to read a blob, from
what earlier steps moved in memory (a format-1 log chaining through 2) and then
from the tree's folder. `convert_bytes` and `convert_tree` both know the folder.
A record whose blob does not read is copied unchanged: the loader judges it as it
would have, and one bad record does not hold up the store's migration.

### 3.3 Removing the old blobs

The disk saving only arrives once the old JSON blobs go, and today nothing deletes
a blob.

- `convert_tree` writes `blobs.obsolete` in the tree: the digests of the
  `raw_output` blobs the conversion stopped naming, minus any that a converted log
  in the tree still names.
- The old blobs are deleted only when the backups of the old logs go, since a
  backup needs them. A daemon's start deletes both once `layout.json` records
  format 3. A later conversion of a tree another process held at that start keeps
  its backups until the user deletes them, and so keeps these blobs.
- It is a list of what this conversion stopped naming, never a sweep of unnamed
  files: a blob with no name in a log can be an upload in progress.
- Each step can repeat, so a crash leaves at worst files to delete at the next
  start.

## 4. What changes for whom

- **The store.** A daemon's first start converts the store, as format 2's did (about
  19 s on this NVMe then; this one has not been timed, and section 7's copy will
  time it). `view.json`
  names its format, so old caches are ignored and rebuilt.
- **Going back.** A build before this refuses format 3 logs
  (`session_requires_newer_engine`). This is the existing rule for format bumps, and
  the user decided on 2026-10-31 not to guard against older binaries writing into a
  migrated store.
- **Other machines.** labpc and Unicorn convert when their daemons update.
- **Protocols.** No client, plugin, or runtime protocol change, as `metadata` is
  free-form. `HISTORY_FORMAT` is 3.

## 5. Left out, and why

Proposed alongside this on 2026-11-06, then measured against the code:

- **`run_started` repeats** (about 10 MB, 1.3% of the store). Moving `agent` and
  `internal_agents` to blobs would add a file read and a parse per run on every
  load, and the loader inlines them anyway, so no memory is saved.
- **`record_omitted` placeholders** (up to 10 MB, 1.3%). The placeholders exist so
  that the sequence stays contiguous, which the log's validation and line index
  rely on. Collapsing them changes that invariant for little.
- **Compressing cold logs** (up to about 290 MB). The largest remaining saving, but
  how the log is compressed depends on how pages read it, the likely next step for
  memory. Decide them together.
- **Retention of real images** (about 190 MB). They are what the transcript shows.

## 6. Phases

1. **The writer**, and `HISTORY_FORMAT` 3 with its converter and the framework
   change, as one commit: a format-3 log must not hold format-2 data.
2. **Removing old blobs** (3.3).
3. **Golden trees** at `crates/engine/tests/fixtures/history/3/`, and the docs:
   `reference/events.md` and the compatibility spec's notes.

## 7. Checks before any real log is rewritten

Automated:

- The rule: a string that fails each of its three conditions stays; a nested one
  is found; a non-canonical one stays.
- **Round trip.** On every line of a fixture with images: putting the base64 back
  from the named files gives the old record exactly.
- Formats 1 and 2 convert through the chain and read as the snapshot shows.
- A crash at each step of 3.3 leaves a log whose blobs all resolve, and the backups
  restorable until they go.
- A fork, a revert, and artifact cleanup keep the raw image.
- The answer to the unconfirmed row of section 2: an ACP session whose agent becomes
  native, if allowed, sends the reference, and background results read no image.

On a scratch copy of the real store, before it is released:

- Convert the copy. For every log: all events other than those rewritten are byte
  for byte the same, and the rewritten ones round trip as above.
- Every blob any converted log names exists.
- The display view's answers (first window and every older page) hold the same
  events before and after, on the largest sessions.
- The store shrinks by about 144 MB.
- RSS on opening the largest session repeats section 1's result, on at least four
  runs each.

If any of these fail, nothing ships until it is understood.

## 8. Decisions

From the user, 2026-11-06:

1. Replace with a reference rather than dropping `raw_output`.
2. Delete the old blobs when the backups go.
3. Leave out section 5's items.

## 9. Implementation notes (2026-11-06)

- The rule is `blobs::reference_stored_images`, the writer's use of it
  `stored_raw_output` in `runner.rs`, the converter `v2_to_v3.rs`.
- Converting rewrites every line through JSON, as format 2's did, so a log's key
  order can change where nothing else does. Section 7's checks compare JSON values,
  not bytes.
- A store's old blobs go before `sessions/.backup-v2/` does, in `sweep_top_level`, so
  a crash in between does it again at the next start. A backup left beside its log
  keeps them, and a blob a log names again stays.
- No client protocol change: clients since 1.63.0 take any `history_format`, and an
  older one is told format 1.
- Golden trees of format 3 are recorded with this build; the fixtures hold no images,
  so unit tests cover those.
- **A visible change:** a result whose only large part was the base64 no longer
  counts as long in the display view, as the view's cut counts metadata. So its `cut`
  entry goes, and the transcript stops offering to load a whole result with nothing
  more to show. Its event is the same.
- No refusal was found to a session's next run using a native agent after an ACP one,
  as the run's agent decides. So section 2's first row is taken as reachable, and it
  gets the reference. It was not run end to end.

Section 7's checks on a scratch copy of yunhao-labserver's store, 2026-11-06, release
builds, format 2 under the build before this and format 3 under this one:

| Check | Result |
|---|---|
| Conversion at a daemon's first start | 143 trees in 11.2 s, 0 pending; 13 s to ready |
| Records | 236,653 in 232 logs: 235,776 the same JSON, 232 only `history_format`, 645 rewritten, all rebuilt exactly from the files; none unaccounted for |
| Blobs | 634 deleted (143.9 MB), none named by a log; none added; no named digest newly missing |
| Store | 769.7 MB to 623.7 MB, 146 MB saved |
| Left behind | No `blobs.obsolete`, `.bak`, staged file, or `.backup-v2` |
| Display view, 7 largest sessions and those with images | 308 answers, 40,910 events, all the same; 65 answers differ only by 281 `cut` entries gone, each on a rewritten record, and none added |
| RSS on opening the largest session, four runs each | Median jump 128 MB to 61.5 MB (97 to 169, 56 to 93); opened 294.5 MB to 222 MB |

Not checked: an image drawn in a browser on the converted store, as the session's
reduced rows keep them folded. Its display block and file are unchanged.
