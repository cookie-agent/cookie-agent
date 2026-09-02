//! Durable exact tree-grant invalidations committed before mutations execute.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};
#[cfg(test)]
use std::{fs::OpenOptions, io::Write};

use cookie_agent_protocol::{SessionId, Sha256Digest, TreeApprovalGrantId};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::events::{EventLogError, append_jsonl, load_jsonl_shared};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GrantInvalidationRecord {
    seq: u64,
    timestamp: Timestamp,
    root_session_id: SessionId,
    grant_ids: Vec<TreeApprovalGrantId>,
    resource_digests: Vec<Sha256Digest>,
}

#[derive(Debug, Error)]
pub enum GrantJournalError {
    #[error(transparent)]
    Event(#[from] EventLogError),
    #[error("invalid grant invalidation journal at {path}: {message}")]
    Corrupt { path: PathBuf, message: String },
    #[error("grant invalidation journal at {path} is poisoned and must be reopened")]
    Poisoned { path: PathBuf },
}

#[derive(Debug)]
pub struct GrantInvalidationJournal {
    path: PathBuf,
    state: Mutex<GrantJournalState>,
    #[cfg(test)]
    failure: Mutex<Option<TestFailure>>,
}

#[derive(Debug)]
struct GrantJournalState {
    records: Vec<GrantInvalidationRecord>,
    poisoned: bool,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum TestFailure {
    PartialWrite,
    DurableButError,
}

impl GrantInvalidationJournal {
    pub fn open(path: PathBuf) -> Result<Arc<Self>, GrantJournalError> {
        let records = load_jsonl_shared::<GrantInvalidationRecord>(&path)?;
        for (index, record) in records.iter().enumerate() {
            let expected = index as u64 + 1;
            if record.seq != expected {
                return Err(GrantJournalError::Corrupt {
                    path,
                    message: format!(
                        "grant invalidation sequence {} is not contiguous; expected {expected}",
                        record.seq
                    ),
                });
            }
            if record.grant_ids.is_empty() || record.resource_digests.is_empty() {
                return Err(GrantJournalError::Corrupt {
                    path,
                    message: "grant invalidation must contain grants and normalized resources"
                        .into(),
                });
            }
            let unique = record.grant_ids.iter().copied().collect::<HashSet<_>>();
            if unique.len() != record.grant_ids.len() {
                return Err(GrantJournalError::Corrupt {
                    path,
                    message: "grant invalidation contains duplicate grant IDs".into(),
                });
            }
        }
        Ok(Arc::new(Self {
            path,
            state: Mutex::new(GrantJournalState {
                records,
                poisoned: false,
            }),
            #[cfg(test)]
            failure: Mutex::new(None),
        }))
    }

    pub fn invalidate(
        &self,
        root_session_id: SessionId,
        mut grant_ids: Vec<TreeApprovalGrantId>,
        mut resource_digests: Vec<Sha256Digest>,
    ) -> Result<(), GrantJournalError> {
        grant_ids.sort_by_key(ToString::to_string);
        grant_ids.dedup();
        resource_digests.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        resource_digests.dedup();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.poisoned {
            return Err(GrantJournalError::Poisoned {
                path: self.path.clone(),
            });
        }
        if grant_ids.is_empty() {
            return Ok(());
        }
        let seq = state
            .records
            .last()
            .map_or(Some(1), |record| record.seq.checked_add(1))
            .ok_or_else(|| {
                state.poisoned = true;
                GrantJournalError::Poisoned {
                    path: self.path.clone(),
                }
            })?;
        let record = GrantInvalidationRecord {
            seq,
            timestamp: Timestamp::now(),
            root_session_id,
            grant_ids,
            resource_digests,
        };
        if let Err(error) = self.append_record(&record) {
            state.poisoned = true;
            return Err(error);
        }
        state.records.push(record);
        Ok(())
    }

    fn append_record(&self, record: &GrantInvalidationRecord) -> Result<(), GrantJournalError> {
        #[cfg(test)]
        if let Some(failure) = self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            match failure {
                TestFailure::PartialWrite => {
                    let bytes = serde_json::to_vec(record).expect("record serializes");
                    let mut file = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.path)
                        .map_err(|source| EventLogError::Io {
                            path: self.path.clone(),
                            source,
                        })?;
                    file.write_all(&bytes[..bytes.len() / 2])
                        .and_then(|()| file.sync_data())
                        .map_err(|source| EventLogError::Io {
                            path: self.path.clone(),
                            source,
                        })?;
                    return Err(GrantJournalError::Event(EventLogError::Io {
                        path: self.path.clone(),
                        source: std::io::Error::other("injected partial write failure"),
                    }));
                }
                TestFailure::DurableButError => {
                    append_jsonl(&self.path, record)?;
                    return Err(GrantJournalError::Event(EventLogError::Io {
                        path: self.path.clone(),
                        source: std::io::Error::other("injected post-fsync failure"),
                    }));
                }
            }
        }
        append_jsonl(&self.path, record).map_err(Into::into)
    }

    #[must_use]
    pub fn invalidated_ids(&self) -> HashSet<TreeApprovalGrantId> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .iter()
            .flat_map(|record| record.grant_ids.iter().copied())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn inject_failure(&self, failure: TestFailure) {
        *self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(failure);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cookie_agent_protocol::{SessionId, Sha256Digest, TreeApprovalGrantId};
    use uuid::Uuid;

    use super::{GrantInvalidationJournal, TestFailure};

    fn invalidate(journal: &GrantInvalidationJournal, grant: TreeApprovalGrantId) {
        journal
            .invalidate(
                SessionId(Uuid::from_u128(2)),
                vec![grant],
                vec![Sha256Digest::new("33".repeat(32)).expect("digest")],
            )
            .expect("commit invalidation");
    }

    #[test]
    fn invalidation_is_absent_before_commit_and_durable_after_reopen() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("grant-invalidations.jsonl");
        let journal = GrantInvalidationJournal::open(path.clone()).expect("open journal");
        let grant = TreeApprovalGrantId(Uuid::from_u128(1));
        assert!(!journal.invalidated_ids().contains(&grant));

        journal
            .invalidate(
                SessionId(Uuid::from_u128(2)),
                vec![grant],
                vec![Sha256Digest::new("11".repeat(32)).expect("digest")],
            )
            .expect("commit invalidation");
        drop(journal);

        let reopened = GrantInvalidationJournal::open(path).expect("reopen journal");
        assert!(reopened.invalidated_ids().contains(&grant));
    }

    #[test]
    fn journal_rejects_noncontiguous_records() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("grant-invalidations.jsonl");
        let grant = TreeApprovalGrantId(Uuid::from_u128(1));
        let journal = GrantInvalidationJournal::open(path.clone()).expect("open journal");
        journal
            .invalidate(
                SessionId(Uuid::from_u128(2)),
                vec![grant],
                vec![Sha256Digest::new("22".repeat(32)).expect("digest")],
            )
            .expect("commit invalidation");
        drop(journal);

        let source =
            fs::read_to_string(&path)
                .expect("read journal")
                .replacen("\"seq\":1", "\"seq\":2", 1);
        fs::write(&path, source).expect("tamper journal");
        assert!(GrantInvalidationJournal::open(path).is_err());
    }

    #[test]
    fn ambiguous_partial_write_poisons_until_reopen() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("grant-invalidations.jsonl");
        let journal = GrantInvalidationJournal::open(path.clone()).expect("open journal");
        journal.inject_failure(TestFailure::PartialWrite);
        let first = TreeApprovalGrantId(Uuid::from_u128(10));
        assert!(
            journal
                .invalidate(
                    SessionId(Uuid::from_u128(2)),
                    vec![first],
                    vec![Sha256Digest::new("44".repeat(32)).expect("digest")],
                )
                .is_err()
        );
        let second = TreeApprovalGrantId(Uuid::from_u128(11));
        assert!(
            journal
                .invalidate(
                    SessionId(Uuid::from_u128(2)),
                    vec![second],
                    vec![Sha256Digest::new("55".repeat(32)).expect("digest")],
                )
                .is_err()
        );
        drop(journal);

        let before = fs::read(&path).expect("read torn journal");
        let reopened = GrantInvalidationJournal::open(path.clone()).expect("read torn tail");
        assert!(!reopened.invalidated_ids().contains(&first));
        assert!(!reopened.invalidated_ids().contains(&second));
        assert_eq!(fs::read(path).expect("reread torn journal"), before);
    }

    #[test]
    fn durable_but_error_poisons_and_reopen_recovers_committed_record() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("grant-invalidations.jsonl");
        let journal = GrantInvalidationJournal::open(path.clone()).expect("open journal");
        journal.inject_failure(TestFailure::DurableButError);
        let first = TreeApprovalGrantId(Uuid::from_u128(20));
        assert!(
            journal
                .invalidate(
                    SessionId(Uuid::from_u128(2)),
                    vec![first],
                    vec![Sha256Digest::new("66".repeat(32)).expect("digest")],
                )
                .is_err()
        );
        let second = TreeApprovalGrantId(Uuid::from_u128(21));
        assert!(
            journal
                .invalidate(
                    SessionId(Uuid::from_u128(2)),
                    vec![second],
                    vec![Sha256Digest::new("77".repeat(32)).expect("digest")],
                )
                .is_err()
        );
        drop(journal);

        let reopened = GrantInvalidationJournal::open(path).expect("reopen durable record");
        assert!(reopened.invalidated_ids().contains(&first));
        assert!(!reopened.invalidated_ids().contains(&second));
        invalidate(&reopened, second);
    }
}
