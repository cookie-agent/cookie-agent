use std::time::{Duration, Instant};

use super::support::{frame_rows, test_app};
use crate::ui::terminal_layout_with_tree_rows;

#[tokio::test]
async fn a_status_message_dims_out_and_clears_after_five_seconds() {
    let mut app = test_app().await;
    let shown = Instant::now();
    app.status = "rename failed: no such session".into();
    assert!(!app.expire_status_at(shown));
    assert_eq!(app.status_fade_at(shown + Duration::from_millis(3_900)), 0);
    let early = app.status_fade_at(shown + Duration::from_millis(4_100));
    let late = app.status_fade_at(shown + Duration::from_millis(4_900));
    assert!(0 < early && early < late, "{early} then {late}");
    assert!(!app.expire_status_at(shown + Duration::from_millis(4_900)));
    assert_eq!(app.status, "rename failed: no such session");
    assert!(app.expire_status_at(shown + Duration::from_secs(5)));
    assert!(app.status.is_empty());
    assert!(!app.expire_status_at(shown + Duration::from_secs(6)));
}

#[tokio::test]
async fn a_new_message_starts_its_own_five_seconds() {
    let mut app = test_app().await;
    let first = Instant::now();
    app.status = "first".into();
    assert!(!app.expire_status_at(first));
    let second = first + Duration::from_secs(4);
    app.status = "second".into();
    assert!(!app.expire_status_at(second));
    assert_eq!(app.status_fade_at(second), 0);
    assert!(!app.expire_status_at(first + Duration::from_secs(6)));
    assert!(app.expire_status_at(second + Duration::from_secs(5)));
    // The same message again shows again once it was cleared.
    app.status = "second".into();
    let again = second + Duration::from_secs(7);
    assert!(!app.expire_status_at(again));
    assert_eq!(app.status, "second");
}

#[tokio::test]
async fn the_status_shows_in_the_composers_bottom_border_and_takes_no_row() {
    let mut app = test_app().await;
    let (width, height) = (60, 16);
    let layout = terminal_layout_with_tree_rows(
        ratatui::layout::Rect::new(0, 0, width, height),
        0,
        0,
        false,
        false,
        1,
    );
    // The conversation runs down to the composer.
    assert_eq!(layout.conversation.bottom(), layout.input.y);
    app.status = "no compaction to cancel".into();
    let rows = frame_rows(&mut app, width, height);
    let border = &rows[usize::from(layout.input.bottom() - 1)];
    assert!(border.contains(" no compaction to cancel ─╯"), "{border}");
    assert!(
        rows.iter()
            .filter(|row| row.contains("no compaction"))
            .count()
            == 1,
        "{rows:#?}"
    );
}

#[tokio::test]
async fn reconnecting_shows_until_the_connection_is_back() {
    let mut app = test_app().await;
    app.reconnecting = true;
    app.reconnect_error = Some("connection refused".into());
    let shown = Instant::now();
    assert!(!app.expire_status_at(shown + Duration::from_secs(60)));
    let rows = frame_rows(&mut app, 80, 16);
    assert!(
        rows.iter()
            .any(|row| row.contains("daemon disconnected — reconnecting (connection refused)")),
        "{rows:#?}"
    );
}
