//! Test-only: an open through [`super::STATE_FILE`] against a parse of the whole
//! file (`docs/site/specs/forever-sessions.md` section 7). Every open that did not
//! read its log whole compares, as the per-append reference fold does, so a
//! snapshot that differs from a parse cannot pass unnoticed in any test.

use super::*;

/// The bytes of `value`, for a comparison naming only `what`, so that a real log's
/// contents stay out of any output.
fn bytes_of<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("what state.json holds serializes")
}

fn assert_same_bytes<T: Serialize>(actual: &T, expected: &T, what: &str) {
    assert!(
        bytes_of(actual) == bytes_of(expected),
        "{what} differs from a parse of the whole file"
    );
}

fn assert_raw_lines_as_parsed(raw: &RawLines, parsed: &RawLines) {
    // A log that let go of its lines keeps those no record reproduces.
    let parsed = parsed
        .lines
        .iter()
        .filter(|line| !raw.partial || parsed.partial || matches!(line.bytes, RawBytes::Kept(_)))
        .collect::<Vec<_>>();
    assert_eq!(raw.lines.len(), parsed.len(), "the raw lines");
    for (line, expected) in raw.lines.iter().zip(parsed) {
        assert_eq!(line.seq, expected.seq, "a raw line's sequence");
        match (&line.bytes, &expected.bytes) {
            (RawBytes::InFile { start, end }, RawBytes::InFile { start: s, end: e }) => {
                assert_eq!((start, end), (s, e), "where a raw line is");
            }
            (RawBytes::Kept(bytes), RawBytes::Kept(kept)) => {
                assert!(bytes == kept, "a kept raw line");
            }
            _ => panic!("a raw line is kept by one read and left in the file by the other"),
        }
    }
}

impl EventLog {
    /// Panics unless this log, which did not read itself whole as it opened, is
    /// what a parse of its file gives.
    pub(in crate::events) fn assert_opened_as_parsed(self: &Arc<Self>) {
        if crate::session::reference_fold_skipped() {
            return;
        }
        let parsed = Self::open_parsed(self.path.clone(), self.session_id, true, None, None)
            .expect("a log that opened parses");
        EVENT_LOG_PARSES.with(|parses| parses.set(parses.get().saturating_sub(1)));
        let whole = parsed.storage().snapshot();
        assert_eq!(self.next_seq(), parsed.next_seq(), "where the log goes on");
        assert_eq!(
            self.diagnostics, parsed.diagnostics,
            "the load's diagnostics"
        );
        assert_eq!(
            self.torn_tail_ignored, parsed.torn_tail_ignored,
            "a torn final record"
        );
        self.validation
            .lock()
            .expect("validation lock poisoned")
            .assert_agrees_with_whole(
                &parsed.validation.lock().expect("validation lock poisoned"),
                "a parse of the whole file",
            );
        assert!(
            self.initial_taint() == parsed.initial_taint(),
            "the load's taint differs from a parse of the whole file"
        );
        {
            let storage = self.storage();
            let expected = parsed.storage();
            assert_eq!(
                storage.lines,
                expected
                    .lines
                    .as_ref()
                    .zip(storage.lines.as_ref())
                    .map(|(parsed, lines)| parsed.let_go_as(lines)),
                "the file's line index"
            );
            assert_eq!(storage.refused, expected.refused, "the records refused");
            assert_eq!(
                storage.physical_model_turns, expected.physical_model_turns,
                "the model turns"
            );
            assert!(storage.creation == expected.creation, "the creation");
            assert_same_bytes(&storage.index, &expected.index, "the index and its folds");
            assert_raw_lines_as_parsed(&storage.raw, &expected.raw);
            if let Some(view_start) = &storage.view_start
                && let Some(placed) = view_start.start()
            {
                let at = whole
                    .iter()
                    .position(|event| event.seq == placed.seq)
                    .expect("a view cache starts at a record of the log");
                assert_eq!(
                    whole[at].timestamp, placed.timestamp,
                    "where a view cache starts"
                );
                assert_eq!(
                    view_start.header(),
                    cookie_agent_protocol::display_view::state_events(&whole[..at]),
                    "a view cache's header"
                );
            }
        }
        state::reference::assert_matches_scans(&self.state(), &whole);
        let read = self
            .unnoted_pass(..)
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .expect("a log that opened reads whole");
        assert_eq!(read.len(), whole.len(), "the records read");
        assert!(
            read.iter().zip(whole.iter()).all(|(a, b)| a == b),
            "a pass reads otherwise than a parse"
        );
        // A snapshot and its tail fold to what the whole file folds to a record at
        // a time, exactly: what each let go of is the same.
        // But for a fold that turned native after an ACP agent's window let go of
        // what its model's context needs: it reads itself whole for that model, as
        // the log that wrote the file did, and holds and validates differently
        // until then (`docs/site/specs/forever-sessions.md` section 12).
        let folded = self
            .opened_from_state
            .then(|| fold_file(&self.path, self.session_id, TornTail::Ignore))
            .flatten();
        if folded.is_some() {
            EVENT_LOG_PARSES.with(|parses| parses.set(parses.get().saturating_sub(1)));
        }
        if let Some(folded) = folded.filter(|folded| !folded.storage.reads_whole_for_its_model()) {
            assert!(
                *self.validation.lock().expect("validation lock poisoned") == folded.validation,
                "the validation state differs from a fold of the whole file"
            );
            let storage = self.storage();
            assert_eq!(
                storage.lines, folded.storage.lines,
                "the line index of a fold"
            );
            assert_same_bytes(&storage.index, &folded.storage.index, "a fold's index");
            assert_raw_lines_as_parsed(&storage.raw, &folded.storage.raw);
            assert_eq!(storage.window, folded.storage.window, "a fold's window");
        }
        let opened = self.opened.lock().expect("opened lock poisoned").clone();
        if let Some((tip, folds)) = opened {
            assert_eq!(tip, parsed.physical_tip_seq(), "the tip folded to");
            crate::session::assert_opened_folds_as_parsed(self, folds, &parsed);
        }
    }

    fn next_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Acquire)
    }

    /// Whether the log opened from its [`STATE_FILE`].
    #[must_use]
    pub fn opened_from_state_for_test(&self) -> bool {
        self.opened_from_state
    }

    /// Opens the log at `path` by reading it whole, as a reference for a test.
    pub fn parsed_read_only(
        path: PathBuf,
        session_id: SessionId,
    ) -> Result<Arc<Self>, EventLogError> {
        Self::open_parsed(path, session_id, true, None, None)
    }
}
