#[test]
fn approval_expiry_uses_the_requested_window() {
    let before = jiff::Timestamp::now();
    let expires_at = super::approval_expiry(120_000).expect("expiry");
    let remaining = expires_at.duration_since(before).unsigned_abs();
    assert!(remaining >= std::time::Duration::from_secs(119));
    assert!(remaining <= std::time::Duration::from_secs(121));
}

#[test]
fn approval_expiry_wait_tracks_the_remaining_window() {
    let wait = super::approval_expiry_wait(super::approval_expiry(120_000));
    assert!(wait >= std::time::Duration::from_secs(119));
    assert!(wait <= std::time::Duration::from_secs(121));
}

#[test]
fn escaping_turns_every_open_angle_into_an_entity() {
    assert_eq!(super::escape_untrusted("a < b"), "a &lt; b");
    assert!(matches!(
        super::escape_untrusted("plain"),
        std::borrow::Cow::Borrowed("plain")
    ));
}
