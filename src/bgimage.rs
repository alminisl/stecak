//! Background image loading. Decoding and resizing happen on a short-lived worker
//! thread; the result is sized to the window so the GPU texture is never larger than
//! the screen area it covers (a 4K wallpaper would otherwise cost ~33 MB of VRAM).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use image::imageops::FilterType;
use winit::event_loop::EventLoopProxy;

use crate::pane::UserEvent;

/// Decode `path`, fit it to `target` (physical pixels) according to `fit`, and send the
/// RGBA result back. Requests superseded by a newer `generation` (e.g. during a live
/// window resize) are dropped before doing any work.
pub fn load_async(
    path: PathBuf,
    fit: String,
    target: (u32, u32),
    generation: Arc<AtomicU64>,
    proxy: EventLoopProxy<UserEvent>,
) {
    let my_gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = std::thread::Builder::new().name("bg-image".into()).spawn(move || {
        // Debounce: live resizes fire many events; only the last one decodes.
        std::thread::sleep(Duration::from_millis(120));
        if generation.load(Ordering::SeqCst) != my_gen {
            return;
        }
        let result = decode(&path, &fit, target);
        if let Err(e) = &result {
            log::error!("background image {}: {e}", path.display());
        }
        if generation.load(Ordering::SeqCst) == my_gen {
            let _ = proxy.send_event(UserEvent::BackgroundImage(result.ok()));
        }
    });
}

/// JPEGs are decoded at the smallest 1/2, 1/4 or 1/8 DCT scale that still covers the
/// window, which cuts peak memory for big wallpapers by up to 64×.
fn decode_jpeg_scaled(path: &PathBuf, (tw, th): (u32, u32)) -> Option<image::DynamicImage> {
    let file = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut dec = jpeg_decoder::Decoder::new(file);
    dec.read_info().ok()?;
    let info = dec.info()?;
    // For "cover" the image must cover the window in both directions.
    let (iw, ih) = (info.width as f32, info.height as f32);
    let s = (tw as f32 / iw).max(th as f32 / ih).min(1.0);
    let (w, h) = dec.scale((iw * s).ceil() as u16, (ih * s).ceil() as u16).ok()?;
    let pixels = dec.decode().ok()?;
    let (w, h) = (w as u32, h as u32);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => image::RgbImage::from_raw(w, h, pixels).map(image::DynamicImage::ImageRgb8),
        jpeg_decoder::PixelFormat::L8 => image::GrayImage::from_raw(w, h, pixels).map(image::DynamicImage::ImageLuma8),
        _ => None, // CMYK etc.: fall back to the full decoder
    }
}

fn decode(path: &PathBuf, fit: &str, (tw, th): (u32, u32)) -> Result<(Vec<u8>, u32, u32), String> {
    let is_jpeg = path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg"));
    // "center" shows the image at native size, so it must not be downscaled while decoding.
    let scaled = if is_jpeg && fit != "center" { decode_jpeg_scaled(path, (tw, th)) } else { None };
    let img = match scaled {
        Some(img) => img,
        None => image::ImageReader::open(path)
            .map_err(|e| e.to_string())?
            .with_guessed_format()
            .map_err(|e| e.to_string())?
            .decode()
            .map_err(|e| e.to_string())?,
    };
    let (iw, ih) = (img.width().max(1), img.height().max(1));
    let (tw, th) = (tw.max(1), th.max(1));
    let filter = FilterType::Triangle;
    let out = match fit {
        "stretch" => img.resize_exact(tw, th, filter),
        "contain" => img.resize(tw, th, filter),
        // Center: native size, cropped to the window if larger.
        "center" => {
            let (w, h) = (iw.min(tw), ih.min(th));
            img.crop_imm((iw - w) / 2, (ih - h) / 2, w, h)
        }
        // Cover: scale to fill, then crop the overflow so the texture is exactly window-sized.
        _ => img.resize_to_fill(tw, th, filter),
    };
    let rgba = out.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((rgba.into_raw(), w, h))
}
