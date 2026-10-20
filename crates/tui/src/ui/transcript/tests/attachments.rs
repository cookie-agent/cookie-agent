//! Attaching files to the composer draft (spec 11.6).

use std::path::Path;

use cookie_agent_protocol::{ArtifactRef, InputMedia, MediaKindLabel, SessionId};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::state::{ComposerInput, SessionState};
use crate::ui::app::*;

use super::support::*;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A structurally complete PNG of `width`×`height` whose image data is
/// `data_bytes` long; nothing decodes its pixels.
fn png(width: u32, height: u32, data_bytes: usize) -> Vec<u8> {
    fn chunk(bytes: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        bytes.extend_from_slice(&crc.finalize().to_be_bytes());
    }
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = width.to_be_bytes().to_vec();
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut bytes, b"IHDR", &header);
    let data = (0..data_bytes)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    chunk(&mut bytes, b"IDAT", &data);
    chunk(&mut bytes, b"IEND", b"");
    bytes
}

fn media(kind: &str, mime_types: &[&str], max_bytes: u64) -> InputMedia {
    InputMedia {
        kind: MediaKindLabel::new(kind).expect("kind"),
        mime_types: mime_types.iter().map(|value| (*value).to_owned()).collect(),
        max_bytes,
        max_dimension: (kind == MediaKindLabel::IMAGE).then_some(8000),
        max_pixels: (kind == MediaKindLabel::IMAGE).then_some(16 * 1024 * 1024),
        max_count: 20,
    }
}

/// An app viewing a fresh session whose draft model accepts `accepted`,
/// talking to a daemon that answers uploads (and `answer` for the rest).
async fn attach_app(
    accepted: Vec<InputMedia>,
    answer: impl Fn(&str, &Value) -> Option<Result<Value, Value>> + Send + 'static,
) -> (App, SessionId, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.store.sessions.insert(session, SessionState::default());
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.models[0].input_media = accepted;
    if app.draft.is_none() {
        app.draft = app.default_draft_selection();
    }
    assert!(app.draft.is_some());
    let (client, recorded) = answering_client(move |method, params| {
        if let Some(answer) = answer(method, params) {
            return answer;
        }
        Ok(match method {
            "artifact.upload.begin" => json!({"upload_id": "upl_1", "chunk_bytes": 1024 * 1024}),
            "artifact.upload.chunk" => json!({}),
            "artifact.upload.finish" => json!({"artifact": {
                "sha256": params["sha256"],
                "byte_length": 1_572_970,
                "media_type": "image/png",
                "filename": "shot.png"
            }}),
            _ => return Err(json!({"code": -32000, "message": "not scripted"})),
        })
    });
    app.client = client;
    (app, session, recorded)
}

/// Waits for the next RPC update and applies it.
async fn apply_next_update(app: &mut App) {
    let update = tokio::time::timeout(std::time::Duration::from_secs(5), app.rpc_updates_rx.recv())
        .await
        .expect("rpc update timeout")
        .expect("rpc update");
    app.handle_rpc_update(update);
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write file");
    path.to_string_lossy().into_owned()
}

fn requests(recorded: &std::sync::Arc<std::sync::Mutex<Vec<Value>>>, method: &str) -> Vec<Value> {
    recorded
        .lock()
        .expect("recorded")
        .iter()
        .filter(|value| value["method"].as_str() == Some(method))
        .map(|value| value["params"].clone())
        .collect()
}

#[tokio::test]
async fn an_accepted_file_uploads_in_chunks_shows_a_chip_and_is_sent_as_a_file_part() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = png(4, 3, 1_572_900);
    let path = write_file(dir.path(), "shot.png", &bytes);
    let (mut app, session, recorded) = attach_app(
        vec![media(
            MediaKindLabel::IMAGE,
            &["image/png", "image/jpeg"],
            4 * 1024 * 1024,
        )],
        |_, _| None,
    )
    .await;

    app.attach_file(&path).await;
    assert!(app.status.contains("attaching shot.png"), "{}", app.status);
    apply_next_update(&mut app).await;

    let begin = requests(&recorded, "artifact.upload.begin");
    assert_eq!(begin.len(), 1);
    assert_eq!(begin[0]["session_id"], session.to_string());
    assert_eq!(begin[0]["byte_length"], bytes.len());
    assert_eq!(begin[0]["filename"], "shot.png");
    // Two chunks of at most 1 MiB, in order, then the digest.
    let chunks = requests(&recorded, "artifact.upload.chunk");
    assert_eq!(
        chunks
            .iter()
            .map(|chunk| chunk["offset"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![0, 1024 * 1024]
    );
    let finish = requests(&recorded, "artifact.upload.finish");
    assert_eq!(
        finish[0]["sha256"],
        cookie_agent_protocol::Sha256Digest::of_bytes(&bytes).to_string()
    );
    assert_eq!(app.attachments.len(), 1);
    // The chip sits on the composer's bottom border.
    let rows = frame_rows(&mut app, 100, 20);
    let border = rows
        .iter()
        .find(|row| row.starts_with("╰") && row.contains("shot.png"))
        .expect("chip on a bottom border");
    assert!(border.contains("📎"), "{border}");
    assert!(border.contains("shot.png · image/png · 1.5 MB"), "{border}");

    type_input(&mut app, "what is wrong here?").await;
    app.handle_key(key(KeyCode::Enter)).await;
    wait_for_recorded_request(&recorded, "session.submit_input", 1).await;
    let submitted = last_request_params(&recorded, "session.submit_input");
    assert_eq!(
        submitted["content"][0],
        json!({"type": "text", "text": "what is wrong here?"})
    );
    assert_eq!(submitted["content"][1]["type"], "file");
    assert_eq!(submitted["content"][1]["artifact"]["filename"], "shot.png");
    assert!(app.attachments.is_empty());
}

#[tokio::test]
async fn a_message_of_attachments_alone_is_sent() {
    let (mut app, _session, recorded) = attach_app(
        vec![media(MediaKindLabel::IMAGE, &["image/png"], 1024)],
        |_, _| None,
    )
    .await;
    app.attachments.push(ArtifactRef {
        sha256: "ab".repeat(32),
        byte_length: 10,
        media_type: "image/png".into(),
        filename: Some("a.png".into()),
    });
    app.handle_key(key(KeyCode::Enter)).await;
    wait_for_recorded_request(&recorded, "session.submit_input", 1).await;
    let submitted = last_request_params(&recorded, "session.submit_input");
    assert_eq!(submitted["content"].as_array().unwrap().len(), 1);
    assert_eq!(submitted["content"][0]["type"], "file");
}

#[tokio::test]
async fn a_kind_the_model_does_not_take_is_refused_before_uploading() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "shot.png", &png(4, 3, 16));
    let (mut app, _session, recorded) = attach_app(
        vec![media(
            MediaKindLabel::PDF,
            &["application/pdf"],
            1024 * 1024,
        )],
        |_, _| None,
    )
    .await;
    app.attach_file(&path).await;
    apply_next_update(&mut app).await;
    assert!(
        app.status
            .contains("does not accept image files; it accepts pdf"),
        "{}",
        app.status
    );
    assert!(requests(&recorded, "artifact.upload.begin").is_empty());
    assert!(app.attachments.is_empty());
}

#[tokio::test]
async fn a_file_over_the_model_limit_is_refused_with_the_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "big.png", &png(4, 3, 4096));
    let (mut app, _session, recorded) = attach_app(
        vec![media(MediaKindLabel::IMAGE, &["image/png"], 1024)],
        |_, _| None,
    )
    .await;
    // Larger than anything the model takes: refused from its size alone.
    app.attach_file(&path).await;
    assert!(
        app.status
            .contains("big.png is 4 KB; Arbitrary Model accepts files up to 1 KB"),
        "{}",
        app.status
    );
    // An image over the pixel limit is read and refused before uploading.
    let wide = write_file(dir.path(), "wide.png", &png(9000, 2, 16));
    app.models[0].input_media = vec![media(MediaKindLabel::IMAGE, &["image/png"], 1024 * 1024)];
    app.attach_file(&wide).await;
    apply_next_update(&mut app).await;
    assert!(
        app.status.contains(
            "wide.png is 9000×2 pixels; Arbitrary Model accepts images up to 8000 pixels a side"
        ),
        "{}",
        app.status
    );
    assert!(requests(&recorded, "artifact.upload.begin").is_empty());
}

#[tokio::test]
async fn the_daemon_refusing_a_too_large_file_shows_its_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "shot.png", &png(4, 3, 64));
    let (mut app, _session, _recorded) = attach_app(
        vec![media(MediaKindLabel::IMAGE, &["image/png"], 1024 * 1024)],
        |method, _| {
            (method == "artifact.upload.finish").then(|| {
                Err(json!({
                    "code": -32000,
                    "message": "The image is too large to attach.",
                    "data": {
                        "code": "media_too_large",
                        "kind": "image",
                        "byte_length": 2000,
                        "max_bytes": 1000
                    }
                }))
            })
        },
    )
    .await;
    app.attach_file(&path).await;
    apply_next_update(&mut app).await;
    assert!(
        app.status
            .contains("shot.png is 2 KB; image files are accepted up to 1000 B"),
        "{}",
        app.status
    );
    assert!(app.attachments.is_empty());
}

#[tokio::test]
async fn backspace_at_the_start_of_the_draft_removes_the_newest_chip() {
    let (mut app, _session, _recorded) = attach_app(
        vec![media(MediaKindLabel::IMAGE, &["image/png"], 1024)],
        |_, _| None,
    )
    .await;
    for name in ["a.png", "b.png"] {
        app.attachments.push(ArtifactRef {
            sha256: format!("{:0>64}", name.len() + app.attachments.len()),
            byte_length: 10,
            media_type: "image/png".into(),
            filename: Some(name.into()),
        });
    }
    type_input(&mut app, "x").await;
    // With text before the cursor, Backspace edits the text.
    app.handle_key(key(KeyCode::Backspace)).await;
    assert_eq!(app.input.as_str(), "");
    assert_eq!(app.attachments.len(), 2);
    app.handle_key(key(KeyCode::Backspace)).await;
    assert_eq!(app.attachments.len(), 1);
    assert_eq!(app.attachments[0].display_name(), "a.png");
    assert!(app.status.contains("removed b.png"), "{}", app.status);
}

#[tokio::test]
async fn a_recalled_input_restores_its_files_as_chips() {
    let (mut app, session, _recorded) = attach_app(
        vec![media(MediaKindLabel::IMAGE, &["image/png"], 1024)],
        |_, _| None,
    )
    .await;
    let file = ArtifactRef {
        sha256: "cd".repeat(32),
        byte_length: 482_113,
        media_type: "image/png".into(),
        filename: Some("chart.png".into()),
    };
    app.handle_rpc_update(RpcUpdate::SteerRecalled {
        session_id: session,
        input: ComposerInput {
            text: "look at this".into(),
            files: vec![file.clone()],
        },
    });
    assert_eq!(app.input.as_str(), "look at this");
    assert_eq!(app.attachments, vec![file.clone()]);
    // The same input voided by a failed send restores once, not twice.
    app.handle_rpc_update(RpcUpdate::SteerFailed {
        session_id: session,
        input: ComposerInput {
            text: String::new(),
            files: vec![file.clone()],
        },
        error: "transport closed".into(),
    });
    assert_eq!(app.attachments, vec![file]);
    assert_eq!(app.input.as_str(), "look at this");
}

#[tokio::test]
async fn a_pasted_file_path_is_offered_for_attaching_not_attached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_file(dir.path(), "shot.png", &png(4, 3, 64));
    let (mut app, _session, recorded) = attach_app(
        vec![media(MediaKindLabel::IMAGE, &["image/png"], 1024 * 1024)],
        |_, _| None,
    )
    .await;
    app.handle_paste(&format!("'{path}'"));
    assert_eq!(app.modal, Modal::AttachConfirm);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("Attach shot.png"), "{rendered}");
    // Declining types the pasted text, as a plain paste would have.
    app.handle_key(key(KeyCode::Esc)).await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(app.input.as_str(), format!("'{path}'"));
    assert!(requests(&recorded, "artifact.upload.begin").is_empty());

    app.handle_paste(&path);
    app.handle_key(key(KeyCode::Enter)).await;
    apply_next_update(&mut app).await;
    assert_eq!(requests(&recorded, "artifact.upload.begin").len(), 1);
    assert_eq!(app.attachments.len(), 1);

    // A model that takes no files gets the paste as text.
    app.models[0].input_media = Vec::new();
    app.handle_paste(&path);
    assert_eq!(app.modal, Modal::None);
}

#[tokio::test]
async fn attach_is_offered_only_for_a_model_that_takes_files() {
    let (mut app, _session, _recorded) = attach_app(Vec::new(), |_, _| None).await;
    let attach = crate::ui::slash::COMMANDS
        .iter()
        .find(|spec| spec.name == "attach")
        .expect("attach command");
    assert!(!app.command_is_available(attach));
    app.attach_file("/dev/null").await;
    assert!(
        app.status.contains("accepts no attachments"),
        "{}",
        app.status
    );
    app.models[0].input_media = vec![
        media(MediaKindLabel::IMAGE, &["image/png"], 1024),
        media(MediaKindLabel::PDF, &["application/pdf"], 1024),
    ];
    assert!(app.command_is_available(attach));
    assert_eq!(
        app.accepted_media().expect("accepted").kinds(),
        "image, pdf"
    );
}

#[test]
fn pasted_paths_are_recognized_as_terminals_paste_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("my shot.png");
    std::fs::write(&path, b"x").expect("write");
    let plain = path.to_string_lossy().into_owned();
    let found = |text: &str| crate::ui::app::pasted_file_path_for_test(text);
    let mut pastes = vec![
        format!("'{plain}'"),
        format!("\"{plain}\"\n"),
        format!("file://{}", plain.replace(' ', "%20")),
    ];
    // A shell's backslash escape is a separator in a Windows path.
    if !cfg!(windows) {
        pastes.push(plain.replace(' ', "\\ "));
    }
    for pasted in pastes {
        assert_eq!(found(&pasted).as_deref(), Some(path.as_path()), "{pasted}");
    }
    for pasted in [
        "my shot.png".to_owned(),
        format!("{plain}\n{plain}"),
        dir.path().to_string_lossy().into_owned(),
        format!("{plain}.missing"),
    ] {
        assert_eq!(found(&pasted), None, "{pasted}");
    }
}
