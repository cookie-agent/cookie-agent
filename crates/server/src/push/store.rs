//! `<data_root>/push/`: the daemon's VAPID key (`vapid.pk8`) and the
//! browsers it pushes to (`subscriptions.json`), both owner-only files
//! written through a flushed temporary file renamed over them (push
//! notifications spec section 6.4).

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub(crate) const PUSH_DIR: &str = "push";
const KEY_FILE: &str = "vapid.pk8";
const SUBSCRIPTIONS_FILE: &str = "subscriptions.json";
/// Left by a daemon going down to restart, for the one that comes back.
const RESTARTING_FILE: &str = "restarting";

/// A browser the daemon pushes to, with what it needs to reach it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredSubscription {
    pub(crate) id: String,
    pub(crate) endpoint: String,
    pub(crate) p256dh: String,
    pub(crate) auth: String,
    pub(crate) label: String,
    pub(crate) created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_sent_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SubscriptionsFile {
    subscriptions: Vec<StoredSubscription>,
}

/// The `push` folder under the data root.
pub(crate) struct PushStore {
    directory: PathBuf,
}

impl PushStore {
    pub(crate) fn new(data_root: &Path) -> Self {
        Self {
            directory: data_root.join(PUSH_DIR),
        }
    }

    /// The stored PKCS#8 key, `None` when there is none yet.
    pub(crate) fn read_key(&self) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.directory.join(KEY_FILE)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn write_key(&self, pkcs8: &[u8]) -> io::Result<()> {
        self.write(KEY_FILE, pkcs8)
    }

    /// Marks that the daemon goes down to restart.
    pub(crate) fn mark_restarting(&self) -> io::Result<()> {
        self.write(
            RESTARTING_FILE,
            jiff::Timestamp::now().to_string().as_bytes(),
        )
    }

    /// Whether the daemon before this one went down to restart, forgetting
    /// it.
    pub(crate) fn take_restarting(&self) -> bool {
        fs::remove_file(self.directory.join(RESTARTING_FILE)).is_ok()
    }

    /// The stored subscriptions; none when the file does not exist. A file
    /// that does not parse, or has a member this build does not know, is
    /// an error.
    pub(crate) fn read_subscriptions(&self) -> io::Result<Vec<StoredSubscription>> {
        let path = self.directory.join(SUBSCRIPTIONS_FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<SubscriptionsFile>(&bytes)
                .map(|file| file.subscriptions)
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: {error}", path.display()),
                    )
                }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn write_subscriptions(
        &self,
        subscriptions: &[StoredSubscription],
    ) -> io::Result<()> {
        let file = SubscriptionsFile {
            subscriptions: subscriptions.to_vec(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file).expect("the subscriptions serialize");
        bytes.push(b'\n');
        self.write(SUBSCRIPTIONS_FILE, &bytes)
    }

    /// Writes `name` through a flushed temporary file, owner-only on Unix,
    /// renamed over it.
    fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        let mut random = [0; 8];
        getrandom::getrandom(&mut random).map_err(io::Error::other)?;
        let temporary = self.directory.join(format!(
            ".{name}.{}.tmp",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let result = (|| {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut output = options.open(&temporary)?;
            io::Write::write_all(&mut output, bytes)?;
            output.sync_all()?;
            drop(output);
            fs::rename(&temporary, self.directory.join(name))?;
            #[cfg(unix)]
            fs::File::open(&self.directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subscription(id: &str) -> StoredSubscription {
        StoredSubscription {
            id: id.into(),
            endpoint: format!("https://push.example/{id}"),
            p256dh: "key".into(),
            auth: "auth".into(),
            label: "Phone".into(),
            created_at: Timestamp::UNIX_EPOCH,
            last_sent_at: None,
            last_error: Some("410".into()),
        }
    }

    #[test]
    fn subscriptions_and_the_key_round_trip_owner_only() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = PushStore::new(root.path());
        assert_eq!(store.read_subscriptions().expect("none yet"), Vec::new());
        assert_eq!(store.read_key().expect("none yet"), None);
        store
            .write_subscriptions(&[subscription("a"), subscription("b")])
            .expect("writes");
        store.write_key(b"pkcs8").expect("writes");
        assert_eq!(
            store.read_subscriptions().expect("reads"),
            vec![subscription("a"), subscription("b")]
        );
        assert_eq!(
            store.read_key().expect("reads").as_deref(),
            Some(&b"pkcs8"[..])
        );
        #[cfg(unix)]
        for name in [KEY_FILE, SUBSCRIPTIONS_FILE] {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.path().join(PUSH_DIR).join(name))
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
    }

    #[test]
    fn a_subscriptions_file_with_an_unknown_member_is_an_error() {
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join(PUSH_DIR)).expect("dir");
        fs::write(
            root.path().join(PUSH_DIR).join(SUBSCRIPTIONS_FILE),
            r#"{"subscriptions":[],"extra":1}"#,
        )
        .expect("write");
        assert!(PushStore::new(root.path()).read_subscriptions().is_err());
    }
}
