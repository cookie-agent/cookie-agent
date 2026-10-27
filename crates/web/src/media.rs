//! Media display blocks shown inline: their artifact is read from the daemon
//! (`artifact.read`) and shown through a `Blob` URL, kept for the page's
//! life so a re-rendered block shows it again at once.

use std::{cell::RefCell, collections::HashMap};

use leptos::task::spawn_local;
use wasm_bindgen::JsValue;

use crate::workspace::Workspace;

thread_local! {
    static URLS: RefCell<HashMap<String, (String, String)>> = RefCell::new(HashMap::new());
}

/// Fills every media block under `root` that is not shown yet.
pub fn show_within(root: &web_sys::Element, workspace: Workspace) {
    let Ok(blocks) = root.query_selector_all("[data-media]:not([data-shown])") else {
        return;
    };
    for index in 0..blocks.length() {
        let Some(element) = blocks
            .item(index)
            .and_then(|node| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(node).ok())
        else {
            continue;
        };
        let Some(sha256) = element.get_attribute("data-media") else {
            continue;
        };
        let _ = element.set_attribute("data-shown", "loading");
        let alt = element.get_attribute("data-alt").unwrap_or_default();
        if let Some((url, media_type)) = URLS.with(|urls| urls.borrow().get(&sha256).cloned()) {
            show(&element, &url, &media_type, &alt);
            continue;
        }
        spawn_local(async move {
            match workspace.read_media(sha256.clone()).await {
                Ok((bytes, media_type)) => match blob_url(&bytes, &media_type) {
                    Ok(url) => {
                        URLS.with(|urls| {
                            urls.borrow_mut()
                                .insert(sha256, (url.clone(), media_type.clone()));
                        });
                        show(&element, &url, &media_type, &alt);
                    }
                    Err(_) => element.set_text_content(Some("the media could not be shown")),
                },
                Err(error) => {
                    element
                        .set_text_content(Some(&format!("the media could not be read: {error}")));
                }
            }
        });
    }
}

fn blob_url(bytes: &[u8], media_type: &str) -> Result<String, JsValue> {
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::of1(&array);
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(media_type);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)?;
    web_sys::Url::create_object_url_with_blob(&blob)
}

fn show(element: &web_sys::Element, url: &str, media_type: &str, alt: &str) {
    let Some(document) = element.owner_document() else {
        return;
    };
    let viewable = media_type.starts_with("image/") || media_type.starts_with("video/");
    let (tag, attributes): (&str, &[(&str, &str)]) = if media_type.starts_with("image/") {
        (
            "img",
            &[
                ("alt", alt),
                ("draggable", "false"),
                ("class", "max-h-96 max-w-full rounded"),
            ],
        )
    } else if media_type.starts_with("video/") {
        // A still of its first frame; the viewer plays it.
        (
            "video",
            &[
                ("preload", "metadata"),
                ("muted", ""),
                ("playsinline", ""),
                ("class", "max-h-96 max-w-full rounded"),
            ],
        )
    } else if media_type.starts_with("audio/") {
        ("audio", &[("controls", "")])
    } else {
        (
            "a",
            &[
                ("download", ""),
                ("class", "text-caramel underline dark:text-maple"),
            ],
        )
    };
    let Ok(shown) = document.create_element(tag) else {
        return;
    };
    let source = if tag == "a" { "href" } else { "src" };
    let _ = shown.set_attribute(source, url);
    for (name, value) in attributes {
        let _ = shown.set_attribute(name, value);
    }
    if tag == "a" {
        shown.set_text_content(Some(if alt.is_empty() { "download" } else { alt }));
    }
    element.set_inner_html("");
    let shown = if viewable {
        opener(&document, shown, url, media_type, alt).unwrap_or_else(|shown| shown)
    } else {
        shown
    };
    let _ = element.append_child(&shown);
    let _ = element.set_attribute("data-shown", "yes");
}

/// `shown` inside a button that opens it in the media viewer (the
/// transcript's `open-media` action); `shown` back when there is no button.
fn opener(
    document: &web_sys::Document,
    shown: web_sys::Element,
    url: &str,
    media_type: &str,
    alt: &str,
) -> Result<web_sys::Element, web_sys::Element> {
    let Ok(button) = document.create_element("button") else {
        return Err(shown);
    };
    let video = media_type.starts_with("video/");
    let name = if !alt.is_empty() {
        alt
    } else if video {
        "video"
    } else {
        "image"
    };
    for (attribute, value) in [
        ("type", "button"),
        ("data-action", "open-media"),
        ("data-src", url),
        ("data-media-type", media_type),
        ("data-alt", alt),
        ("aria-label", &format!("Open {name}")),
        ("title", "Open in the viewer"),
        (
            "class",
            "media-open group relative block max-w-full cursor-zoom-in rounded hover:ring-2 hover:ring-caramel focus-visible:ring-2 focus-visible:ring-caramel dark:hover:ring-maple dark:focus-visible:ring-maple",
        ),
    ] {
        let _ = button.set_attribute(attribute, value);
    }
    let _ = button.append_child(&shown);
    if video {
        let _ = button.insert_adjacent_html(
            "beforeend",
            &format!(
                "<span class=\"pointer-events-none absolute inset-0 flex items-center justify-center\">{}</span>",
                crate::icons::svg(
                    icondata::FaCirclePlaySolid,
                    "text-5xl text-white/90 drop-shadow group-hover:text-white",
                )
            ),
        );
    }
    Ok(button)
}

/// A media file open in the media viewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Viewing {
    /// The file's `Blob` URL.
    pub url: String,
    pub media_type: String,
    /// The file's label; empty when it has none.
    pub alt: String,
}

impl Viewing {
    /// The file is a video (else an image).
    pub fn is_video(&self) -> bool {
        self.media_type.starts_with("video/")
    }

    /// What the viewer calls the file.
    pub fn title(&self) -> &str {
        if self.alt.is_empty() {
            &self.media_type
        } else {
            &self.alt
        }
    }
}

/// The smallest and largest zoom of the media viewer, against the file
/// fitted to the window.
pub const MIN_ZOOM: f64 = 0.25;
pub const MAX_ZOOM: f64 = 32.0;

/// How the media viewer shows its file: zoomed by `scale` about the
/// file's centre, then moved by `x` and `y` pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Zoom {
    pub scale: f64,
    pub x: f64,
    pub y: f64,
}

impl Default for Zoom {
    fn default() -> Self {
        Self {
            scale: 1.0,
            x: 0.0,
            y: 0.0,
        }
    }
}

impl Zoom {
    /// Zoomed by `factor`, keeping the point at `x`, `y` (pixels from the
    /// stage's centre, where the fitted file's centre sits) where it is.
    pub fn zoom_at(self, factor: f64, x: f64, y: f64) -> Self {
        let scale = (self.scale * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let ratio = scale / self.scale;
        Self {
            scale,
            x: x - (x - self.x) * ratio,
            y: y - (y - self.y) * ratio,
        }
    }

    /// Moved by `dx`, `dy` pixels.
    pub fn panned(self, dx: f64, dy: f64) -> Self {
        Self {
            x: self.x + dx,
            y: self.y + dy,
            ..self
        }
    }

    /// The CSS `transform` showing it.
    pub fn transform(self) -> String {
        format!(
            "translate({:.1}px, {:.1}px) scale({:.4})",
            self.x, self.y, self.scale
        )
    }

    /// The zoom as a percentage, "250%".
    pub fn percent(self) -> String {
        format!("{:.0}%", self.scale * 100.0)
    }
}

/// A video's position as `m:ss.ss`, or `h:mm:ss.ss` past an hour; `0:00.00`
/// for a position not known yet.
pub fn clock(seconds: f64) -> String {
    let seconds = if seconds.is_finite() && seconds > 0.0 {
        seconds
    } else {
        0.0
    };
    let hundredths = (seconds * 100.0).floor() as u64;
    let (whole, hundredths) = (hundredths / 100, hundredths % 100);
    let (hours, minutes, seconds) = (whole / 3600, whole / 60 % 60, whole % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}.{hundredths:02}")
    } else {
        format!("{minutes}:{seconds:02}.{hundredths:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        let zoom = Zoom::default().zoom_at(2.0, 100.0, -50.0);
        assert_eq!(zoom.scale, 2.0);
        // The file point under (100, -50) was (100, -50) and is again.
        assert_eq!(
            (zoom.x + 100.0 * zoom.scale, zoom.y - 50.0 * zoom.scale),
            (100.0, -50.0)
        );
        let back = zoom.zoom_at(0.5, 100.0, -50.0);
        assert_eq!(back, Zoom::default());
    }

    #[test]
    fn zoom_stays_within_its_bounds() {
        let zoom = Zoom::default();
        assert_eq!(zoom.zoom_at(1000.0, 0.0, 0.0).scale, MAX_ZOOM);
        assert_eq!(zoom.zoom_at(0.001, 0.0, 0.0).scale, MIN_ZOOM);
        // At a bound, zooming further moves nothing.
        let most = Zoom {
            scale: MAX_ZOOM,
            x: 10.0,
            y: 20.0,
        };
        assert_eq!(most.zoom_at(2.0, 300.0, 300.0), most);
    }

    #[test]
    fn panning_moves_the_file() {
        let zoom = Zoom {
            scale: 3.0,
            x: 1.0,
            y: 2.0,
        }
        .panned(-4.0, 5.0);
        assert_eq!(
            zoom,
            Zoom {
                scale: 3.0,
                x: -3.0,
                y: 7.0
            }
        );
        assert_eq!(zoom.transform(), "translate(-3.0px, 7.0px) scale(3.0000)");
        assert_eq!(zoom.percent(), "300%");
    }

    #[test]
    fn clock_shows_hundredths_and_hours_only_past_one() {
        assert_eq!(clock(0.0), "0:00.00");
        assert_eq!(clock(f64::NAN), "0:00.00");
        assert_eq!(clock(65.257), "1:05.25");
        assert_eq!(clock(3_725.5), "1:02:05.50");
    }
}
