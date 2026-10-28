//! Each session's own composer draft: what the user typed and attached for
//! a session stays with it when they open another and comes back when they
//! return. The new session's draft has its own, its files held in the page
//! until its first message. Drafts live in the page only: nothing writes
//! them to the browser's storage or the daemon, and a reload ends them.

use std::collections::HashMap;

use cookie_agent_protocol::{ArtifactRef, SessionId};
use leptos::prelude::*;

use super::Workspace;

/// Whose draft the composer holds.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum ComposerSlot {
    Session(SessionId),
    /// The draft of the session still to be created.
    NewSession,
}

/// A draft the composer put aside.
#[derive(Default)]
struct ParkedDraft {
    text: String,
    attachments: Vec<ArtifactRef>,
}

/// The drafts put aside, and whose draft the composer holds now.
#[derive(Default)]
pub(super) struct ComposerDrafts {
    holder: Option<ComposerSlot>,
    parked: HashMap<ComposerSlot, ParkedDraft>,
}

/// `text` ahead of `typed`, as a message that comes back to a draft goes.
fn ahead_of(text: String, typed: &mut String) {
    *typed = match (text.is_empty(), typed.is_empty()) {
        (_, true) => text,
        (true, false) => std::mem::take(typed),
        (false, false) => format!("{text}\n\n{typed}"),
    };
}

/// `files` ahead of `typed`, without a file twice.
fn files_ahead_of(files: Vec<ArtifactRef>, typed: &mut Vec<ArtifactRef>) {
    let typed_files = std::mem::replace(typed, files);
    for file in typed_files {
        if !typed.iter().any(|kept| kept.sha256 == file.sha256) {
            typed.push(file);
        }
    }
}

impl Workspace {
    /// Whose draft the composer should hold: the new session's while the
    /// page composes one, else the open session's.
    pub(super) fn composer_slot(self) -> Option<ComposerSlot> {
        if self.new_session.get_untracked() {
            return Some(ComposerSlot::NewSession);
        }
        self.selected.get_untracked().map(ComposerSlot::Session)
    }

    /// Puts the composer's draft aside for whose it was and takes out the
    /// draft of whose it should be now, after the view changed. The first
    /// holder adopts what the composer has.
    pub(super) fn follow_composer_slot(self) {
        let slot = self.composer_slot();
        let Some(previous) = self.composer_drafts.with_value(|drafts| drafts.holder) else {
            self.composer_drafts
                .update_value(|drafts| drafts.holder = slot);
            return;
        };
        if Some(previous) == slot {
            return;
        }
        let draft = ParkedDraft {
            text: self.composer.get_untracked(),
            attachments: self.attachments.get_untracked(),
        };
        let restored = self
            .composer_drafts
            .try_update_value(|drafts| {
                drafts.holder = slot;
                if !draft.text.is_empty() || !draft.attachments.is_empty() {
                    drafts.parked.insert(previous, draft);
                }
                slot.and_then(|slot| drafts.parked.remove(&slot))
            })
            .flatten()
            .unwrap_or_default();
        self.composer.set(restored.text);
        self.attachments.set(restored.attachments);
        self.downscale_offer.set(None);
    }

    /// Whose draft the composer shows: its holder's, else, before any view
    /// change, the view's.
    fn shown_slot(self) -> Option<ComposerSlot> {
        self.composer_drafts
            .with_value(|drafts| drafts.holder)
            .or_else(|| self.composer_slot())
    }

    /// The new session's draft became `session_id`'s, the session its
    /// first message created: what the composer holds now (typed while it
    /// was created) stays.
    pub(super) fn composer_follows_created(self, session_id: SessionId) {
        let composing = self.shown_slot() == Some(ComposerSlot::NewSession);
        if composing {
            self.composer_drafts.update_value(|drafts| {
                drafts.holder = Some(ComposerSlot::Session(session_id));
            });
        } else {
            self.follow_composer_slot();
        }
    }

    /// Puts a message `slot` did not take back into its draft, ahead of
    /// anything typed meanwhile: into the composer while it shows that
    /// draft, else into the draft put aside.
    pub(super) fn return_to_draft(self, slot: ComposerSlot, text: String, files: Vec<ArtifactRef>) {
        if self.shown_slot() == Some(slot) {
            self.composer.update(|composer| ahead_of(text, composer));
            self.attachments
                .update(|attachments| files_ahead_of(files, attachments));
            return;
        }
        self.composer_drafts.update_value(|drafts| {
            let draft = drafts.parked.entry(slot).or_default();
            ahead_of(text, &mut draft.text);
            files_ahead_of(files, &mut draft.attachments);
        });
    }

    /// Adds `file` to the end of `session_id`'s draft, shown or put aside.
    pub(super) fn attach_to_draft(self, session_id: SessionId, file: ArtifactRef) {
        let slot = ComposerSlot::Session(session_id);
        let add = |attachments: &mut Vec<ArtifactRef>| {
            if !attachments
                .iter()
                .any(|attached| attached.sha256 == file.sha256)
            {
                attachments.push(file.clone());
            }
        };
        if self.shown_slot() == Some(slot) {
            self.attachments.update(add);
        } else {
            self.composer_drafts.update_value(|drafts| {
                add(&mut drafts.parked.entry(slot).or_default().attachments)
            });
        }
    }

    /// Forgets the draft of a session that is gone.
    pub(super) fn forget_composer_draft(self, session_id: SessionId) {
        self.composer_drafts.update_value(|drafts| {
            drafts.parked.remove(&ComposerSlot::Session(session_id));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(sha: char) -> ArtifactRef {
        ArtifactRef {
            sha256: sha.to_string().repeat(64),
            byte_length: 1,
            media_type: "text/plain".into(),
            filename: None,
        }
    }

    #[test]
    fn a_returned_message_goes_ahead_of_what_was_typed_meanwhile() {
        let mut typed = String::new();
        ahead_of("sent".into(), &mut typed);
        assert_eq!(typed, "sent");
        ahead_of("again".into(), &mut typed);
        assert_eq!(typed, "again\n\nsent");
        ahead_of(String::new(), &mut typed);
        assert_eq!(typed, "again\n\nsent");
        let mut files = vec![file('b'), file('c')];
        files_ahead_of(vec![file('a'), file('b')], &mut files);
        assert_eq!(files, [file('a'), file('b'), file('c')]);
    }
}
