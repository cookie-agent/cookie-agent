use std::{collections::BTreeMap, sync::Arc, time::Duration};

use cookie_agent_config::SpeechConfig;
use cookie_agent_models::{ModelManager, provider_store::ProviderStore};
use cookie_agent_protocol::SpeechStatus;
use sha2::Sha256;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::*;
use crate::runtime::{
    EngineOptions,
    tests::support::{
        PanicResistantTempDir, create_private_test_dir, empty_catalog, empty_configuration,
        private_tempdir,
    },
};

/// A WebM recording, as far as sniffing goes.
const WEBM: &[u8] = b"\x1a\x45\xdf\xa3 opus frames";

/// A local service answering every request with `status` and `body`, after
/// `delay`; its base URL.
async fn service(status: u16, body: &'static str, delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut data = Vec::new();
                let mut buffer = [0u8; 8192];
                loop {
                    let Ok(read) = socket.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    data.extend_from_slice(&buffer[..read]);
                    if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&data[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                        if data.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                tokio::time::sleep(delay).await;
                let answer = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(answer.as_bytes()).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}/v1")
}

fn engine(speech: Option<&str>) -> (PanicResistantTempDir, Engine) {
    let directory = private_tempdir();
    create_private_test_dir(&directory.path().join(".cookie-agent"));
    let store = directory.path().join("provider-store");
    create_private_test_dir(&store);
    let manager = Arc::new(
        ModelManager::new(
            BTreeMap::new(),
            empty_catalog(),
            ProviderStore::open(store).unwrap(),
        )
        .unwrap(),
    );
    let mut config = empty_configuration();
    config.runtime.speech = speech.map(|text| toml::from_str::<SpeechConfig>(text).unwrap());
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config,
        model_manager: manager,
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(directory.path()),
    })
    .unwrap();
    (directory, engine)
}

fn backends(urls: &[(&str, &str)]) -> String {
    urls.iter()
        .map(|(name, url)| {
            format!("[[backends]]\nname = \"{name}\"\nendpoint = \"{url}\"\nmodel = \"whisper\"\ntimeout_secs = 5\n")
        })
        .collect()
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

async fn dictate(
    engine: &Engine,
    connection: u64,
    bytes: &[u8],
) -> Result<SpeechTranscribeFinishResult, EngineError> {
    let id = engine.begin_transcription(
        connection,
        bytes.len() as u64,
        "audio/webm;codecs=opus",
        None,
    )?;
    engine.transcription_chunk(connection, &id, 0, bytes)?;
    engine
        .finish_transcription(connection, &id, &digest(bytes))
        .await
}

fn speech_error(result: Result<impl std::fmt::Debug, EngineError>) -> SpeechError {
    match result {
        Err(EngineError::Speech(error)) => error,
        other => panic!("not a speech error: {other:?}"),
    }
}

#[tokio::test]
async fn the_status_shows_voice_inputs_limits_only_while_it_is_configured() {
    let (_directory, off) = engine(None);
    assert_eq!(off.status_board().status().speech, None);
    let (_directory, on) = engine(Some(&format!(
        "max_seconds = 90\nmax_bytes = 0\nrecording = \"wav\"\n{}",
        backends(&[("local", "http://127.0.0.1:9/v1")])
    )));
    assert_eq!(
        on.status_board().status().speech,
        Some(SpeechStatus {
            max_seconds: Some(90),
            max_bytes: None,
            recording: SpeechRecording::Wav,
        })
    );
}

#[tokio::test]
async fn a_failing_backend_hands_the_recording_to_the_next() {
    let broken = service(500, r#"{"error":{"message":"overloaded"}}"#, Duration::ZERO).await;
    let missing = service(404, r#"{"detail":"Not Found"}"#, Duration::ZERO).await;
    let working = service(200, r#"{"text":" add a test "}"#, Duration::ZERO).await;
    let (_directory, engine) = engine(Some(&backends(&[
        ("broken", &broken),
        ("missing", &missing),
        ("working", &working),
    ])));
    let result = dictate(&engine, 1, WEBM).await.unwrap();
    assert_eq!(result.text, "add a test");
    assert_eq!(result.backend, "working");
}

#[tokio::test]
async fn when_every_backend_fails_the_error_names_each() {
    let broken = service(401, r#"{"error":{"message":"bad key"}}"#, Duration::ZERO).await;
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}/v1")
    };
    let (_directory, engine) = engine(Some(&backends(&[("hosted", &broken), ("local", &closed)])));
    let SpeechError::Failed { reason } = speech_error(dictate(&engine, 1, WEBM).await) else {
        panic!("not a failure");
    };
    let parts = reason.split(" · ").collect::<Vec<_>>();
    assert_eq!(parts.len(), 2, "{reason}");
    assert_eq!(parts[0], "hosted: returned 401 (bad key)");
    assert!(parts[1].starts_with("local: unreachable"), "{reason}");
}

#[tokio::test]
async fn cancel_abandons_a_transcription_in_flight() {
    let slow = service(200, r#"{"text":"late"}"#, Duration::from_secs(10)).await;
    let (_directory, engine) = engine(Some(&backends(&[("slow", &slow)])));
    let id = engine
        .begin_transcription(1, WEBM.len() as u64, "audio/webm", None)
        .unwrap();
    engine.transcription_chunk(1, &id, 0, WEBM).unwrap();
    let finishing = {
        let engine = engine.clone();
        let id = id.clone();
        tokio::spawn(async move { engine.finish_transcription(1, &id, &digest(WEBM)).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    engine.cancel_transcription(1, &id).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), finishing)
        .await
        .expect("cancel ends the wait")
        .unwrap();
    assert_eq!(speech_error(result), SpeechError::Cancelled);
}

#[tokio::test]
async fn a_closed_connection_abandons_its_transcriptions() {
    let slow = service(200, r#"{"text":"late"}"#, Duration::from_secs(10)).await;
    let (_directory, engine) = engine(Some(&backends(&[("slow", &slow)])));
    let id = engine
        .begin_transcription(4, WEBM.len() as u64, "audio/webm", None)
        .unwrap();
    engine.transcription_chunk(4, &id, 0, WEBM).unwrap();
    let finishing = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.finish_transcription(4, &id, &digest(WEBM)).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    engine.drop_connection_transcriptions(4);
    let result = tokio::time::timeout(Duration::from_secs(3), finishing)
        .await
        .expect("closing ends the wait")
        .unwrap();
    assert_eq!(speech_error(result), SpeechError::Cancelled);
}

#[tokio::test]
async fn recordings_are_refused_by_their_limits_and_content() {
    let (_directory, off) = engine(None);
    assert_eq!(
        speech_error(off.begin_transcription(1, 10, "audio/webm", None)),
        SpeechError::Unavailable
    );

    let (_directory, engine) = engine(Some(&format!(
        "max_bytes = 100\n{}",
        backends(&[("local", "http://127.0.0.1:9/v1")])
    )));
    assert_eq!(
        speech_error(engine.begin_transcription(1, 101, "audio/webm", None)),
        SpeechError::TooLarge {
            byte_length: 101,
            max_bytes: 100
        }
    );
    assert!(
        engine
            .begin_transcription(1, 100, "audio/webm", None)
            .is_ok()
    );
    assert_eq!(
        speech_error(engine.begin_transcription(1, 10, "video/webm", None)),
        SpeechError::UnsupportedMedia {
            media_type: "video/webm".into()
        }
    );
    // The connection's second recording is its last.
    assert!(engine.begin_transcription(1, 10, "audio/ogg", None).is_ok());
    assert_eq!(
        speech_error(engine.begin_transcription(1, 10, "audio/ogg", None)),
        SpeechError::TooMany {
            max: MAX_TRANSCRIPTIONS_PER_CONNECTION
        }
    );

    // Chunks in order, on the connection that began the recording.
    let id = engine.begin_transcription(2, 6, "audio/ogg", None).unwrap();
    assert_eq!(
        speech_error(engine.transcription_chunk(3, &id, 0, b"OggS")),
        SpeechError::NotFound
    );
    assert_eq!(
        speech_error(engine.transcription_chunk(2, &id, 2, b"OggS")),
        SpeechError::OffsetMismatch {
            offset: 2,
            expected_offset: 0
        }
    );
    engine.transcription_chunk(2, &id, 0, b"OggS").unwrap();
    assert!(matches!(
        speech_error(engine.transcription_chunk(2, &id, 4, b"xyz")),
        SpeechError::SizeMismatch {
            byte_length: 6,
            received: 7
        }
    ));
    assert!(matches!(
        speech_error(engine.finish_transcription(2, &id, &digest(b"OggS")).await),
        SpeechError::SizeMismatch {
            byte_length: 6,
            received: 4
        }
    ));
    engine.transcription_chunk(2, &id, 4, b"..").unwrap();
    assert!(matches!(
        speech_error(engine.finish_transcription(2, &id, &digest(b"other")).await),
        SpeechError::DigestMismatch { .. }
    ));
    // A refused digest ends the recording.
    assert_eq!(
        speech_error(
            engine
                .finish_transcription(2, &id, &digest(b"OggS.."))
                .await
        ),
        SpeechError::NotFound
    );

    // Declared audio whose bytes are not.
    let text = b"plain text, not audio";
    let id = engine
        .begin_transcription(5, text.len() as u64, "audio/webm", None)
        .unwrap();
    engine.transcription_chunk(5, &id, 0, text).unwrap();
    assert_eq!(
        speech_error(engine.finish_transcription(5, &id, &digest(text)).await),
        SpeechError::UnsupportedMedia {
            media_type: "audio/webm".into()
        }
    );
}
