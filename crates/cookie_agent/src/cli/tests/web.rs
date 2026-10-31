//! `cookie web`.

use super::*;

#[test]
fn cookie_web_prints_the_page_with_the_token_in_its_fragment() {
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "web",
            "--open",
            "--url",
            "wss://lab.example/cookie/ws"
        ])
        .unwrap()
        .command,
        Some(Command::Web {
            url: Some("wss://lab.example/cookie/ws".into()),
            token: None,
            open: true,
        })
    );
    assert_eq!(
        web_link("ws://127.0.0.1:7419/ws", TEST_TOKEN)
            .unwrap()
            .as_str(),
        format!("http://127.0.0.1:7419/#token={TEST_TOKEN}")
    );
    assert_eq!(
        web_link("wss://lab.example/cookie/ws", TEST_TOKEN)
            .unwrap()
            .as_str(),
        format!("https://lab.example/cookie/#token={TEST_TOKEN}")
    );
    assert!(web_link("ws://127.0.0.1:7419/runtime", TEST_TOKEN).is_err());
}
