//! Entry thumbnails: fetched from the docs site the first time their card
//! shows, decoded off the UI thread, kept as textures for the session. Each
//! is tried once: offline, cards keep their placeholder; the gallery's
//! Reload forgets the failures.

use crate::gallery::GalleryEntry;
use egui::{ColorImage, Context, Id, TextureHandle, TextureOptions};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Wide enough for a card of three columns; a quarter of an 800 px picture.
const WIDTH: u32 = 400;

enum Thumb {
    Loading,
    Decoded(ColorImage),
    Ready(TextureHandle),
    Failed,
}

/// The thumbnails of one egui context, by entry key.
type Thumbs = Arc<Mutex<HashMap<String, Thumb>>>;

fn thumbs(ctx: &Context) -> Thumbs {
    let id = Id::unique("gallery_thumbs");
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Thumbs>(id).clone())
}

/// The texture of `entry`'s thumbnail once it arrived; the first call starts
/// its fetch.
pub fn texture(ctx: &Context, entry: &GalleryEntry) -> Option<TextureHandle> {
    // Tests draw cards without the network.
    if cfg!(test) {
        return None;
    }
    let path = entry.thumbnail.as_deref()?;
    let thumbs = thumbs(ctx);
    let mut map = thumbs.lock().unwrap_or_else(|p| p.into_inner());
    match map.get_mut(entry.key.as_str()) {
        None => {
            map.insert(entry.key.clone(), Thumb::Loading);
            drop(map);
            let url = format!("{}{path}", crate::gallery::remote::SITE);
            fetch(ctx, &thumbs, entry.key.clone(), url);
            None
        }
        Some(slot @ Thumb::Decoded(_)) => {
            let Thumb::Decoded(image) = std::mem::replace(slot, Thumb::Loading) else {
                return None;
            };
            let name = format!("gallery-{}", entry.key);
            let texture = ctx.load_texture(name, image, TextureOptions::LINEAR);
            *slot = Thumb::Ready(texture.clone());
            Some(texture)
        }
        Some(Thumb::Ready(texture)) => Some(texture.clone()),
        Some(Thumb::Loading | Thumb::Failed) => None,
    }
}

/// Forgets the thumbnails that failed, so their cards fetch them again.
pub fn forget_failed(ctx: &Context) {
    let thumbs = thumbs(ctx);
    let mut map = thumbs.lock().unwrap_or_else(|p| p.into_inner());
    map.retain(|_, t| !matches!(t, Thumb::Failed));
}

fn fetch(ctx: &Context, thumbs: &Thumbs, key: String, url: String) {
    let (ctx, thumbs) = (ctx.clone(), Arc::clone(thumbs));
    let done = move |url: &str, image: Result<ColorImage, String>| {
        let thumb = match image {
            Ok(image) => Thumb::Decoded(image),
            Err(e) => {
                log::debug!("thumbnail {url}: {e}");
                Thumb::Failed
            }
        };
        let mut map = thumbs.lock().unwrap_or_else(|p| p.into_inner());
        map.insert(key, thumb);
        ctx.request_repaint();
    };
    #[cfg(not(target_arch = "wasm32"))]
    crate::utils::executor::TaskExecutor::spawn_background(move || {
        let rt = crate::utils::executor::get_shared_tokio_rt();
        let bytes = rt.block_on(crate::data::backends::http::fetch_url_bytes(&url));
        done(&url, bytes.and_then(|b| decode(&b)));
    });
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_futures::spawn_local(async move {
        let image = match crate::data::backends::http::fetch_url_bytes(&url).await {
            Ok(bytes) => browser::decode(&bytes).await,
            Err(e) => Err(e),
        };
        done(&url, image);
    });
}

/// The picture in `bytes`, at most [`WIDTH`] wide. Desktop only: the
/// browser decodes its own pictures (`browser`), keeping the image crate's
/// PNG and WebP decoders out of the wasm.
#[cfg(not(target_arch = "wasm32"))]
fn decode(bytes: &[u8]) -> Result<ColorImage, String> {
    let image = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let image = if image.width() > WIDTH {
        image.thumbnail(WIDTH, u32::MAX)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Pictures decoded by the browser: `createImageBitmap`, drawn at most
/// [`WIDTH`] wide on an `OffscreenCanvas`, read back as pixels.
#[cfg(target_arch = "wasm32")]
mod browser {
    use super::WIDTH;
    use egui::ColorImage;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;

    fn error(e: JsValue) -> String {
        format!("{e:?}")
    }

    pub async fn decode(bytes: &[u8]) -> Result<ColorImage, String> {
        let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
        let blob = web_sys::Blob::new_with_u8_array_sequence(&parts).map_err(error)?;
        let window = web_sys::window().ok_or("no window")?;
        let promise = window.create_image_bitmap_with_blob(&blob).map_err(error)?;
        let bitmap: web_sys::ImageBitmap = JsFuture::from(promise)
            .await
            .map_err(error)?
            .unchecked_into();
        let (width, height) = (bitmap.width().max(1), bitmap.height());
        let w = width.min(WIDTH);
        let h = (u64::from(height) * u64::from(w) / u64::from(width)).max(1) as u32;
        let canvas = web_sys::OffscreenCanvas::new(w, h).map_err(error)?;
        let context: web_sys::OffscreenCanvasRenderingContext2d = canvas
            .get_context("2d")
            .map_err(error)?
            .ok_or("no 2d context")?
            .unchecked_into();
        let (fw, fh) = (f64::from(w), f64::from(h));
        context
            .draw_image_with_image_bitmap_and_dw_and_dh(&bitmap, 0.0, 0.0, fw, fh)
            .map_err(error)?;
        bitmap.close();
        let pixels = context.get_image_data(0.0, 0.0, fw, fh).map_err(error)?;
        let size = [w as usize, h as usize];
        Ok(ColorImage::from_rgba_unmultiplied(size, &pixels.data()))
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    /// A WebP picture decodes to at most [`super::WIDTH`] wide, its shape kept.
    #[test]
    fn a_wide_picture_shrinks_to_the_card() {
        let mut bytes = Vec::new();
        let picture = image::RgbaImage::from_pixel(800, 450, image::Rgba([10, 20, 30, 255]));
        let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut bytes);
        let encoded = picture.write_with_encoder(encoder);
        assert!(encoded.is_ok(), "{encoded:?}");
        let decoded = super::decode(&bytes).map(|i| i.size);
        assert_eq!(decoded, Ok([400, 225]));
        assert!(super::decode(b"not a picture").is_err());
    }
}
