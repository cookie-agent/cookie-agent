//! Each session's own composer draft: what the user typed and attached for
//! a session stays with it when they look at another and comes back when
//! they return. The new-session draft has its own. Drafts live in this
//! process only: nothing writes them to disk or the daemon, and they end
//! with it.

use std::collections::HashMap;

use cookie_agent_protocol::{ArtifactRef, SessionId};

use super::*;
use crate::ui::input::InputState;

/// Whose draft the composer holds.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ComposerSlot {
    Session(SessionId),
    /// The draft of a session still to be created (`cookie`'s first
    /// screen, `/new`).
    NewSession,
}

/// A draft the composer put aside.
struct ParkedDraft {
    input: InputState,
    attachments: Vec<ArtifactRef>,
}

/// The drafts put aside, and whose draft the composer holds now.
#[derive(Default)]
pub(super) struct ComposerDrafts {
    holder: Option<ComposerSlot>,
    parked: HashMap<ComposerSlot, ParkedDraft>,
}

impl App {
    /// Whose draft the composer should hold: the new-session draft's while
    /// there is one, else the viewed session's.
    fn composer_slot(&self) -> Option<ComposerSlot> {
        if self.new_session_draft.is_some() {
            return Some(ComposerSlot::NewSession);
        }
        self.selected.map(ComposerSlot::Session)
    }

    /// Puts the composer's draft aside for whose it was and takes out the
    /// draft of whose it should be now, after the view changed. The first
    /// holder adopts what the composer has.
    pub(super) fn follow_composer_slot(&mut self) {
        let slot = self.composer_slot();
        if slot == self.composer_drafts.holder {
            return;
        }
        let Some(holder) = std::mem::replace(&mut self.composer_drafts.holder, slot) else {
            return;
        };
        let draft = ParkedDraft {
            input: std::mem::take(&mut self.input),
            attachments: std::mem::take(&mut self.attachments),
        };
        if !draft.input.as_str().is_empty() || !draft.attachments.is_empty() {
            self.composer_drafts.parked.insert(holder, draft);
        }
        // An offer to attach a pasted path, and a selection in the
        // composer, belong to the draft that was showing.
        self.attach_offer = None;
        if matches!(self.selection, Some(TextSelection::Composer { .. })) {
            self.selection = None;
        }
        if let Some(draft) = slot.and_then(|slot| self.composer_drafts.parked.remove(&slot)) {
            self.input = draft.input;
            self.attachments = draft.attachments;
        }
    }

    /// Forgets the new-session draft put aside as the session it started
    /// was created and shown: it is sent as that session's first message.
    pub(super) fn drop_new_session_draft(&mut self) {
        self.composer_drafts
            .parked
            .remove(&ComposerSlot::NewSession);
    }

    /// Forgets the draft of a session that is gone.
    pub(super) fn forget_composer_draft(&mut self, session_id: SessionId) {
        self.composer_drafts
            .parked
            .remove(&ComposerSlot::Session(session_id));
    }
}
