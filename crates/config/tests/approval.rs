use std::fs;

use cookie_agent_config::{ApprovalCacheTtl, load_from_user_root};

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

/// `[approval] cache_ttl` takes each TTL the reviewer can ask for, its
/// provider's own configuration, or no cache (approval reviewer spec
/// section 5).
#[test]
fn the_reviewer_cache_ttl_takes_each_documented_value() {
    for (written, expected, minutes) in [
        ("1h", ApprovalCacheTtl::OneHour, Some(60)),
        ("30m", ApprovalCacheTtl::ThirtyMinutes, Some(30)),
        ("12h", ApprovalCacheTtl::TwelveHours, Some(720)),
        ("5m", ApprovalCacheTtl::FiveMinutes, Some(5)),
        ("provider", ApprovalCacheTtl::Provider, None),
        ("off", ApprovalCacheTtl::Off, None),
    ] {
        let directory = root(&format!("[approval]\ncache_ttl = \"{written}\"\n"));
        let loaded = load_from_user_root(Some(directory.path())).expect(written);
        assert_eq!(loaded.runtime.approval.cache_ttl, expected);
        assert_eq!(expected.as_str(), written);
        assert_eq!(expected.minutes(), minutes);
    }
}

#[test]
fn the_reviewer_cache_ttl_is_strict() {
    for invalid in [
        "[approval]\ncache_ttl = \"24h\"\n",
        "[approval]\ncache_ttl = \"1H\"\n",
        "[approval]\ncache_ttl = 60\n",
        "[approval]\ncache_ttl = \"\"\n",
        "[approval]\ncache = \"1h\"\n",
    ] {
        let directory = root(invalid);
        assert!(
            load_from_user_root(Some(directory.path())).is_err(),
            "{invalid}"
        );
    }
    // Setting the TTL keeps the section's other key.
    let directory = root("[approval]\ntimeout_ms = 45000\ncache_ttl = \"off\"\n");
    let loaded = load_from_user_root(Some(directory.path())).expect("both keys");
    assert_eq!(loaded.runtime.approval.timeout_ms, Some(45_000));
    assert_eq!(loaded.runtime.approval.cache_ttl, ApprovalCacheTtl::Off);
}

/// `[approval] timeout_ms`: a positive value is the user's window, and
/// absent, zero, or negative means an approval waits until it is answered
/// (push notifications spec section 4.4).
#[test]
fn the_approval_window_is_positive_or_unbounded() {
    for (written, window) in [
        (
            "[approval]\ntimeout_ms = 1\n",
            Some(std::time::Duration::from_millis(1)),
        ),
        ("[approval]\ntimeout_ms = 0\n", None),
        ("[approval]\ntimeout_ms = -1\n", None),
        ("[approval]\ncache_ttl = \"off\"\n", None),
    ] {
        let directory = root(written);
        let loaded = load_from_user_root(Some(directory.path())).expect(written);
        assert_eq!(loaded.runtime.approval.window(), window, "{written}");
    }
    for invalid in [
        "[approval]\ntimeout_ms = 1.5\n",
        "[approval]\ntimeout_ms = \"never\"\n",
    ] {
        let directory = root(invalid);
        assert!(
            load_from_user_root(Some(directory.path())).is_err(),
            "{invalid}"
        );
    }
}
