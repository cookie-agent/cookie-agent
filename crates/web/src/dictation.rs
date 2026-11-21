//! The composer's microphone (`docs/site/specs/voice-input.md` sections 3
//! and 4): it records with `MediaRecorder`, sends the recording to the
//! daemon to transcribe, and puts the words at the end of the draft. The
//! recording is kept only until it is transcribed or discarded.

use std::{cell::RefCell, rc::Rc};

use cookie_agent_client_state::speech;
use cookie_agent_protocol::UploadId;
use leptos::{prelude::*, task::spawn_local};
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::JsFuture;

use crate::workspace::Workspace;

/// How often the level meter and the clock move, in milliseconds.
const TICK_MS: u32 = 70;
/// The levels the meter keeps, more than any bar shows.
const MAX_LEVELS: usize = 600;
/// The per-browser switch that sends the words once they arrive.
pub const SEND_WHEN_DONE_KEY: &str = "cookie.dictation_send";

/// Where a dictation is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// Nothing: the composer's own row shows.
    Idle,
    /// Waiting for the microphone, which may mean the browser's prompt.
    Starting,
    Recording,
    Transcribing,
    /// Every backend failed; the recording waits for Retry or Discard.
    Failed,
}

/// The page's dictation: one at a time, in the viewed session's composer.
#[derive(Clone, Copy)]
pub struct Dictation {
    pub phase: RwSignal<Phase>,
    /// Seconds recorded.
    pub elapsed: RwSignal<u64>,
    /// Why the last transcription failed.
    pub failure: RwSignal<String>,
    /// Send the words as the message once they arrive.
    pub send_when_done: RwSignal<bool>,
    recorder: StoredValue<Option<Recorder>, LocalStorage>,
    levels: StoredValue<Vec<f32>, LocalStorage>,
    /// The recording and its type, kept for Retry.
    kept: StoredValue<Option<(Vec<u8>, String)>, LocalStorage>,
    transcription: StoredValue<Option<UploadId>, LocalStorage>,
    /// Bumped by every start and cancel, so a late answer of an abandoned
    /// dictation is dropped.
    generation: StoredValue<u64>,
}

/// The microphone while it records.
struct Recorder {
    stream: web_sys::MediaStream,
    recorder: web_sys::MediaRecorder,
    context: Option<web_sys::AudioContext>,
    analyser: Option<web_sys::AnalyserNode>,
    ticker: Option<gloo_timers::callback::Interval>,
    _on_data: Closure<dyn FnMut(web_sys::BlobEvent)>,
    _on_stop: Closure<dyn FnMut(web_sys::Event)>,
    /// What `stop` does once the recorder has handed over its data: send
    /// it, or drop it.
    keep: Rc<RefCell<bool>>,
}

impl Recorder {
    /// Releases the microphone, which turns the browser's indicator off.
    fn release(&mut self) {
        self.ticker = None;
        for track in self.stream.get_tracks().iter() {
            if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                track.stop();
            }
        }
        if let Some(context) = self.context.take() {
            let _ = context.close();
        }
    }
}

fn now() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map_or(0.0, |performance| performance.now())
}

/// Whether the page may use the microphone: https, or localhost.
#[must_use]
pub fn secure() -> bool {
    web_sys::window().is_some_and(|window| window.is_secure_context())
}

/// What the browser said when it gave no microphone.
fn refusal(error: &wasm_bindgen::JsValue) -> String {
    let name = error
        .dyn_ref::<web_sys::DomException>()
        .map(web_sys::DomException::name)
        .unwrap_or_default();
    match name.as_str() {
        "NotAllowedError" | "SecurityError" => {
            "Microphone access was refused. Allow it in the browser's site settings, then try again."
                .to_owned()
        }
        "NotFoundError" | "OverconstrainedError" => "No microphone found.".to_owned(),
        "NotReadableError" => "The microphone is in use by another program.".to_owned(),
        _ => format!("The microphone could not start: {}", crate::files::describe(error.clone())),
    }
}

impl Dictation {
    #[must_use]
    pub fn new() -> Self {
        Self {
            phase: RwSignal::new(Phase::Idle),
            elapsed: RwSignal::new(0),
            failure: RwSignal::new(String::new()),
            send_when_done: RwSignal::new(
                crate::browser::load(SEND_WHEN_DONE_KEY).as_deref() == Some("on"),
            ),
            recorder: StoredValue::new_local(None),
            levels: StoredValue::new_local(Vec::new()),
            kept: StoredValue::new_local(None),
            transcription: StoredValue::new_local(None),
            generation: StoredValue::new(0),
        }
    }

    /// Whether the bar shows in place of the composer's row.
    #[must_use]
    pub fn active(self) -> bool {
        self.phase.get() != Phase::Idle
    }

    pub fn set_send_when_done(self, on: bool) {
        crate::browser::store(SEND_WHEN_DONE_KEY, if on { "on" } else { "off" });
        self.send_when_done.set(on);
    }

    fn next_generation(self) -> u64 {
        self.generation.update_value(|generation| *generation += 1);
        self.generation.get_value()
    }

    fn current(self, generation: u64) -> bool {
        self.generation.try_get_value() == Some(generation)
    }

    /// Starts recording, asking for the microphone the first time.
    pub fn start(self, workspace: Workspace) {
        if self.phase.get_untracked() != Phase::Idle || !workspace.writable() {
            return;
        }
        if !secure() {
            workspace
                .status
                .set("The microphone needs https or localhost; this page is plain http.".into());
            return;
        }
        let generation = self.next_generation();
        self.kept.set_value(None);
        self.phase.set(Phase::Starting);
        spawn_local(async move {
            let stream = match open_microphone().await {
                Ok(stream) => stream,
                Err(message) => {
                    if self.current(generation) {
                        self.phase.set(Phase::Idle);
                        workspace.status.set(message);
                    }
                    return;
                }
            };
            if !self.current(generation) {
                for track in stream.get_tracks().iter() {
                    if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                        track.stop();
                    }
                }
                return;
            }
            match self.record(workspace, stream, generation) {
                Ok(()) => {
                    self.elapsed.set(0);
                    self.phase.set(Phase::Recording);
                }
                Err(message) => {
                    self.phase.set(Phase::Idle);
                    workspace.status.set(message);
                }
            }
        });
    }

    fn record(
        self,
        workspace: Workspace,
        stream: web_sys::MediaStream,
        generation: u64,
    ) -> Result<(), String> {
        let chosen = speech::recorder_type(web_sys::MediaRecorder::is_type_supported);
        let options = web_sys::MediaRecorderOptions::new();
        if let Some(kind) = chosen {
            options.set_mime_type(kind);
        }
        let recorder = web_sys::MediaRecorder::new_with_media_stream_and_media_recorder_options(
            &stream, &options,
        )
        .map_err(|error| refusal(&error))?;
        let chunks: Rc<RefCell<Vec<web_sys::Blob>>> = Rc::default();
        let on_data = {
            let chunks = Rc::clone(&chunks);
            Closure::<dyn FnMut(web_sys::BlobEvent)>::new(move |event: web_sys::BlobEvent| {
                if let Some(data) = event.data().filter(|data| data.size() > 0.0) {
                    chunks.borrow_mut().push(data);
                }
            })
        };
        let keep = Rc::new(RefCell::new(true));
        let on_stop = {
            let chunks = Rc::clone(&chunks);
            let keep = Rc::clone(&keep);
            let recorded_type = recorder.mime_type();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let parts = std::mem::take(&mut *chunks.borrow_mut());
                if !*keep.borrow() || !self.current(generation) {
                    return;
                }
                let media_type = if recorded_type.is_empty() {
                    chosen.unwrap_or("audio/webm").to_owned()
                } else {
                    recorded_type.clone()
                };
                spawn_local(async move {
                    let bytes = blob_bytes(&parts, &media_type).await;
                    if !self.current(generation) {
                        return;
                    }
                    match bytes {
                        Ok(bytes) if !bytes.is_empty() => {
                            self.transcribe(workspace, bytes, media_type, generation);
                        }
                        Ok(_) => {
                            self.phase.set(Phase::Idle);
                            workspace.status.set("Nothing was recorded.".into());
                        }
                        Err(message) => {
                            self.phase.set(Phase::Idle);
                            workspace.status.set(message);
                        }
                    }
                });
            })
        };
        recorder.set_ondataavailable(Some(on_data.as_ref().unchecked_ref()));
        recorder.set_onstop(Some(on_stop.as_ref().unchecked_ref()));
        recorder
            .start_with_time_slice(1000)
            .map_err(|error| refusal(&error))?;
        let (context, analyser) = level_meter(&stream);
        let started = now();
        self.levels.set_value(Vec::new());
        let max_seconds = workspace
            .speech
            .get_untracked()
            .and_then(|status| status.max_seconds);
        let ticker = gloo_timers::callback::Interval::new(TICK_MS, move || {
            self.tick(started, max_seconds);
        });
        self.recorder.set_value(Some(Recorder {
            stream,
            recorder,
            context,
            analyser,
            ticker: Some(ticker),
            _on_data: on_data,
            _on_stop: on_stop,
            keep,
        }));
        Ok(())
    }

    /// One tick of the clock and the meter; it stops the recording at the
    /// daemon's limit.
    fn tick(self, started: f64, max_seconds: Option<u64>) {
        let seconds = ((now() - started) / 1000.0).max(0.0) as u64;
        if self.elapsed.get_untracked() != seconds {
            self.elapsed.set(seconds);
        }
        let level = self
            .recorder
            .with_value(|recorder| {
                recorder
                    .as_ref()
                    .and_then(|recorder| recorder.analyser.clone())
            })
            .map_or(0.0, |analyser| level_of(&analyser));
        self.levels.update_value(|levels| {
            levels.push(level);
            if levels.len() > MAX_LEVELS {
                levels.drain(..levels.len() - MAX_LEVELS);
            }
        });
        self.levels.with_value(|levels| draw_meter(levels));
        if max_seconds.is_some_and(|max| seconds >= max) {
            self.stop();
        }
    }

    /// Stops recording and transcribes what it has.
    pub fn stop(self) {
        if self.phase.get_untracked() != Phase::Recording {
            return;
        }
        self.phase.set(Phase::Transcribing);
        self.recorder.update_value(|recorder| {
            if let Some(recorder) = recorder.as_mut() {
                let _ = recorder.recorder.stop();
                recorder.release();
            }
        });
    }

    fn transcribe(self, workspace: Workspace, bytes: Vec<u8>, media_type: String, generation: u64) {
        self.phase.set(Phase::Transcribing);
        self.recorder.update_value(|recorder| {
            if let Some(recorder) = recorder.take() {
                recorder.recorder.set_ondataavailable(None);
                recorder.recorder.set_onstop(None);
            }
        });
        let Some(client) = workspace.client() else {
            self.kept.set_value(Some((bytes, media_type)));
            self.failure.set("not connected to the daemon".into());
            self.phase.set(Phase::Failed);
            return;
        };
        let session = workspace.selected.get_untracked();
        spawn_local(async move {
            let answer = speech::transcribe(&client, &bytes, &media_type, session, |id| {
                if self.current(generation) {
                    self.transcription.set_value(Some(id));
                }
            })
            .await;
            if !self.current(generation) {
                return;
            }
            self.transcription.set_value(None);
            match answer {
                Ok(result) if result.text.trim().is_empty() => {
                    self.phase.set(Phase::Idle);
                    workspace.status.set("Heard no speech.".into());
                }
                Ok(result) => {
                    self.phase.set(Phase::Idle);
                    workspace
                        .composer
                        .update(|draft| *draft = speech::add_words(draft, &result.text));
                    if self.send_when_done.get_untracked() {
                        workspace.submit(cookie_agent_protocol::InputDelivery::Steer);
                    } else {
                        focus_composer();
                    }
                }
                Err(error) if speech::cancelled(&error) => {}
                Err(error) => {
                    self.kept.set_value(Some((bytes, media_type)));
                    self.failure.set(speech::failure_text(&error));
                    self.phase.set(Phase::Failed);
                }
            }
        });
    }

    /// Sends the kept recording again.
    pub fn retry(self, workspace: Workspace) {
        let Some((bytes, media_type)) = self.kept.try_update_value(Option::take).flatten() else {
            return;
        };
        let generation = self.next_generation();
        self.transcribe(workspace, bytes, media_type, generation);
    }

    /// Abandons the dictation, whatever it is doing, and drops the
    /// recording.
    pub fn cancel(self, workspace: Workspace) {
        if self
            .phase
            .try_get_untracked()
            .is_none_or(|phase| phase == Phase::Idle)
        {
            return;
        }
        self.next_generation();
        self.recorder.update_value(|recorder| {
            if let Some(mut recorder) = recorder.take() {
                *recorder.keep.borrow_mut() = false;
                // Its handlers go with it: none may run once it is dropped.
                recorder.recorder.set_ondataavailable(None);
                recorder.recorder.set_onstop(None);
                if recorder.recorder.state() != web_sys::RecordingState::Inactive {
                    let _ = recorder.recorder.stop();
                }
                recorder.release();
            }
        });
        self.kept.set_value(None);
        if let Some(id) = self.transcription.try_update_value(Option::take).flatten()
            && let Some(client) = workspace.client()
        {
            spawn_local(async move { speech::cancel(&client, id).await });
        }
        self.phase.set(Phase::Idle);
        focus_composer();
    }
}

impl Default for Dictation {
    fn default() -> Self {
        Self::new()
    }
}

async fn open_microphone() -> Result<web_sys::MediaStream, String> {
    let devices = web_sys::window()
        .ok_or("no window")?
        .navigator()
        .media_devices()
        .map_err(|_| "This browser offers no microphone.".to_owned())?;
    let constraints = web_sys::MediaStreamConstraints::new();
    let audio = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&audio, &"channelCount".into(), &1.into());
    let _ = js_sys::Reflect::set(&audio, &"echoCancellation".into(), &true.into());
    let _ = js_sys::Reflect::set(&audio, &"noiseSuppression".into(), &true.into());
    constraints.set_audio(&audio);
    let promise = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(|error| refusal(&error))?;
    JsFuture::from(promise)
        .await
        .map_err(|error| refusal(&error))?
        .dyn_into::<web_sys::MediaStream>()
        .map_err(|_| "The microphone could not start.".to_owned())
}

/// An analyser on `stream` for the level meter; none where the browser
/// gives no audio context, and then the meter stays flat.
fn level_meter(
    stream: &web_sys::MediaStream,
) -> (Option<web_sys::AudioContext>, Option<web_sys::AnalyserNode>) {
    let Ok(context) = web_sys::AudioContext::new() else {
        return (None, None);
    };
    let analyser = context.create_analyser().ok();
    if let (Ok(source), Some(analyser)) = (context.create_media_stream_source(stream), &analyser) {
        analyser.set_fft_size(512);
        let _ = source.connect_with_audio_node(analyser);
    }
    (Some(context), analyser)
}

/// The loudness of the last frame, 0 to 1.
fn level_of(analyser: &web_sys::AnalyserNode) -> f32 {
    let mut samples = vec![0u8; analyser.fft_size() as usize];
    analyser.get_byte_time_domain_data(&mut samples);
    let sum = samples
        .iter()
        .map(|sample| {
            let value = (f32::from(*sample) - 128.0) / 128.0;
            value * value
        })
        .sum::<f32>();
    let rms = (sum / samples.len().max(1) as f32).sqrt();
    (rms * 4.0).min(1.0)
}

/// Draws the newest `levels` as bars from the right of the meter.
fn draw_meter(levels: &[f32]) {
    let Some(canvas) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("dictation-meter"))
        .and_then(|element| element.dyn_into::<web_sys::HtmlCanvasElement>().ok())
    else {
        return;
    };
    let ratio = web_sys::window().map_or(1.0, |window| window.device_pixel_ratio());
    let width = f64::from(canvas.client_width());
    let height = f64::from(canvas.client_height());
    if width <= 0.0 || height <= 0.0 {
        return;
    }
    let (pixel_width, pixel_height) = ((width * ratio) as u32, (height * ratio) as u32);
    if canvas.width() != pixel_width || canvas.height() != pixel_height {
        canvas.set_width(pixel_width);
        canvas.set_height(pixel_height);
    }
    let Some(context) = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|context| context.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())
    else {
        return;
    };
    let _ = context.set_transform(ratio, 0.0, 0.0, ratio, 0.0, 0.0);
    context.clear_rect(0.0, 0.0, width, height);
    let color = web_sys::window()
        .and_then(|window| window.get_computed_style(&canvas).ok().flatten())
        .and_then(|style| style.get_property_value("color").ok())
        .unwrap_or_else(|| "#555".to_owned());
    context.set_fill_style_str(&color);
    let step = 4.0;
    let bars = (width / step) as usize;
    let shown = &levels[levels.len().saturating_sub(bars)..];
    for (index, level) in shown.iter().enumerate() {
        let bar = (f64::from(*level) * (height - 4.0)).max(2.0);
        let x = width - (shown.len() - index) as f64 * step + 1.0;
        context.set_global_alpha(if *level < 0.08 { 0.35 } else { 1.0 });
        context.fill_rect(x, (height - bar) / 2.0, 2.0, bar);
    }
    context.set_global_alpha(1.0);
}

async fn blob_bytes(parts: &[web_sys::Blob], media_type: &str) -> Result<Vec<u8>, String> {
    let array = js_sys::Array::new();
    for part in parts {
        array.push(part);
    }
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(media_type);
    let blob = web_sys::Blob::new_with_blob_sequence_and_options(&array, &options)
        .map_err(crate::files::describe)?;
    let buffer = JsFuture::from(blob.array_buffer())
        .await
        .map_err(crate::files::describe)?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

fn focus_composer() {
    if let Some(element) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("composer"))
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = element.focus();
    }
}
