//! The media viewer: an image or video from the transcript over the whole
//! page, to inspect it. The wheel, a pinch, the zoom buttons, and `+`, `-`,
//! `0` zoom; a drag moves the zoomed file; a double click zooms in, or back
//! to fit. A video plays with its own bar: play and pause (Space or `k`),
//! a frame back or forward (`,` and `.`), five seconds back or forward
//! (Left and Right), a seek bar, its speed, and its sound. The bar on top
//! names the file and its size in pixels. Escape, ✕, or a click beside the
//! file closes it.

use std::collections::HashMap;

use leptos::{ev, html, prelude::*};
use wasm_bindgen::{JsCast as _, closure::Closure};

use crate::{
    icons::Glyph,
    media::{Viewing, Zoom, clock},
    workspace::Workspace,
};

const BAR_BUTTON: &str = "inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center gap-1 rounded-md px-2 text-sm hover:bg-white/15 active:bg-white/25 disabled:opacity-40";

/// The step of the frame buttons: a frame at 30 frames a second, since a
/// browser does not tell a video's frame rate.
const FRAME: f64 = 1.0 / 30.0;

/// The speeds a video plays at.
const SPEEDS: [f64; 6] = [0.1, 0.25, 0.5, 1.0, 1.5, 2.0];

/// The viewer, open while `workspace.media_viewer` holds a file.
#[component]
pub(crate) fn MediaViewer(workspace: Workspace) -> impl IntoView {
    move || {
        workspace
            .media_viewer
            .get()
            .map(|viewing| view! { <Viewer workspace viewing /> })
    }
}

#[component]
fn Viewer(workspace: Workspace, viewing: Viewing) -> impl IntoView {
    let close = move || workspace.media_viewer.set(None);
    let zoom = RwSignal::new(Zoom::default());
    let root = NodeRef::<html::Div>::new();
    let stage = NodeRef::<html::Div>::new();
    let video = NodeRef::<html::Video>::new();
    let is_video = viewing.is_video();
    let title = viewing.title().to_owned();
    let label = title.clone();
    let download = if viewing.alt.is_empty() {
        "media".to_owned()
    } else {
        viewing.alt.clone()
    };

    // The focus goes to the viewer, for its keys, and back where it was
    // once the viewer closes.
    let before = StoredValue::new_local(
        web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.active_element()),
    );
    Effect::new(move |_| {
        if let Some(root) = root.get() {
            let _ = root.focus();
        }
    });
    on_cleanup(move || {
        if let Some(Some(before)) = before.try_get_value()
            && before.is_connected()
            && let Ok(before) = before.dyn_into::<web_sys::HtmlElement>()
        {
            let _ = before.focus();
        }
    });

    // Where a point of the page is, from the stage's centre.
    let from_centre = move |client_x: i32, client_y: i32| {
        stage.get_untracked().map_or((0.0, 0.0), |stage| {
            let rect = stage.get_bounding_client_rect();
            (
                f64::from(client_x) - rect.left() - rect.width() / 2.0,
                f64::from(client_y) - rect.top() - rect.height() / 2.0,
            )
        })
    };

    // The wheel zooms. The page gets it through a listener that is not
    // passive, so that the browser's own zoom (Ctrl and the wheel, or a
    // trackpad's pinch) does not zoom the page instead.
    let wheel = StoredValue::new_local(None::<Closure<dyn Fn(web_sys::WheelEvent)>>);
    Effect::new(move |_| {
        let Some(element) = stage.get() else {
            return;
        };
        let listener =
            Closure::<dyn Fn(web_sys::WheelEvent)>::new(move |event: web_sys::WheelEvent| {
                event.prevent_default();
                let delta = match event.delta_mode() {
                    web_sys::WheelEvent::DOM_DELTA_LINE => event.delta_y() * 16.0,
                    web_sys::WheelEvent::DOM_DELTA_PAGE => event.delta_y() * 400.0,
                    _ => event.delta_y(),
                };
                // A pinch arrives as small steps with Ctrl held.
                let rate = if event.ctrl_key() { 0.01 } else { 0.002 };
                let (x, y) = from_centre(event.client_x(), event.client_y());
                zoom.update(|zoom| *zoom = zoom.zoom_at((-delta * rate).exp(), x, y));
            });
        let options = web_sys::AddEventListenerOptions::new();
        options.set_passive(false);
        let _ = element.add_event_listener_with_callback_and_add_event_listener_options(
            "wheel",
            listener.as_ref().unchecked_ref(),
            &options,
        );
        wheel.set_value(Some(listener));
    });

    // The pointers down on the stage, by ID: one drags, two pinch.
    let pointers = StoredValue::new(HashMap::<i32, (f64, f64)>::new());
    // How far the pointers moved since the press, to tell a drag from a
    // click, and whether the press was on the file.
    let travel = StoredValue::new(0.0_f64);
    let on_file = StoredValue::new(false);
    let pressed_on_file = |event: &ev::PointerEvent| {
        event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|target| target.closest("[data-viewer-file]").ok().flatten())
            .is_some()
    };
    let on_pointerdown = move |event: ev::PointerEvent| {
        if event.pointer_type() == "mouse" && event.button() != 0 {
            return;
        }
        if event.pointer_type() == "mouse" {
            // No text or image selection under a drag.
            event.prevent_default();
        }
        if pointers.with_value(HashMap::is_empty) {
            travel.set_value(0.0);
            on_file.set_value(pressed_on_file(&event));
        }
        if let Some(stage) = stage.get_untracked() {
            let _ = stage.set_pointer_capture(event.pointer_id());
        }
        pointers.update_value(|pointers| {
            pointers.insert(
                event.pointer_id(),
                (f64::from(event.client_x()), f64::from(event.client_y())),
            );
        });
    };
    let on_pointermove = move |event: ev::PointerEvent| {
        let id = event.pointer_id();
        let now = (f64::from(event.client_x()), f64::from(event.client_y()));
        let Some(was) = pointers.with_value(|pointers| pointers.get(&id).copied()) else {
            return;
        };
        let other = pointers.with_value(|pointers| {
            pointers
                .iter()
                .find(|(other, _)| **other != id)
                .map(|(_, at)| *at)
        });
        pointers.update_value(|pointers| {
            pointers.insert(id, now);
        });
        travel.update_value(|travel| *travel += (now.0 - was.0).hypot(now.1 - was.1));
        match other {
            // Two fingers: the file follows their midpoint and zooms with
            // their spread.
            Some(other) => {
                let before = was.0 - other.0;
                let before = before.hypot(was.1 - other.1);
                let after = (now.0 - other.0).hypot(now.1 - other.1);
                let mid = |a: (f64, f64)| ((a.0 + other.0) / 2.0, (a.1 + other.1) / 2.0);
                let (from, to) = (mid(was), mid(now));
                let (x, y) = from_centre(to.0 as i32, to.1 as i32);
                zoom.update(|zoom| {
                    let panned = zoom.panned(to.0 - from.0, to.1 - from.1);
                    *zoom = if before > 0.0 {
                        panned.zoom_at(after / before, x, y)
                    } else {
                        panned
                    };
                });
            }
            None => zoom.update(|zoom| *zoom = zoom.panned(now.0 - was.0, now.1 - was.1)),
        }
    };
    let on_pointerup = move |event: ev::PointerEvent| {
        pointers.update_value(|pointers| {
            pointers.remove(&event.pointer_id());
        });
    };

    // A click on a video plays or pauses it, a click beside the file
    // closes the viewer; a drag does neither.
    let toggle_play = move || {
        if let Some(video) = video.get_untracked() {
            if video.paused() {
                let _ = video.play();
            } else {
                let _ = video.pause();
            }
        }
    };
    let on_click = move |_: ev::MouseEvent| {
        if travel.get_value() > 5.0 {
            return;
        }
        if !on_file.get_value() {
            close();
        } else if is_video {
            toggle_play();
        }
    };
    let on_dblclick = move |event: ev::MouseEvent| {
        if !on_file.get_value() {
            return;
        }
        let (x, y) = from_centre(event.client_x(), event.client_y());
        zoom.update(|zoom| {
            *zoom = if *zoom == Zoom::default() {
                zoom.zoom_at(2.5, x, y)
            } else {
                Zoom::default()
            };
        });
    };
    let zoom_by = move |factor: f64| zoom.update(|zoom| *zoom = zoom.zoom_at(factor, 0.0, 0.0));

    // The video's state, as its events tell it.
    let playing = RwSignal::new(false);
    let position = RwSignal::new(0.0_f64);
    let duration = RwSignal::new(0.0_f64);
    let speed = RwSignal::new(1.0_f64);
    let muted = RwSignal::new(false);
    let seek = move |to: f64| {
        if let Some(video) = video.get_untracked() {
            let end = video.duration();
            let to = if end.is_finite() {
                to.clamp(0.0, end)
            } else {
                to.max(0.0)
            };
            video.set_current_time(to);
            position.set(to);
        }
    };
    let step = move |by: f64| {
        if let Some(video) = video.get_untracked() {
            let _ = video.pause();
            seek(video.current_time() + by);
        }
    };

    let on_keydown = move |event: ev::KeyboardEvent| {
        if event.ctrl_key() || event.meta_key() || event.alt_key() {
            return;
        }
        let in_field = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|target| target.tag_name() == "SELECT");
        let handled = match event.key().as_str() {
            "Escape" => {
                close();
                true
            }
            "+" | "=" => {
                zoom_by(1.25);
                true
            }
            "-" | "_" => {
                zoom_by(0.8);
                true
            }
            "0" => {
                zoom.set(Zoom::default());
                true
            }
            " " | "k" if is_video && !in_field => {
                toggle_play();
                true
            }
            "," if is_video => {
                step(-FRAME);
                true
            }
            "." if is_video => {
                step(FRAME);
                true
            }
            "ArrowLeft" if is_video && !in_field => {
                seek(position.get_untracked() - 5.0);
                true
            }
            "ArrowRight" if is_video && !in_field => {
                seek(position.get_untracked() + 5.0);
                true
            }
            _ => false,
        };
        if handled {
            event.prevent_default();
            event.stop_propagation();
        }
    };

    // The file's own size in pixels, and how large its pixels show when it
    // fits the window, measured once it loads and whenever the window
    // resizes. Pixels shown three times their size or more show crisp
    // rather than blurred.
    let size = RwSignal::new(None::<(u32, u32)>);
    let fitted = RwSignal::new(1.0_f64);
    let measure = move || {
        let Some(element) = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id("media-viewer-file"))
        else {
            return;
        };
        let natural = if let Some(image) = element.dyn_ref::<web_sys::HtmlImageElement>() {
            (image.natural_width(), image.natural_height())
        } else if let Some(video) = element.dyn_ref::<web_sys::HtmlVideoElement>() {
            (video.video_width(), video.video_height())
        } else {
            return;
        };
        if natural.0 == 0 {
            return;
        }
        size.set(Some(natural));
        if let Some(element) = element.dyn_ref::<web_sys::HtmlElement>() {
            fitted.set(f64::from(element.offset_width()) / f64::from(natural.0));
        }
    };
    let resized = window_event_listener(ev::resize, move |_| measure());
    on_cleanup(move || resized.remove());
    let file_class = move || {
        format!(
            "max-h-full max-w-full select-none object-contain will-change-transform {} {}",
            if is_video && zoom.get() == Zoom::default() {
                "cursor-pointer"
            } else {
                "cursor-grab active:cursor-grabbing"
            },
            if zoom.get().scale * fitted.get() >= 3.0 {
                "[image-rendering:pixelated]"
            } else {
                ""
            }
        )
    };
    let transform = move || zoom.get().transform();
    let url = viewing.url.clone();
    let file = if is_video {
        view! {
            <video
                node_ref=video
                id="media-viewer-file"
                data-viewer-file
                src=url
                autoplay
                playsinline
                class=file_class
                style:transform=transform
                on:loadedmetadata=move |_| {
                    if let Some(video) = video.get_untracked() {
                        duration.set(video.duration());
                    }
                    measure();
                }
                on:durationchange=move |_| {
                    if let Some(video) = video.get_untracked() {
                        duration.set(video.duration());
                    }
                }
                on:timeupdate=move |_| {
                    if let Some(video) = video.get_untracked() {
                        position.set(video.current_time());
                    }
                }
                on:seeked=move |_| {
                    if let Some(video) = video.get_untracked() {
                        position.set(video.current_time());
                    }
                }
                on:play=move |_| playing.set(true)
                on:pause=move |_| playing.set(false)
                on:ended=move |_| playing.set(false)
                on:ratechange=move |_| {
                    if let Some(video) = video.get_untracked() {
                        speed.set(video.playback_rate());
                    }
                }
                on:volumechange=move |_| {
                    if let Some(video) = video.get_untracked() {
                        muted.set(video.muted());
                    }
                }
            ></video>
        }
        .into_any()
    } else {
        view! {
            <img
                id="media-viewer-file"
                data-viewer-file
                src=url
                alt=viewing.alt.clone()
                draggable="false"
                class=file_class
                style:transform=transform
                on:load=move |_| measure()
            />
        }
        .into_any()
    };

    let controls = is_video.then(|| {
        let known = move || duration.get().is_finite() && duration.get() > 0.0;
        view! {
            <div
                id="media-viewer-controls"
                class="flex shrink-0 flex-wrap items-center gap-1 px-2 pt-1 pb-[max(0.25rem,env(safe-area-inset-bottom))]"
            >
                <button
                    type="button"
                    id="media-viewer-play"
                    class=BAR_BUTTON
                    aria-label=move || if playing.get() { "Pause" } else { "Play" }
                    title=move || if playing.get() { "Pause (Space)" } else { "Play (Space)" }
                    on:click=move |_| toggle_play()
                >
                    {move || {
                        if playing.get() {
                            view! { <Glyph icon=icondata::FaPauseSolid /> }
                        } else {
                            view! { <Glyph icon=icondata::FaPlaySolid /> }
                        }
                    }}
                </button>
                <button
                    type="button"
                    id="media-viewer-back"
                    class=BAR_BUTTON
                    aria-label="Back a frame"
                    title="Back a frame (,)"
                    on:click=move |_| step(-FRAME)
                >
                    <Glyph icon=icondata::FaBackwardStepSolid />
                </button>
                <button
                    type="button"
                    id="media-viewer-forward"
                    class=BAR_BUTTON
                    aria-label="Forward a frame"
                    title="Forward a frame (.)"
                    on:click=move |_| step(FRAME)
                >
                    <Glyph icon=icondata::FaForwardStepSolid />
                </button>
                <span class="shrink-0 px-1 font-mono text-xs tabular-nums" id="media-viewer-time">
                    {move || clock(position.get())}
                </span>
                <input
                    type="range"
                    id="media-viewer-seek"
                    aria-label="Position"
                    class="h-11 min-w-32 flex-1 cursor-pointer accent-caramel dark:accent-maple"
                    min="0"
                    step="any"
                    disabled=move || !known()
                    prop:max=move || if known() { duration.get() } else { 0.0 }
                    prop:value=move || position.get()
                    on:input=move |event| {
                        if let Ok(to) = event_target_value(&event).parse::<f64>() {
                            seek(to);
                        }
                    }
                />
                <span class="shrink-0 px-1 font-mono text-xs tabular-nums" id="media-viewer-duration">
                    {move || clock(duration.get())}
                </span>
                <select
                    id="media-viewer-speed"
                    aria-label="Speed"
                    title="Speed"
                    class="min-h-11 shrink-0 rounded-md bg-transparent px-1 text-sm hover:bg-white/15 [&>option]:text-black"
                    prop:value=move || speed.get().to_string()
                    on:change=move |event| {
                        if let (Ok(to), Some(video)) = (
                            event_target_value(&event).parse::<f64>(),
                            video.get_untracked(),
                        ) {
                            video.set_playback_rate(to);
                        }
                    }
                >
                    {SPEEDS
                        .iter()
                        .map(|speed| {
                            view! { <option value=speed.to_string()>{format!("{speed}×")}</option> }
                        })
                        .collect_view()}
                </select>
                <button
                    type="button"
                    id="media-viewer-mute"
                    class=BAR_BUTTON
                    aria-label=move || if muted.get() { "Unmute" } else { "Mute" }
                    title=move || if muted.get() { "Unmute" } else { "Mute" }
                    on:click=move |_| {
                        if let Some(video) = video.get_untracked() {
                            video.set_muted(!video.muted());
                        }
                    }
                >
                    {move || {
                        if muted.get() {
                            view! { <Glyph icon=icondata::FaVolumeXmarkSolid /> }
                        } else {
                            view! { <Glyph icon=icondata::FaVolumeHighSolid /> }
                        }
                    }}
                </button>
            </div>
        }
    });

    view! {
        <div
            node_ref=root
            id="media-viewer"
            role="dialog"
            aria-modal="true"
            aria-label=label
            tabindex="-1"
            class="fixed inset-0 z-[60] flex flex-col bg-black/95 text-white outline-none"
            on:keydown=on_keydown
        >
            <div class="flex shrink-0 items-center gap-1 px-2 pt-[max(0.25rem,env(safe-area-inset-top))] pb-1">
                <span class="flex min-w-0 flex-1 items-baseline gap-2 px-2 text-sm">
                    <span id="media-viewer-title" class="min-w-0 truncate">{title}</span>
                    <span id="media-viewer-size" class="shrink-0 font-mono text-xs text-white/70 tabular-nums">
                        {move || size.get().map(|(width, height)| format!("{width}×{height}"))}
                    </span>
                </span>
                <button
                    type="button"
                    id="media-viewer-zoom-out"
                    class=BAR_BUTTON
                    aria-label="Zoom out"
                    title="Zoom out (-)"
                    on:click=move |_| zoom_by(0.8)
                >
                    <Glyph icon=icondata::FaMagnifyingGlassMinusSolid />
                </button>
                <button
                    type="button"
                    id="media-viewer-fit"
                    class=format!("{BAR_BUTTON} font-mono tabular-nums")
                    aria-label="Fit to the window"
                    title="Fit to the window (0)"
                    on:click=move |_| zoom.set(Zoom::default())
                >
                    {move || zoom.get().percent()}
                </button>
                <button
                    type="button"
                    id="media-viewer-zoom-in"
                    class=BAR_BUTTON
                    aria-label="Zoom in"
                    title="Zoom in (+)"
                    on:click=move |_| zoom_by(1.25)
                >
                    <Glyph icon=icondata::FaMagnifyingGlassPlusSolid />
                </button>
                <a
                    id="media-viewer-download"
                    class=BAR_BUTTON
                    href=viewing.url.clone()
                    download=download
                    aria-label="Download"
                    title="Download"
                >
                    <Glyph icon=icondata::FaDownloadSolid />
                </a>
                <button
                    type="button"
                    id="media-viewer-close"
                    class=BAR_BUTTON
                    aria-label="Close"
                    title="Close (Esc)"
                    on:click=move |_| close()
                >
                    <Glyph icon=icondata::FaXmarkSolid />
                </button>
            </div>
            <div
                node_ref=stage
                id="media-viewer-stage"
                class="relative flex min-h-0 flex-1 touch-none items-center justify-center overflow-hidden p-2 select-none"
                on:pointerdown=on_pointerdown
                on:pointermove=on_pointermove
                on:pointerup=on_pointerup
                on:pointercancel=on_pointerup
                on:click=on_click
                on:dblclick=on_dblclick
            >
                {file}
            </div>
            {controls}
        </div>
    }
}
