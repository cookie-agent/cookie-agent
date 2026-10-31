//! User media: uploads, artifact reads, leases, admission, and what model
//! history makes of media a model does not accept.

use std::time::{Duration, Instant};

use cookie_agent_protocol::{
    ArtifactRef, ClientRunId, EventOrigin, EventPayload, InputDelivery, Open, RunSelection,
    RunStartParams, SessionId, SessionSubmitInputParams, SessionSubmitInputResult, UserContent,
    UserContentPart,
};
use sha2::{Digest as _, Sha256};

use crate::{EngineError, MediaError};

use super::support::*;
use crate::runtime::artifacts::uploads::UPLOAD_LEASE;

const IMAGE_CAPABILITIES: &str = "input = [\"text\", \"image\"]\noutput = [\"text\"]\ncontext_tokens = 4096\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = false\nnative_replay = \"unsupported\"\nmedia = { image = { mime_types = [\"image/png\"], max_bytes = 20971520, max_count = 1 } }";
const TEXT_CAPABILITIES: &str = "input = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 4096\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = false\nnative_replay = \"unsupported\"\nmedia = {}";
const PRIMARY: &str = "---\ndescription: Media test agent\nmode: primary\nenabled: true\nmodels:\n  - { model: \"custom.test/group/model\", variant: base }\n  - { model: \"custom.test/group/fallback\", variant: base }\npermissions: {}\n---\nLook at what the user attaches.\n";
const CONNECTION: u64 = 7;

fn media_fixture(endpoint: &str, capabilities: &str) -> (Fixture, RunSelection) {
    custom_fixture_with_capabilities(
        endpoint,
        PRIMARY,
        None,
        None,
        false,
        None,
        None,
        4_096,
        None,
        "anthropic-compatible",
        Some(capabilities),
    )
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xedb8_8320
            };
        }
    }
    !crc
}

fn png_chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    let mut checked = kind.to_vec();
    checked.extend_from_slice(data);
    bytes.extend_from_slice(&checked);
    bytes.extend_from_slice(&crc32(&checked).to_be_bytes());
}

/// A structurally complete PNG whose image data is `data_bytes` long; the
/// daemon never decodes pixels.
pub(super) fn png(width: u32, height: u32, data_bytes: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = width.to_be_bytes().to_vec();
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(&mut bytes, b"IHDR", &header);
    let data = (0..data_bytes)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    png_chunk(&mut bytes, b"IDAT", &data);
    png_chunk(&mut bytes, b"IEND", b"");
    bytes
}

fn gif(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = b"GIF89a".to_vec();
    bytes.extend_from_slice(&width.to_le_bytes());
    bytes.extend_from_slice(&height.to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0x2c, 0, 0, 0, 0]);
    bytes.extend_from_slice(&width.to_le_bytes());
    bytes.extend_from_slice(&height.to_le_bytes());
    bytes.extend_from_slice(&[0, 2, 1, 0, 0, 0x3b]);
    bytes
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Uploads `bytes` in chunks of at most 1 MiB as `filename`.
async fn upload(
    fixture: &Fixture,
    session: SessionId,
    bytes: &[u8],
    filename: &str,
) -> cookie_agent_protocol::ArtifactUploadFinishResult {
    let id = fixture
        .engine
        .begin_upload(
            CONNECTION,
            session,
            bytes.len() as u64,
            Some(filename.into()),
        )
        .await
        .expect("begin upload");
    for (index, chunk) in bytes.chunks(1024 * 1024).enumerate() {
        fixture
            .engine
            .upload_chunk(
                CONNECTION,
                &id,
                (index * 1024 * 1024) as u64,
                chunk.to_vec(),
            )
            .await
            .expect("upload chunk");
    }
    fixture
        .engine
        .finish_upload(CONNECTION, &id, &digest(bytes))
        .await
        .expect("finish upload")
}

/// Uploads `bytes` as a `file` upload: any file, which a model sees only as
/// its label.
async fn upload_file(
    fixture: &Fixture,
    session: SessionId,
    bytes: &[u8],
    filename: &str,
) -> cookie_agent_protocol::ArtifactUploadFinishResult {
    let id = fixture
        .engine
        .begin_upload_of_kind(
            CONNECTION,
            session,
            bytes.len() as u64,
            Some(filename.into()),
            cookie_agent_protocol::UploadKind::File,
        )
        .await
        .expect("begin upload");
    fixture
        .engine
        .upload_chunk(CONNECTION, &id, 0, bytes.to_vec())
        .await
        .expect("upload chunk");
    fixture
        .engine
        .finish_upload(CONNECTION, &id, &digest(bytes))
        .await
        .expect("finish upload")
}

fn media(error: EngineError) -> MediaError {
    match error {
        EngineError::Media(error) => error,
        other => panic!("expected a media error, got {other:?}"),
    }
}

fn with_file(text: &str, artifact: &ArtifactRef) -> UserContent {
    UserContent::new(vec![
        Open::Known(UserContentPart::Text { text: text.into() }),
        Open::Known(UserContentPart::File {
            artifact: artifact.clone(),
        }),
    ])
    .unwrap()
}

fn origin() -> EventOrigin {
    EventOrigin::new("client:test").unwrap()
}

fn start(
    session: SessionId,
    selection: &RunSelection,
    key: &str,
    content: UserContent,
) -> RunStartParams {
    RunStartParams {
        reset_fallback: false,
        session_id: session,
        client_run_id: ClientRunId::new(key).unwrap(),
        selection: selection.clone(),
        content,
    }
}

fn upload_temporaries(fixture: &Fixture) -> Vec<String> {
    walk(fixture._directory.path())
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            name.starts_with(".upload-").then_some(name)
        })
        .collect()
}

fn walk(directory: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[tokio::test]
async fn uploads_arrive_in_chunks_and_are_stored_as_their_content_says() {
    let (fixture, selection) = media_fixture("http://127.0.0.1:9/v1", IMAGE_CAPABILITIES);
    let session = fixture.engine.create_session(selection).unwrap();
    let bytes = png(640, 480, 2 * 1024 * 1024 + 4096);
    // The name says JPEG; the content says PNG, and the content wins.
    let finished = upload(&fixture, session.session_id, &bytes, "photo.jpg").await;
    assert_eq!(
        finished.artifact,
        ArtifactRef {
            sha256: digest(&bytes),
            byte_length: bytes.len() as u64,
            media_type: "image/png".into(),
            filename: Some("photo.jpg".into()),
        }
    );
    let image = finished.image.expect("an image's size");
    assert_eq!((image.width, image.height), (640, 480));
    assert!(
        fixture
            .engine
            .inner
            .artifacts
            .blob_path(session.session_id, &finished.artifact.sha256)
            .is_file()
    );
    assert!(upload_temporaries(&fixture).is_empty());

    // Ranges read back exactly, the last one short.
    let sha256 = &finished.artifact.sha256;
    let total = bytes.len() as u64;
    let (first, length, media_type) = fixture
        .engine
        .read_artifact_range(session.session_id, sha256, 0, 1024 * 1024)
        .await
        .unwrap();
    assert_eq!((length, media_type.as_str()), (total, "image/png"));
    assert_eq!(first, bytes[..1024 * 1024]);
    let tail_from = total - 100;
    let (tail, _, _) = fixture
        .engine
        .read_artifact_range(session.session_id, sha256, tail_from, 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(tail, bytes[usize::try_from(tail_from).unwrap()..]);
    let (end, _, _) = fixture
        .engine
        .read_artifact_range(session.session_id, sha256, total, 10)
        .await
        .unwrap();
    assert!(end.is_empty());
    assert!(matches!(
        media(
            fixture
                .engine
                .read_artifact_range(session.session_id, sha256, total + 1, 10)
                .await
                .unwrap_err()
        ),
        MediaError::InvalidRequest(_)
    ));
    assert!(matches!(
        media(
            fixture
                .engine
                .read_artifact_range(session.session_id, sha256, 0, 1024 * 1024 + 1)
                .await
                .unwrap_err()
        ),
        MediaError::InvalidRequest(_)
    ));
    assert!(matches!(
        media(
            fixture
                .engine
                .read_artifact_range(session.session_id, &"0".repeat(64), 0, 10)
                .await
                .unwrap_err()
        ),
        MediaError::ArtifactNotFound { .. }
    ));
    // A tool's text output reads back as text.
    let (_, text_digest) = fixture
        .engine
        .inner
        .artifacts
        .retain(session.session_id, b"line one\nline two\n")
        .unwrap();
    let (text, _, media_type) = fixture
        .engine
        .read_artifact_range(session.session_id, &text_digest, 5, 3)
        .await
        .unwrap();
    assert_eq!(
        (text.as_slice(), media_type.as_str()),
        (&b"one"[..], "text/plain")
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn chunks_must_arrive_in_order_and_fill_the_upload_exactly() {
    let (fixture, selection) = media_fixture("http://127.0.0.1:9/v1", IMAGE_CAPABILITIES);
    let session = fixture.engine.create_session(selection).unwrap();
    let bytes = png(4, 4, 64);
    let engine = &fixture.engine;
    let id = engine
        .begin_upload(CONNECTION, session.session_id, bytes.len() as u64, None)
        .await
        .unwrap();
    let (head, rest) = bytes.split_at(10);
    // A gap is refused, and the upload keeps waiting for the right chunk.
    assert_eq!(
        media(
            engine
                .upload_chunk(CONNECTION, &id, 5, head.to_vec())
                .await
                .unwrap_err()
        ),
        MediaError::OffsetMismatch {
            offset: 5,
            expected_offset: 0
        }
    );
    engine
        .upload_chunk(CONNECTION, &id, 0, head.to_vec())
        .await
        .unwrap();
    // A repeated chunk is out of order too.
    assert_eq!(
        media(
            engine
                .upload_chunk(CONNECTION, &id, 0, head.to_vec())
                .await
                .unwrap_err()
        ),
        MediaError::OffsetMismatch {
            offset: 0,
            expected_offset: 10
        }
    );
    // Finishing early is refused and leaves the upload in progress.
    assert_eq!(
        media(
            engine
                .finish_upload(CONNECTION, &id, &digest(&bytes))
                .await
                .unwrap_err()
        ),
        MediaError::SizeMismatch {
            byte_length: bytes.len() as u64,
            received: 10
        }
    );
    // A chunk past the declared size is refused.
    let mut longer = rest.to_vec();
    longer.push(0);
    assert_eq!(
        media(
            engine
                .upload_chunk(CONNECTION, &id, 10, longer)
                .await
                .unwrap_err()
        ),
        MediaError::SizeMismatch {
            byte_length: bytes.len() as u64,
            received: bytes.len() as u64 + 1
        }
    );
    engine
        .upload_chunk(CONNECTION, &id, 10, rest.to_vec())
        .await
        .unwrap();
    engine
        .finish_upload(CONNECTION, &id, &digest(&bytes))
        .await
        .unwrap();
    // A finished upload is no longer in progress.
    assert_eq!(
        media(
            engine
                .finish_upload(CONNECTION, &id, &digest(&bytes))
                .await
                .unwrap_err()
        ),
        MediaError::UploadNotFound
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_digest_mismatch_or_refused_content_drops_the_upload() {
    let (fixture, selection) = media_fixture("http://127.0.0.1:9/v1", IMAGE_CAPABILITIES);
    let session = fixture.engine.create_session(selection).unwrap();
    let engine = &fixture.engine;
    let send = |bytes: Vec<u8>| async move {
        let id = engine
            .begin_upload(CONNECTION, session.session_id, bytes.len() as u64, None)
            .await
            .unwrap();
        engine
            .upload_chunk(CONNECTION, &id, 0, bytes)
            .await
            .unwrap();
        id
    };

    let bytes = png(4, 4, 64);
    let id = send(bytes.clone()).await;
    let MediaError::DigestMismatch { actual } = media(
        engine
            .finish_upload(CONNECTION, &id, &"0".repeat(64))
            .await
            .unwrap_err(),
    ) else {
        panic!("a digest mismatch");
    };
    assert_eq!(actual, digest(&bytes));
    assert_eq!(
        media(
            engine
                .finish_upload(CONNECTION, &id, &digest(&bytes))
                .await
                .unwrap_err()
        ),
        MediaError::UploadNotFound
    );

    // Bytes that are neither a media file nor text.
    let binary = b"\x00\x01\xfe not media".to_vec();
    let id = send(binary.clone()).await;
    assert!(matches!(
        media(engine.finish_upload(CONNECTION, &id, &digest(&binary)).await.unwrap_err()),
        MediaError::TypeUnsupported { media_type, .. } if media_type == "application/octet-stream"
    ));
    let mut torn = png(4, 4, 64);
    torn.truncate(torn.len() - 4);
    let id = send(torn.clone()).await;
    assert!(matches!(
        media(engine.finish_upload(CONNECTION, &id, &digest(&torn)).await.unwrap_err()),
        MediaError::TypeUnsupported { media_type, .. } if media_type == "image/png"
    ));
    let wide = png(20_000, 10, 16);
    let id = send(wide.clone()).await;
    assert_eq!(
        media(
            engine
                .finish_upload(CONNECTION, &id, &digest(&wide))
                .await
                .unwrap_err()
        ),
        MediaError::TooLarge {
            kind: "image".into(),
            byte_length: wide.len() as u64,
            max_bytes: 20 * 1024 * 1024,
            width: Some(20_000),
            height: Some(10),
            max_dimension: Some(16_384),
            max_pixels: Some(16 * 1024 * 1024),
        }
    );
    // Nothing refused was stored, and no temporary file is left behind.
    assert!(upload_temporaries(&fixture).is_empty());
    for refused in [&binary, &torn, &wide] {
        assert!(
            !engine
                .inner
                .artifacts
                .blob_path(session.session_id, &digest(refused))
                .exists()
        );
    }

    // An upload above the largest attachment cap is refused at once.
    assert_eq!(
        media(
            engine
                .begin_upload(CONNECTION, session.session_id, 25 * 1024 * 1024 + 1, None)
                .await
                .unwrap_err()
        ),
        MediaError::TooLarge {
            kind: "file".into(),
            byte_length: 25 * 1024 * 1024 + 1,
            max_bytes: 25 * 1024 * 1024,
            width: None,
            height: None,
            max_dimension: None,
            max_pixels: None,
        }
    );
    assert!(matches!(
        media(
            engine
                .begin_upload(CONNECTION, session.session_id, 10, Some("../x.png".into()))
                .await
                .unwrap_err()
        ),
        MediaError::InvalidRequest(_)
    ));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn uploads_in_progress_belong_to_their_connection_and_end_with_it() {
    let (fixture, selection) = media_fixture("http://127.0.0.1:9/v1", IMAGE_CAPABILITIES);
    let session = fixture.engine.create_session(selection).unwrap();
    let engine = &fixture.engine;
    let mut ids = Vec::new();
    for _ in 0..crate::MAX_UPLOADS_PER_CONNECTION {
        ids.push(
            engine
                .begin_upload(CONNECTION, session.session_id, 100, None)
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        media(
            engine
                .begin_upload(CONNECTION, session.session_id, 100, None)
                .await
                .unwrap_err()
        ),
        MediaError::TooManyUploads {
            max_uploads: crate::MAX_UPLOADS_PER_CONNECTION
        }
    );
    // Another connection has its own allowance, and cannot touch these.
    let other = engine
        .begin_upload(CONNECTION + 1, session.session_id, 100, None)
        .await
        .unwrap();
    assert_eq!(
        media(
            engine
                .upload_chunk(CONNECTION + 1, &ids[0], 0, vec![1])
                .await
                .unwrap_err()
        ),
        MediaError::UploadNotFound
    );
    assert_eq!(
        media(engine.cancel_upload(CONNECTION + 1, &ids[0]).unwrap_err()),
        MediaError::UploadNotFound
    );
    engine
        .upload_chunk(CONNECTION, &ids[0], 0, vec![1, 2, 3])
        .await
        .unwrap();
    assert_eq!(
        upload_temporaries(&fixture).len(),
        crate::MAX_UPLOADS_PER_CONNECTION + 1
    );
    engine.cancel_upload(CONNECTION, &ids[1]).unwrap();
    assert_eq!(
        upload_temporaries(&fixture).len(),
        crate::MAX_UPLOADS_PER_CONNECTION
    );
    // The connection closes: its uploads and their files go.
    engine.drop_connection_uploads(CONNECTION);
    assert_eq!(upload_temporaries(&fixture).len(), 1);
    assert_eq!(
        media(
            engine
                .upload_chunk(CONNECTION, &ids[0], 3, vec![4])
                .await
                .unwrap_err()
        ),
        MediaError::UploadNotFound
    );
    engine
        .upload_chunk(CONNECTION + 1, &other, 0, vec![1])
        .await
        .unwrap();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_lease_sweep_never_takes_an_upload_an_input_holds_or_references() {
    let (fixture, selection) = media_fixture("http://127.0.0.1:9/v1", IMAGE_CAPABILITIES);
    let session = fixture.engine.create_session(selection).unwrap();
    let engine = &fixture.engine;
    let bytes = png(4, 4, 64);
    let id = engine
        .begin_upload(CONNECTION, session.session_id, bytes.len() as u64, None)
        .await
        .unwrap();
    engine
        .upload_chunk(CONNECTION, &id, 0, bytes.clone())
        .await
        .unwrap();
    let uploaded = Instant::now();
    let artifact = engine
        .finish_upload_at(CONNECTION, &id, &digest(&bytes), uploaded)
        .unwrap()
        .artifact;
    let path = engine
        .inner
        .artifacts
        .blob_path(session.session_id, &artifact.sha256);
    let expired = uploaded + UPLOAD_LEASE + Duration::from_secs(1);

    // Within its lease the upload stays.
    engine.sweep_upload_leases(uploaded + UPLOAD_LEASE - Duration::from_secs(1));
    assert!(path.is_file());
    // An input being admitted holds it past its lease: the sweep that would
    // race the submit leaves it alone.
    let pinned = engine
        .pin_input_artifacts(session.session_id, &[&artifact])
        .unwrap();
    engine.sweep_upload_leases(expired);
    assert!(path.is_file(), "a pinned upload survives the sweep");
    // Had the submit failed, the next sweep takes it.
    drop(pinned);
    engine.sweep_upload_leases(expired);
    assert!(
        !path.exists(),
        "an unreferenced upload goes after its lease"
    );
    // And a submit after the sweep finds it gone instead of naming a blob
    // that no longer exists.
    assert!(matches!(
        media(
            engine
                .pin_input_artifacts(session.session_id, &[&artifact])
                .err()
                .unwrap()
        ),
        MediaError::ArtifactNotFound { .. }
    ));

    // An upload an event references outlives its lease.
    let id = engine
        .begin_upload(CONNECTION, session.session_id, bytes.len() as u64, None)
        .await
        .unwrap();
    engine
        .upload_chunk(CONNECTION, &id, 0, bytes.clone())
        .await
        .unwrap();
    let artifact = engine
        .finish_upload_at(CONNECTION, &id, &digest(&bytes), uploaded)
        .unwrap()
        .artifact;
    engine
        .append(
            session.session_id,
            None,
            origin(),
            EventPayload::UserInputQueued {
                content: with_file("later", &artifact),
                selection: session.creation_selection.clone(),
                client_run_id: ClientRunId::new("queued-with-file").unwrap(),
            },
        )
        .await
        .unwrap();
    engine.sweep_upload_leases(expired);
    assert!(path.is_file(), "a referenced upload is durable");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn admission_refuses_only_files_it_cannot_send_as_they_are() {
    for expected in ["count", "missing"] {
        let (fixture, selection) = media_fixture("http://127.0.0.1:9/v1", IMAGE_CAPABILITIES);
        let session = fixture.engine.create_session(selection.clone()).unwrap();
        let image = upload(&fixture, session.session_id, &png(2, 2, 200), "a.png")
            .await
            .artifact;
        let content = match expected {
            "count" => {
                let second = upload(&fixture, session.session_id, &png(3, 3, 200), "b.png")
                    .await
                    .artifact;
                let mut parts = with_file("two", &image).into_parts();
                parts.push(Open::Known(UserContentPart::File { artifact: second }));
                UserContent::new(parts).unwrap()
            }
            _ => with_file(
                "gone",
                &ArtifactRef {
                    sha256: "e".repeat(64),
                    ..image.clone()
                },
            ),
        };
        let error = media(
            fixture
                .engine
                .start_run(
                    start(session.session_id, &selection, expected, content),
                    origin(),
                )
                .await
                .unwrap_err(),
        );
        match expected {
            "count" => assert_eq!(
                error,
                MediaError::LimitExceeded {
                    kind: "image".into(),
                    count: 2,
                    max_count: 1
                }
            ),
            _ => assert!(matches!(error, MediaError::ArtifactNotFound { .. })),
        }
        // Nothing of the run was recorded.
        let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
        assert!(projection.runs.is_empty(), "{expected}");
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn files_the_run_model_cannot_take_are_admitted_and_reach_it_as_labels() {
    let small_image = IMAGE_CAPABILITIES.replace("max_bytes = 20971520", "max_bytes = 100");
    for (capabilities, case, reason) in [
        (
            TEXT_CAPABILITIES.to_owned(),
            "unsupported",
            "does not accept images",
        ),
        (small_image, "too_large", "larger than the 100 B"),
        (
            IMAGE_CAPABILITIES.to_owned(),
            "type",
            "does not accept image/gif",
        ),
        (IMAGE_CAPABILITIES.to_owned(), "file", ""),
    ] {
        let (endpoint, captured, _reached, _release) = scripted_server_with_delayed_response(
            vec![anthropic_usage_body("noted", 1, 0, 0)],
            usize::MAX,
        )
        .await;
        let (fixture, selection) = media_fixture(&endpoint, &capabilities);
        let session = fixture.engine.create_session(selection.clone()).unwrap();
        let artifact = match case {
            "type" => {
                upload(&fixture, session.session_id, &gif(2, 2), "a.gif")
                    .await
                    .artifact
            }
            "file" => {
                upload_file(
                    &fixture,
                    session.session_id,
                    b"PK\x03\x04 an archive",
                    "a.zip",
                )
                .await
                .artifact
            }
            _ => {
                upload(&fixture, session.session_id, &png(2, 2, 200), "a.png")
                    .await
                    .artifact
            }
        };
        fixture
            .engine
            .start_run(
                start(
                    session.session_id,
                    &selection,
                    case,
                    with_file("look", &artifact),
                ),
                origin(),
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
        let requests = with_watchdog("captured requests", captured).await.unwrap();
        let body = request_body(&requests[0]).to_string();
        let name = artifact.filename.clone().unwrap();
        let label = format!("[file {name}, ");
        assert!(body.contains(&label), "{case}: {body}");
        assert!(
            body.contains(&format!("artifact {}", &artifact.sha256[..8])),
            "{case}: {body}"
        );
        assert!(body.contains(reason), "{case}: {body}");
        assert!(!body.contains("\"type\":\"image\""), "{case}: {body}");
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn a_reference_must_say_what_the_stored_content_is() {
    let (fixture, selection) = media_fixture("http://127.0.0.1:9/v1", IMAGE_CAPABILITIES);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    // A tool's text output is in the store, but it is no image.
    let text = b"not an image at all";
    fixture
        .engine
        .inner
        .artifacts
        .retain(session.session_id, text)
        .unwrap();
    let claimed = ArtifactRef {
        sha256: digest(text),
        byte_length: text.len() as u64,
        media_type: "image/png".into(),
        filename: None,
    };
    let error = media(
        fixture
            .engine
            .start_run(
                start(
                    session.session_id,
                    &selection,
                    "claimed",
                    with_file("x", &claimed),
                ),
                origin(),
            )
            .await
            .unwrap_err(),
    );
    // The stored bytes are text, which the reference does not say.
    assert!(
        matches!(error, MediaError::ArtifactNotFound { ref reason, .. } if reason.contains("text/plain, not image/png")),
        "{error:?}"
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn steered_and_queued_input_is_admitted_against_the_model_it_will_reach() {
    let (endpoint, _captured, reached, release) =
        scripted_server_with_delayed_response(vec![anthropic_usage_body("done", 1, 0, 0)], 0).await;
    let (fixture, selection) = media_fixture(&endpoint, IMAGE_CAPABILITIES);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    let image = upload(&fixture, session.session_id, &png(2, 2, 32), "a.png")
        .await
        .artifact;
    let second = upload(&fixture, session.session_id, &png(3, 3, 32), "b.png")
        .await
        .artifact;
    // Two images, where the model takes one a message.
    let mut parts = with_file("and these", &image).into_parts();
    parts.push(Open::Known(UserContentPart::File { artifact: second }));
    let two = UserContent::new(parts).unwrap();
    fixture
        .engine
        .start_run(
            start(session.session_id, &selection, "first", "hello".into()),
            origin(),
        )
        .await
        .unwrap();
    with_watchdog("first request", reached).await.unwrap();
    for delivery in [InputDelivery::Steer, InputDelivery::Queue] {
        let error = media(
            fixture
                .engine
                .submit_input(
                    SessionSubmitInputParams {
                        session_id: session.session_id,
                        content: two.clone(),
                        delivery,
                        selection: None,
                        client_run_id: ClientRunId::new(format!("{delivery:?}")).unwrap(),
                        reset_fallback: false,
                    },
                    origin(),
                )
                .await
                .unwrap_err(),
        );
        assert_eq!(
            error,
            MediaError::LimitExceeded {
                kind: "image".into(),
                count: 2,
                max_count: 1
            },
            "{delivery:?}"
        );
    }
    release.notify_one();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
    assert!(
        !projection.log.event_snapshot().iter().any(|event| matches!(
            event.payload,
            EventPayload::UserInputAdmitted { .. } | EventPayload::UserInputQueued { .. }
        ))
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn an_uploaded_image_reaches_the_model_forks_with_its_session_and_goes_after_a_revert() {
    let (endpoint, captured, _reached, _release) = scripted_server_with_delayed_response(
        vec![anthropic_usage_body("a pixel", 1, 0, 0)],
        usize::MAX,
    )
    .await;
    let (fixture, selection) = media_fixture(&endpoint, IMAGE_CAPABILITIES);
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap();
    let bytes = png(2, 2, 32);
    let id = engine
        .begin_upload(
            CONNECTION,
            session.session_id,
            bytes.len() as u64,
            Some("pixel.png".into()),
        )
        .await
        .unwrap();
    engine
        .upload_chunk(CONNECTION, &id, 0, bytes.clone())
        .await
        .unwrap();
    let uploaded = Instant::now();
    let artifact = engine
        .finish_upload_at(CONNECTION, &id, &digest(&bytes), uploaded)
        .unwrap()
        .artifact;
    let started = engine
        .submit_input(
            SessionSubmitInputParams {
                session_id: session.session_id,
                content: with_file("what is this?", &artifact),
                delivery: InputDelivery::Steer,
                selection: None,
                client_run_id: ClientRunId::new("with-image").unwrap(),
                reset_fallback: false,
            },
            origin(),
        )
        .await
        .unwrap();
    assert!(matches!(started, SessionSubmitInputResult::Started { .. }));
    wait_for_session_not_running(engine, session.session_id).await;
    let requests = with_watchdog("captured request", captured).await.unwrap();
    let body = request_body(&requests[0]);
    let user = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|message| message["role"] == "user")
        .unwrap();
    let parts = user["content"].as_array().unwrap();
    assert!(
        parts
            .iter()
            .any(|part| part["type"] == "text" && part["text"] == "what is this?"),
        "{user}"
    );
    let image = parts
        .iter()
        .find(|part| part["type"] == "image")
        .unwrap_or_else(|| panic!("the request carries the image: {user}"));
    assert_eq!(image["source"]["media_type"], "image/png");
    {
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(image["source"]["data"].as_str().unwrap())
                .unwrap(),
            bytes
        );
    }

    // A fork through the submit takes the blob into its own tree.
    let events = engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .event_snapshot();
    let tip = events.last().unwrap().seq;
    let run_started = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
        .unwrap()
        .seq;
    let forked = engine
        .fork_session(session.session_id, tip, origin(), None)
        .await
        .unwrap()
        .session_id;
    let root_path = engine
        .inner
        .artifacts
        .blob_path(session.session_id, &artifact.sha256);
    let fork_path = engine.inner.artifacts.blob_path(forked, &artifact.sha256);
    assert_ne!(root_path, fork_path);
    assert!(fork_path.is_file(), "the fork's tree has the blob");

    // A revert past the submit leaves the blob to its lease.
    engine
        .revert_session(session.session_id, run_started - 1, origin(), Some(tip))
        .await
        .unwrap();
    assert!(root_path.is_file(), "the lease keeps it for now");
    engine.sweep_upload_leases(uploaded + UPLOAD_LEASE + Duration::from_secs(1));
    assert!(!root_path.exists(), "unreferenced once the lease ends");
    assert!(fork_path.is_file(), "the fork still references its copy");
    fixture.engine.shutdown().await;
}

/// [`media_fixture`] with images on `group/model` and text only on
/// `group/fallback`.
async fn image_then_text_fixture(endpoint: &str) -> (Fixture, RunSelection) {
    use cookie_agent_models::{ModelManager, ProviderDefinition, provider_store::ProviderStore};
    let (mut fixture, selection) = media_fixture(endpoint, IMAGE_CAPABILITIES);
    fixture.engine.shutdown().await;
    let provider_id = cookie_agent_protocol::ProviderId::new("custom.test").unwrap();
    let Some(ProviderDefinition::Custom(provider)) =
        fixture.config.runtime.providers.get_mut(&provider_id)
    else {
        panic!("the custom test provider");
    };
    let fallback = provider
        .models
        .get_mut(&cookie_agent_protocol::ProviderModelId::new("group/fallback").unwrap())
        .unwrap();
    fallback
        .capabilities
        .input
        .remove(&cookie_agent_models::authoring::Modality::Image);
    fallback.capabilities.media.clear();
    let current = fixture.manager.current();
    let manager = std::sync::Arc::new(
        ModelManager::new(
            fixture.config.runtime.providers.clone(),
            std::sync::Arc::clone(current.catalog()),
            ProviderStore::open(fixture._directory.path().join("provider-store")).unwrap(),
        )
        .unwrap(),
    );
    fixture.engine = crate::Engine::open(crate::EngineOptions {
        data_dir: fixture._directory.path().join("data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config.clone(),
        model_manager: std::sync::Arc::clone(&manager),
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(fixture._directory.path()),
    })
    .unwrap();
    fixture.manager = manager;
    (fixture, selection)
}

#[tokio::test]
async fn an_image_in_history_reaches_a_text_only_model_as_its_label() {
    let (endpoint, captured, _reached, _release) = scripted_server_with_delayed_response(
        vec![
            anthropic_usage_body("a pixel", 1, 0, 0),
            anthropic_usage_body("still a pixel", 1, 0, 0),
        ],
        usize::MAX,
    )
    .await;
    let (fixture, selection) = image_then_text_fixture(&endpoint).await;
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap();
    let bytes = png(2, 2, 32);
    let artifact = upload(&fixture, session.session_id, &bytes, "pixel.png")
        .await
        .artifact;
    engine
        .start_run(
            start(
                session.session_id,
                &selection,
                "with-image",
                with_file("what is this?", &artifact),
            ),
            origin(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(engine, session.session_id).await;

    // The next run goes to the text-only model, with the image in history.
    let text_only = RunSelection {
        model: cookie_agent_protocol::ModelSelection {
            model: "custom.test/group/fallback".parse().unwrap(),
            variant: None,
        },
        ..selection.clone()
    };
    engine
        .start_run(
            start(
                session.session_id,
                &text_only,
                "text-only",
                "and now?".into(),
            ),
            origin(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(engine, session.session_id).await;
    assert_eq!(
        engine.get_session(session.session_id).unwrap().status,
        cookie_agent_protocol::SessionStatus::Completed
    );
    let requests = with_watchdog("captured requests", captured).await.unwrap();
    assert_eq!(requests.len(), 2);
    let first = request_body(&requests[0]);
    assert!(first.to_string().contains("\"type\":\"image\""), "{first}");
    let second = request_body(&requests[1]);
    assert!(
        !second.to_string().contains("\"type\":\"image\""),
        "{second}"
    );
    // The reason joins the file's label, which still names the artifact.
    let placeholder = format!(
        "[file pixel.png, {} B, artifact {}; not shown: custom.test/group/fallback does not \
         accept images]",
        bytes.len(),
        &artifact.sha256[..8]
    );
    assert!(
        second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| part["text"] == placeholder))
            }),
        "{second}"
    );
    fixture.engine.shutdown().await;
}
