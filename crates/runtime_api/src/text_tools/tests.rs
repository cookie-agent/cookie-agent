use super::*;

fn window(offset: usize, limit: usize) -> LineWindow {
    LineWindow::new(Some(offset), Some(limit)).unwrap()
}

#[test]
fn a_window_is_one_based_and_refuses_zero() {
    assert_eq!(
        LineWindow::new(Some(3), Some(5)).unwrap(),
        LineWindow {
            offset: 3,
            limit: 5
        }
    );
    assert_eq!(
        LineWindow::new(Some(0), None).unwrap_err().message(),
        "offset is 1-based: the first line is offset=1"
    );
    assert_eq!(
        LineWindow::new(None, Some(0)).unwrap_err().message(),
        "limit must be positive"
    );
    let window = window(3, 5);
    assert_eq!((window.first_index(), window.end_index()), (2, 7));
}

#[test]
fn pages_number_lines_from_one() {
    let text = "a\nb\r\nc\nd";
    let first = page_text(text, window(1, 2), MAX_PAGE_BYTES);
    assert_eq!(first.text, "1: a\n2: b\n");
    assert_eq!(first.excerpt, ["a", "b"]);
    assert_eq!((first.start_line, first.total_lines), (1, 4));
    assert_eq!(first.next_offset, Some(3));
    assert_eq!(
        first.continuation().as_deref(),
        Some("[2 more lines; continue with offset=3]")
    );
    let middle = page_text(text, window(2, 2), MAX_PAGE_BYTES);
    assert_eq!(middle.text, "2: b\n3: c\n");
    assert_eq!(
        middle.continuation().as_deref(),
        Some("[1 more line; continue with offset=4]")
    );
    let last = page_text(text, window(4, 2), MAX_PAGE_BYTES);
    assert_eq!(last.text, "4: d\n");
    assert_eq!(last.next_offset, None);
    assert_eq!(last.continuation(), None);
    assert_eq!(last.text_with_continuation(), "4: d\n");
    assert_eq!(
        first.text_with_continuation(),
        "1: a\n2: b\n[2 more lines; continue with offset=3]"
    );
}

#[test]
fn a_page_past_the_last_line_is_an_error() {
    let text = "a\nb\n";
    page_text(text, window(2, 1), MAX_PAGE_BYTES)
        .check_in_range("x")
        .unwrap();
    assert_eq!(
        page_text(text, window(3, 1), MAX_PAGE_BYTES)
            .check_in_range("x")
            .unwrap_err()
            .message(),
        "offset=3 is past the end of x, which has 2 lines"
    );
    // Line 1 of nothing is an empty page.
    let empty = page_text("", window(1, 1), MAX_PAGE_BYTES);
    empty.check_in_range("x").unwrap();
    assert_eq!((empty.text.as_str(), empty.total_lines), ("", 0));
    assert_eq!(
        window(2, 1)
            .check_in_range(1, "entry", "dir")
            .unwrap_err()
            .message(),
        "offset=2 is past the end of dir, which has 1 entry"
    );
    assert_eq!(
        window(1, 2).remaining_marker(5, "entry").as_deref(),
        Some("[3 more entries; continue with offset=3]")
    );
    assert_eq!(window(4, 2).remaining_marker(5, "entry"), None);
}

#[test]
fn long_lines_are_cut_and_the_budget_stops_the_page() {
    let long = "x".repeat(MAX_LINE_BYTES + 10);
    let page = page_text(&format!("{long}\nshort\n"), window(1, 10), MAX_PAGE_BYTES);
    assert_eq!(
        page.text,
        format!(
            "1: {}... [line truncated: 10 more bytes]\n2: short\n",
            "x".repeat(MAX_LINE_BYTES)
        )
    );
    // A budget of two lines stops the page at the third, which it names.
    let page = page_text("aa\nbb\ncc\ndd\n", window(1, 10), 12);
    assert_eq!(page.text, "1: aa\n2: bb\n");
    assert!(page.cut);
    assert_eq!(page.next_offset, Some(3));
    assert_eq!(
        page.continuation().as_deref(),
        Some("[page cut at the result size limit; continue with offset=3]")
    );
}

#[test]
fn chunks_of_any_size_page_alike() {
    let text = "one\ntwo\r\nthree\nfour";
    let whole = page_text(text, window(2, 2), MAX_PAGE_BYTES);
    let mut pager = LinePager::new(window(2, 2), MAX_PAGE_BYTES);
    for byte in text.as_bytes() {
        pager.push(std::slice::from_ref(byte)).unwrap();
    }
    assert_eq!(pager.finish().unwrap(), whole);
}

#[test]
fn text_that_is_not_utf8_fails_unless_lossy() {
    let mut strict = LinePager::new(window(1, 10), MAX_PAGE_BYTES);
    assert!(matches!(
        strict.push(b"ok\n\xff\n"),
        Err(PageError::NotText)
    ));
    let mut lossy = LinePager::new(window(1, 10), MAX_PAGE_BYTES).lossy();
    lossy.push(b"ok\n\xff\n").unwrap();
    assert_eq!(lossy.finish().unwrap().text, "1: ok\n2: \u{fffd}\n");
}

#[test]
fn the_shared_wording() {
    assert_eq!(wrote(true, "a.txt", 12), "Created a.txt (12 bytes).");
    assert_eq!(wrote(false, "a.txt", 1), "Replaced a.txt (1 byte).");
    assert_eq!(edited("a.txt", 1), "Edited a.txt: 1 replacement.");
    assert_eq!(edited("a.txt", 3), "Edited a.txt: 3 replacements.");
    assert_eq!(not_found("a.txt"), "a.txt does not exist");
    assert_eq!(
        occurrence_error("foo", 0, "a.txt", true),
        "`foo` occurs 0 times in a.txt; include more context so it occurs once or set replaceAll"
    );
    assert_eq!(
        occurrence_error("foo", 2, "user://x", false),
        "`foo` occurs 2 times in user://x; include more context so it occurs once"
    );
    let long = "é".repeat(MAX_ECHO_CHARS + 5);
    assert_eq!(echo(&long), format!("{}…", "é".repeat(MAX_ECHO_CHARS)));
    assert_eq!(echo("short"), "short");
}
