//! Where a client's revert and fork cut a session's history: before a
//! user message, at the end of a reply, and before a compaction. The
//! daemon cuts at any sequence (`session.revert`, `session.fork`); these
//! are the points the transcript offers.
//!
//! A cut before a user message keeps the reply before it: the daemon
//! leaves out the run the message starts along with it. So reverting or
//! forking at a user message keeps what a cut at the end of the reply
//! before it keeps, apart from the settings changed in between, and the
//! message's text goes back to a composer.

use cookie_agent_protocol::ContextCheckpointCommit;

use crate::{SessionState, TranscriptItem};

/// A point of history a client's menu reverts or forks at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CutPoint {
    /// A past user message: a revert or fork cuts to just before it, and
    /// its text goes back to a composer (for a fork, the fork's).
    UserMessage {
        /// The sequence of its `user_input_submitted`.
        seq: u64,
        text: String,
    },
    /// The end of a reply: a revert or fork keeps it and drops what came
    /// after ([`SessionState::reply_cut`]). `at_tip` when nothing came
    /// after, so there is nothing to revert.
    ReplyEnd { through_seq: u64, at_tip: bool },
    /// A compaction: a revert through `through_seq`
    /// ([`before_compaction`]) drops it.
    Compaction { through_seq: u64 },
}

impl CutPoint {
    /// The sequence a revert or fork at the point cuts through.
    #[must_use]
    pub fn through_seq(&self) -> u64 {
        match self {
            Self::UserMessage { seq, .. } => before_user_message(*seq),
            Self::ReplyEnd { through_seq, .. } => *through_seq,
            Self::Compaction { through_seq } => *through_seq,
        }
    }

    /// The user message's text, which a revert or fork gives back.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::UserMessage { text, .. } => Some(text),
            Self::ReplyEnd { .. } | Self::Compaction { .. } => None,
        }
    }

    /// Whether a revert at the point would remove anything.
    #[must_use]
    pub fn revertible(&self) -> bool {
        !matches!(self, Self::ReplyEnd { at_tip: true, .. })
    }

    /// Whether the menu offers a fork at the point: a compaction's offers
    /// only the revert.
    #[must_use]
    pub fn forkable(&self) -> bool {
        !matches!(self, Self::Compaction { .. })
    }
}

/// The sequence a revert or fork to just before the user message whose
/// `user_input_submitted` is `seq` cuts through. A user message always
/// follows `session_created` (sequence 1).
#[must_use]
pub fn before_user_message(seq: u64) -> u64 {
    seq.saturating_sub(1).max(1)
}

/// The sequence a revert to before the compaction `commit`, committed at
/// `seq`, cuts through: the end of the input it read, so the summary goes
/// with what made it, and the context it replaced comes back.
#[must_use]
pub fn before_compaction(seq: u64, commit: &ContextCheckpointCommit) -> u64 {
    commit
        .boundaries
        .input_through_seq
        .min(seq.saturating_sub(1))
        .max(1)
}

impl SessionState {
    /// The [`CutPoint::Compaction`] of the compaction row committed at
    /// `seq`.
    #[must_use]
    pub fn compaction_point(&self, seq: u64) -> Option<CutPoint> {
        self.transcript.iter().rev().find_map(|item| match item {
            TranscriptItem::Compaction {
                seq: at, commit, ..
            } if *at == seq => Some(CutPoint::Compaction {
                through_seq: before_compaction(seq, commit),
            }),
            _ => None,
        })
    }

    /// The [`CutPoint::ReplyEnd`] of the reply that ends with the
    /// assistant block `item_id`, by [`Self::reply_cut`].
    #[must_use]
    pub fn reply_point(&self, item_id: u64) -> Option<CutPoint> {
        self.reply_cut(item_id)
            .map(|through_seq| CutPoint::ReplyEnd {
                through_seq,
                at_tip: through_seq >= self.last_seq,
            })
    }

    /// The sequence a revert or fork that keeps the reply ending with the
    /// assistant block `item_id` cuts through: just before what came next
    /// ([`Self::reply_boundaries`]), else the session's last event, so the
    /// settings changed since stay. `None` unless the block is the last of its
    /// run and the run finished.
    #[must_use]
    pub fn reply_cut(&self, item_id: u64) -> Option<u64> {
        let run = *self.assistant_runs.get(&item_id)?;
        let finished = *self.run_finishes.get(&run)?;
        if self.run_last_blocks.get(&run) != Some(&item_id) {
            return None;
        }
        Some(
            self.reply_boundaries
                .range(finished + 1..)
                .next()
                .map_or(self.last_seq, |next| next - 1)
                .max(finished),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_message_cuts_just_before_it() {
        let point = CutPoint::UserMessage {
            seq: 8,
            text: "again".into(),
        };
        assert_eq!(point.through_seq(), 7);
        assert_eq!(point.text(), Some("again"));
        assert!(point.revertible() && point.forkable());
        assert_eq!(before_user_message(1), 1);
        let tip = CutPoint::ReplyEnd {
            through_seq: 9,
            at_tip: true,
        };
        assert!(!tip.revertible() && tip.forkable());
        assert!(!CutPoint::Compaction { through_seq: 3 }.forkable());
    }
}
