//! The browser side of attaching files: the hidden file inputs `/attach`
//! and the camera open, reading a `File`, and decoding and re-encoding an
//! image through a canvas for a downscale.

use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

/// The file input `/attach` opens.
pub const FILE_INPUT: &str = "attach-input";
/// The file input that asks a phone for its camera.
pub const CAMERA_INPUT: &str = "camera-input";

/// Opens the file picker, or the camera on a phone.
pub fn pick_files(camera: bool) {
    let id = if camera { CAMERA_INPUT } else { FILE_INPUT };
    if let Some(input) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
    {
        input.click();
    }
}

/// The files of a file list, in order.
pub fn files_of(list: Option<web_sys::FileList>) -> Vec<web_sys::File> {
    let Some(list) = list else {
        return Vec::new();
    };
    (0..list.length())
        .filter_map(|index| list.get(index))
        .collect()
}

/// A file's bytes.
pub async fn read_file(file: &web_sys::File) -> Result<Vec<u8>, String> {
    let buffer = JsFuture::from(file.array_buffer())
        .await
        .map_err(describe)?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

/// `bytes` of `media_type` decoded as an image.
pub async fn decode_image(
    bytes: &[u8],
    media_type: &str,
) -> Result<web_sys::HtmlImageElement, String> {
    let array = js_sys::Uint8Array::from(bytes);
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(media_type);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(
        &js_sys::Array::of1(&array),
        &options,
    )
    .map_err(describe)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(describe)?;
    let image = web_sys::HtmlImageElement::new().map_err(describe)?;
    image.set_src(&url);
    let decoded = JsFuture::from(image.decode()).await;
    let _ = web_sys::Url::revoke_object_url(&url);
    decoded.map_err(describe)?;
    Ok(image)
}

/// `image` drawn at `width`×`height` and encoded as `encoding`.
pub fn encode_image(
    image: &web_sys::HtmlImageElement,
    (width, height): (u32, u32),
    encoding: &str,
) -> Result<Vec<u8>, String> {
    let document = web_sys::window()
        .and_then(|window| window.document())
        .ok_or("no document")?;
    let canvas = document
        .create_element("canvas")
        .map_err(describe)?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .map_err(|_| "no canvas")?;
    canvas.set_width(width);
    canvas.set_height(height);
    let context = canvas
        .get_context("2d")
        .map_err(describe)?
        .ok_or("no 2d canvas")?
        .dyn_into::<web_sys::CanvasRenderingContext2d>()
        .map_err(|_| "no 2d canvas")?;
    context
        .draw_image_with_html_image_element_and_dw_and_dh(
            image,
            0.0,
            0.0,
            f64::from(width),
            f64::from(height),
        )
        .map_err(describe)?;
    let url = canvas
        .to_data_url_with_type_and_encoder_options(encoding, &JsValue::from_f64(0.85))
        .map_err(describe)?;
    let data = url
        .split_once(',')
        .map(|(_, data)| data)
        .ok_or("the canvas gave no data")?;
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| error.to_string())
}

fn describe(error: JsValue) -> String {
    error
        .dyn_ref::<js_sys::Error>()
        .map(|error| String::from(error.message()))
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "the browser refused".into())
}
