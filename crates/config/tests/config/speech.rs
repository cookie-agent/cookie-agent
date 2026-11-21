//! `[speech]`: voice input's transcription backends (voice input spec
//! section 5).

use std::{fs, time::Duration};

use cookie_agent_config::load_from_user_root;

fn root(config: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("tempdir");
    fs::write(directory.path().join("config.toml"), config).expect("config");
    directory
}

fn loads(config: &str) -> bool {
    load_from_user_root(Some(root(config).path())).is_ok()
}

#[test]
fn every_speech_key_is_read() {
    let directory = root(
        r#"
[speech]
language = "en"
prompt = "UniEnv, UCMA"
max_seconds = 120
max_bytes = 1000000

[[speech.backends]]
provider = "openai"
model = "gpt-4o-mini-transcribe"
timeout_secs = 30

[[speech.backends]]
name = "local-whisper"
endpoint = "http://127.0.0.1:8000/v1"
auth = { method = "bearer-api-key-v1", values = { api_key = "local-key" } }
model = "Systran/faster-whisper-large-v3"
headers = { x-team = "lab" }
"#,
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let speech = loaded.runtime.speech.as_ref().expect("speech");
    assert_eq!(speech.language.as_deref(), Some("en"));
    assert_eq!(speech.prompt.as_deref(), Some("UniEnv, UCMA"));
    assert_eq!(speech.seconds_limit(), Some(120));
    assert_eq!(speech.bytes_limit(), Some(1_000_000));
    let [hosted, local] = speech.backends.as_slice() else {
        panic!("two backends");
    };
    assert_eq!(
        hosted.provider.as_ref().map(ToString::to_string).as_deref(),
        Some("openai")
    );
    assert_eq!(hosted.model, "gpt-4o-mini-transcribe");
    assert_eq!(hosted.timeout(), Some(Duration::from_secs(30)));
    assert_eq!(hosted.label(), "openai");
    assert_eq!(local.label(), "local-whisper");
    assert_eq!(
        local.endpoint.as_ref().map(|url| url.as_str()),
        Some("http://127.0.0.1:8000/v1")
    );
    assert_eq!(
        local.auth.as_ref().map(|auth| auth.method.as_str()),
        Some("bearer-api-key-v1")
    );
    assert_eq!(local.headers.len(), 1);
    assert_eq!(local.timeout(), None);
}

#[test]
fn without_the_table_voice_input_is_off() {
    let directory = root("[share]\nenabled = true\n");
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    assert!(loaded.runtime.speech.is_none());
}

#[test]
fn zero_or_less_means_no_limit() {
    let directory = root(
        "[speech]\nmax_seconds = 0\nmax_bytes = -1\n[[speech.backends]]\nprovider = \"groq\"\nmodel = \"whisper-large-v3-turbo\"\ntimeout_secs = -5\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let speech = loaded.runtime.speech.as_ref().expect("speech");
    assert_eq!(speech.seconds_limit(), None);
    assert_eq!(speech.bytes_limit(), None);
    assert_eq!(speech.backends[0].timeout(), None);
}

#[test]
fn an_endpoint_backend_without_auth_is_the_backends_host() {
    let directory =
        root("[[speech.backends]]\nendpoint = \"http://localhost:9000/v1\"\nmodel = \"whisper\"\n");
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let backend = &loaded.runtime.speech.as_ref().expect("speech").backends[0];
    assert!(backend.auth.is_none());
    assert_eq!(backend.label(), "localhost:9000");
}

#[test]
fn an_endpoint_and_its_key_read_the_environment() {
    // SAFETY: each test runs in its own process under nextest.
    unsafe {
        std::env::set_var("SPEECH_TEST_HOST", "speech.example.com");
        std::env::set_var("SPEECH_TEST_KEY", "sk-from-env");
    }
    let directory = root(
        "[[speech.backends]]\nendpoint = \"https://${env:SPEECH_TEST_HOST}/v1\"\nauth = { method = \"bearer-api-key-v1\", values = { api_key = \"${env:SPEECH_TEST_KEY}\" } }\nmodel = \"whisper-1\"\n",
    );
    let loaded = load_from_user_root(Some(directory.path())).expect("loads");
    let backend = &loaded.runtime.speech.as_ref().expect("speech").backends[0];
    assert_eq!(
        backend.endpoint.as_ref().map(|url| url.as_str()),
        Some("https://speech.example.com/v1")
    );
    // The model name is not a place for the environment.
    assert!(!loads(
        "[[speech.backends]]\nprovider = \"openai\"\nmodel = \"${env:SPEECH_TEST_KEY}\"\n"
    ));
}

#[test]
fn invalid_speech_tables_fail() {
    let backend = "[[speech.backends]]\nprovider = \"openai\"\nmodel = \"whisper-1\"\n";
    assert!(loads(backend));
    let nine = backend.repeat(9);
    let eight = backend.repeat(8);
    assert!(loads(&eight));
    for config in [
        // No backends, or too many.
        "[speech]\nbackends = []\n".to_owned(),
        "[speech]\nlanguage = \"en\"\n".to_owned(),
        nine,
        // Both or neither of `provider` and `endpoint`.
        "[[speech.backends]]\nprovider = \"openai\"\nendpoint = \"https://api.example.com/v1\"\nmodel = \"m\"\n".to_owned(),
        "[[speech.backends]]\nmodel = \"m\"\n".to_owned(),
        // `auth` belongs to an endpoint.
        "[[speech.backends]]\nprovider = \"openai\"\nmodel = \"m\"\nauth = { method = \"no-auth-v1\", values = {} }\n".to_owned(),
        // An auth method a transcription cannot send, or a missing key.
        "[[speech.backends]]\nendpoint = \"https://api.example.com/v1\"\nmodel = \"m\"\nauth = { method = \"aws-sigv4-credentials-v1\", values = { access_key_id = \"a\", secret_access_key = \"s\" } }\n".to_owned(),
        "[[speech.backends]]\nendpoint = \"https://api.example.com/v1\"\nmodel = \"m\"\nauth = { method = \"bearer-api-key-v1\", values = {} }\n".to_owned(),
        // Plain http only to loopback.
        "[[speech.backends]]\nendpoint = \"http://192.168.1.20:8000/v1\"\nmodel = \"m\"\n".to_owned(),
        // An empty or overlong model, name, language, or prompt.
        "[[speech.backends]]\nprovider = \"openai\"\nmodel = \" \"\n".to_owned(),
        format!("[[speech.backends]]\nprovider = \"openai\"\nmodel = \"{}\"\n", "m".repeat(257)),
        format!("[[speech.backends]]\nprovider = \"openai\"\nmodel = \"m\"\nname = \"{}\"\n", "n".repeat(65)),
        format!("[speech]\nlanguage = \"en us\"\n{backend}"),
        format!("[speech]\nprompt = \"{}\"\n{backend}", "p".repeat(1001)),
        // A header the request owns.
        "[[speech.backends]]\nprovider = \"openai\"\nmodel = \"m\"\nheaders = { content-type = \"text/plain\" }\n".to_owned(),
        // Unknown keys and wrong types.
        format!("[speech]\nunknown = 1\n{backend}"),
        "[[speech.backends]]\nprovider = \"openai\"\nmodel = \"m\"\nextra = true\n".to_owned(),
        format!("[speech]\nmax_seconds = \"5m\"\n{backend}"),
        "[[speech.backends]]\nprovider = \"openai\"\nmodel = 5\n".to_owned(),
        "[speech]\nbackends = { provider = \"openai\", model = \"m\" }\n".to_owned(),
    ] {
        assert!(!loads(&config), "{config}");
    }
}
