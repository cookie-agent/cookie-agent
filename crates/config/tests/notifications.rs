//! `[notifications]` (push notifications spec section 10).

use std::fs;

use cookie_agent_config::{NotificationPreview, load_from_user_root};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

#[test]
fn every_notifications_key_is_read() {
    let directory = root(
        "[notifications]\nenabled = false\npreview = \"status\"\napprovals = false\ncron_sessions = false\ncontact = \"mailto:me@example.com\"\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let notifications = &loaded.runtime.notifications;
    assert!(!notifications.enabled);
    assert_eq!(notifications.preview, NotificationPreview::Status);
    assert!(!notifications.approvals);
    assert!(!notifications.cron_sessions);
    assert_eq!(
        notifications.contact.as_deref(),
        Some("mailto:me@example.com")
    );

    let directory =
        root("[notifications]\npreview = \"reply\"\ncontact = \"https://example.com/ops\"\n");
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    assert_eq!(
        loaded.runtime.notifications.preview,
        NotificationPreview::Reply
    );
    assert_eq!(
        loaded.runtime.notifications.contact.as_deref(),
        Some("https://example.com/ops")
    );
}

#[test]
fn notifications_are_strict() {
    for invalid in [
        "[notifications]\npreview = \"full\"\n",
        "[notifications]\nenabled = \"yes\"\n",
        "[notifications]\nsound = true\n",
        "[notifications]\ncontact = \"http://example.com\"\n",
        "[notifications]\ncontact = \"https://\"\n",
        "[notifications]\ncontact = \"mailto:nobody\"\n",
        "[notifications]\ncontact = \"me@example.com\"\n",
    ] {
        let directory = root(invalid);
        assert!(
            load_from_user_root(Some(directory.path())).is_err(),
            "{invalid}"
        );
    }
}
