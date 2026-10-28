//! bounded asynchronous image loading and gpu caching.

mod icons;

use std::{
    collections::{BTreeMap, VecDeque},
    io::{Cursor, Read},
    num::NonZeroU64,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
};

use femtovg::{Canvas, ImageFlags, ImageId, ImageSource, renderer::Renderer, rgb::FromSlice};
use thing_core::{Image, ImageFit, Rect};

const CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_INPUT: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    image: Image,
    fit: ImageFit,
    width: u32,
    height: u32,
    cover_aspect_bits: Option<NonZeroU64>,
}

impl Key {
    fn viewport_aspect(&self) -> f64 {
        self.cover_aspect_bits
            .map(|bits| f64::from_bits(bits.get()))
            .unwrap_or_else(|| f64::from(self.width) / f64::from(self.height))
    }
}

struct Pixels {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    premultiplied: bool,
}

#[derive(Default)]
struct Inbox {
    requests: VecDeque<Key>,
    result: Option<(Key, Result<Pixels, String>)>,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    inbox: Mutex<Inbox>,
    changed: Condvar,
}

enum State {
    Pending,
    Ready {
        id: ImageId,
        width: u32,
        height: u32,
    },
    Failed,
}

struct Entry {
    state: State,
    touched: u64,
}

struct Retired {
    id: ImageId,
    bytes: usize,
}

pub(super) struct Images {
    shared: Arc<Shared>,
    cache: BTreeMap<Key, Entry>,
    retired: Vec<Retired>,
    bytes: usize,
    retired_bytes: usize,
    tick: u64,
}

impl Images {
    pub fn new(wake: impl Fn() + Send + 'static) -> Result<Self, String> {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        thread::Builder::new()
            .name("thing-images".into())
            .spawn(move || {
                let mut icons = icons::Icons::default();
                loop {
                    let key = {
                        let mut inbox = lock(&worker.inbox);
                        while !inbox.stop && (inbox.result.is_some() || inbox.requests.is_empty()) {
                            inbox = worker
                                .changed
                                .wait(inbox)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                        if inbox.stop {
                            break;
                        }
                        inbox.requests.pop_front().expect("waited for a request")
                    };
                    let result = decode(&key, &mut icons);
                    {
                        let mut inbox = lock(&worker.inbox);
                        if inbox.stop {
                            break;
                        }
                        inbox.result = Some((key, result));
                    }
                    wake();
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            shared,
            cache: BTreeMap::new(),
            retired: Vec::new(),
            bytes: 0,
            retired_bytes: 0,
            tick: 0,
        })
    }

    pub fn collect<R: Renderer>(&mut self, canvas: &mut Canvas<R>) {
        let result = lock(&self.shared.inbox).result.take();
        let Some((key, result)) = result else {
            return;
        };
        self.shared.changed.notify_one();
        if !self.cache.contains_key(&key) {
            return;
        }
        let state = match result.and_then(|pixels| {
            while self.resident_bytes().saturating_add(pixels.rgba.len()) > CACHE_BYTES {
                if !self.evict(Some(&key)) {
                    return Err("image exceeds GPU cache budget".into());
                }
                // collect runs before this frame records drawing commands, so
                // images evicted here can be released immediately.
                self.release(canvas);
            }
            let source = ImageSource::Rgba(femtovg::imgref::Img::new(
                pixels.rgba.as_rgba(),
                pixels.width as usize,
                pixels.height as usize,
            ));
            let flags = if pixels.premultiplied {
                ImageFlags::PREMULTIPLIED
            } else {
                ImageFlags::empty()
            };
            let id = canvas
                .create_image(source, flags)
                .map_err(|e| e.to_string())?;
            self.bytes += pixels.rgba.len();
            debug_assert!(self.resident_bytes() <= CACHE_BYTES);
            Ok(State::Ready {
                id,
                width: pixels.width,
                height: pixels.height,
            })
        }) {
            Ok(state) => state,
            Err(error) => {
                eprintln!("thing: cannot load image {:?}: {error}", key.image);
                State::Failed
            }
        };
        if let Some(entry) = self.cache.get_mut(&key) {
            entry.state = state;
        }
    }

    pub fn get(
        &mut self,
        image: &Image,
        fit: ImageFit,
        bounds: Rect,
        scale: f32,
    ) -> Option<(ImageId, u32, u32)> {
        if image.source.is_none()
            && image.memory.is_none()
            && image.icon.is_none()
            && image.app_id.is_none()
        {
            return None;
        }
        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return None;
        }
        let key = request_key(image, fit, bounds, scale);
        self.tick = self.tick.saturating_add(1);
        if let Some(entry) = self.cache.get_mut(&key) {
            entry.touched = self.tick;
            return match entry.state {
                State::Ready { id, width, height } => Some((id, width, height)),
                _ => None,
            };
        }
        if lock(&self.shared.inbox).requests.len() >= 32 {
            return None;
        }
        while self.cache.len() >= 128 {
            if !self.evict(None) {
                return None;
            }
        }
        self.cache.insert(
            key.clone(),
            Entry {
                state: State::Pending,
                touched: self.tick,
            },
        );
        lock(&self.shared.inbox).requests.push_back(key);
        self.shared.changed.notify_one();
        None
    }

    fn evict(&mut self, except: Option<&Key>) -> bool {
        let key = self
            .cache
            .iter()
            .filter(|(key, entry)| Some(*key) != except && !matches!(entry.state, State::Pending))
            .min_by_key(|(_, entry)| entry.touched)
            .map(|(key, _)| key.clone());
        let Some(key) = key else {
            return false;
        };
        if let Some(entry) = self.cache.remove(&key)
            && let State::Ready { id, width, height } = entry.state
        {
            // queued draw commands may still refer to this image. the painter
            // deletes it after submitting the frame.
            let bytes = width as usize * height as usize * 4;
            self.retired.push(Retired { id, bytes });
            self.bytes -= bytes;
            self.retired_bytes += bytes;
        }
        debug_assert!(self.resident_bytes() <= CACHE_BYTES);
        true
    }

    pub fn release<R: Renderer>(&mut self, canvas: &mut Canvas<R>) {
        for retired in self.retired.drain(..) {
            canvas.delete_image(retired.id);
            self.retired_bytes -= retired.bytes;
        }
        debug_assert_eq!(self.retired_bytes, 0);
    }

    fn resident_bytes(&self) -> usize {
        self.bytes.saturating_add(self.retired_bytes)
    }
}

fn request_dimensions(bounds: Rect, scale: f32) -> (u32, u32) {
    (
        (bounds.width * scale).ceil().clamp(1.0, 4096.0) as u32,
        (bounds.height * scale).ceil().clamp(1.0, 4096.0) as u32,
    )
}

fn request_key(image: &Image, fit: ImageFit, bounds: Rect, scale: f32) -> Key {
    let (width, height) = request_dimensions(bounds, scale);
    let cover_aspect_bits = (fit == ImageFit::Cover).then(|| {
        let aspect = f64::from(bounds.width) / f64::from(bounds.height);
        debug_assert!(aspect.is_finite() && aspect > 0.0);
        NonZeroU64::new(aspect.to_bits()).expect("a positive aspect has nonzero bits")
    });
    Key {
        image: image.clone(),
        fit,
        width,
        height,
        cover_aspect_bits,
    }
}

impl Drop for Images {
    fn drop(&mut self) {
        let mut inbox = lock(&self.shared.inbox);
        inbox.stop = true;
        inbox.requests.clear();
        inbox.result.take();
        self.shared.changed.notify_one();
        // decoders cannot be interrupted safely. the worker owns no gl objects;
        // it finishes at most its current file and exits without delaying the ui.
    }
}

fn decode(key: &Key, icons: &mut icons::Icons) -> Result<Pixels, String> {
    let requested = key.width.max(key.height).min(u16::MAX as u32) as u16;
    if let Some(path) = key.image.source.as_deref() {
        return decode_path(std::path::Path::new(path), key);
    }
    let mut last_error = None;
    if let Some(source) = &key.image.memory {
        for candidate in source.candidates() {
            if let Some(name) = candidate.icon() {
                match icons::Icons::find_name(name, &key.image.theme, requested)
                    .and_then(|path| decode_path(&path, key))
                {
                    Ok(pixels) => return Ok(pixels),
                    Err(error) => last_error = Some(error),
                }
            }
            if let Some(pixmap) = select_pixmap(candidate.pixmaps(), key.width, key.height) {
                return decode_pixmap(pixmap, key);
            }
        }
    }
    match icons
        .find(&key.image, requested)
        .and_then(|path| decode_path(&path, key))
    {
        Ok(pixels) => Ok(pixels),
        Err(error) => Err(last_error.unwrap_or(error)),
    }
}

fn decode_path(path: &std::path::Path, key: &Key) -> Result<Pixels, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("image source is not a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_INPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_INPUT {
        return Err("image file exceeds 16 MiB".into());
    }
    decode_bytes_with_aspect(
        &bytes,
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("svg")),
        key.width,
        key.height,
        key.fit,
        key.viewport_aspect(),
    )
}

fn select_pixmap(
    pixmaps: &[thing_core::ImagePixmap],
    width: u32,
    height: u32,
) -> Option<&thing_core::ImagePixmap> {
    pixmaps
        .iter()
        .filter(|pixmap| pixmap.width() >= width && pixmap.height() >= height)
        .min_by_key(|pixmap| u64::from(pixmap.width()) * u64::from(pixmap.height()))
        .or_else(|| {
            pixmaps
                .iter()
                .max_by_key(|pixmap| u64::from(pixmap.width()) * u64::from(pixmap.height()))
        })
}

fn decode_pixmap(pixmap: &thing_core::ImagePixmap, key: &Key) -> Result<Pixels, String> {
    let image = image::RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixmap.rgba().to_vec())
        .ok_or("invalid in-memory image dimensions")?;
    resize_raster(
        image::DynamicImage::ImageRgba8(image),
        key.width,
        key.height,
        key.fit,
        key.viewport_aspect(),
    )
}

#[cfg(test)]
fn decode_bytes(
    bytes: &[u8],
    svg: bool,
    width: u32,
    height: u32,
    fit: ImageFit,
) -> Result<Pixels, String> {
    decode_bytes_with_aspect(
        bytes,
        svg,
        width,
        height,
        fit,
        f64::from(width) / f64::from(height),
    )
}

fn decode_bytes_with_aspect(
    bytes: &[u8],
    svg: bool,
    width: u32,
    height: u32,
    fit: ImageFit,
    viewport_aspect: f64,
) -> Result<Pixels, String> {
    if svg {
        let options = resvg::usvg::Options {
            image_href_resolver: resvg::usvg::ImageHrefResolver {
                resolve_data: Box::new(|_, _, _| None),
                resolve_string: Box::new(|_, _| None),
            },
            ..Default::default()
        };
        let tree = resvg::usvg::Tree::from_data(bytes, &options).map_err(|e| e.to_string())?;
        let source_width = tree.size().width();
        let source_height = tree.size().height();
        let (width, height, transform) = match fit {
            ImageFit::Contain => {
                let factor = (width as f32 / source_width).min(height as f32 / source_height);
                let width = (source_width * factor).round().clamp(1.0, width as f32) as u32;
                let height = (source_height * factor).round().clamp(1.0, height as f32) as u32;
                (
                    width,
                    height,
                    resvg::tiny_skia::Transform::from_scale(factor, factor),
                )
            }
            ImageFit::Cover => (
                width,
                height,
                svg_cover_transform(source_width, source_height, width, height, viewport_aspect)?,
            ),
            ImageFit::Fill => (
                width,
                height,
                resvg::tiny_skia::Transform::from_scale(
                    width as f32 / source_width,
                    height as f32 / source_height,
                ),
            ),
        };
        let mut pixmap =
            resvg::tiny_skia::Pixmap::new(width, height).ok_or("invalid SVG dimensions")?;
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        Ok(Pixels {
            width,
            height,
            rgba: pixmap.take(),
            premultiplied: true,
        })
    } else {
        let mut reader = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(CACHE_BYTES as u64);
        reader.limits(limits);
        let decoded = reader.decode().map_err(|e| e.to_string())?;
        resize_raster(decoded, width, height, fit, viewport_aspect)
    }
}

fn resize_raster(
    decoded: image::DynamicImage,
    width: u32,
    height: u32,
    fit: ImageFit,
    viewport_aspect: f64,
) -> Result<Pixels, String> {
    let image = match fit {
        ImageFit::Contain => decoded.thumbnail(width, height).into_rgba8(),
        ImageFit::Cover => {
            let (x, y, crop_width, crop_height) =
                cover_crop(decoded.width(), decoded.height(), viewport_aspect);
            decoded
                .crop_imm(x, y, crop_width, crop_height)
                .resize_exact(width, height, image::imageops::FilterType::Triangle)
                .into_rgba8()
        }
        ImageFit::Fill => decoded
            .resize_exact(width, height, image::imageops::FilterType::Triangle)
            .into_rgba8(),
    };
    Ok(Pixels {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
        premultiplied: false,
    })
}

fn svg_cover_transform(
    source_width: f32,
    source_height: f32,
    target_width: u32,
    target_height: u32,
    viewport_aspect: f64,
) -> Result<resvg::tiny_skia::Transform, String> {
    let source_width = f64::from(source_width);
    let source_height = f64::from(source_height);
    let source_aspect = source_width / source_height;
    let (x, y, width, height) = if source_aspect > viewport_aspect {
        let width = source_height * viewport_aspect;
        ((source_width - width) / 2.0, 0.0, width, source_height)
    } else if source_aspect < viewport_aspect {
        let height = source_width / viewport_aspect;
        (0.0, (source_height - height) / 2.0, source_width, height)
    } else {
        (0.0, 0.0, source_width, source_height)
    };
    let scale_x = f64::from(target_width) / width;
    let scale_y = f64::from(target_height) / height;
    let translate_x = -x * scale_x;
    let translate_y = -y * scale_y;
    let values = [scale_x, scale_y, translate_x, translate_y];
    if values
        .into_iter()
        .any(|value| !value.is_finite() || value.abs() > f64::from(f32::MAX))
    {
        return Err("SVG cover transform exceeds the supported range".into());
    }
    Ok(resvg::tiny_skia::Transform::from_row(
        scale_x as f32,
        0.0,
        0.0,
        scale_y as f32,
        translate_x as f32,
        translate_y as f32,
    ))
}

fn cover_crop(source_width: u32, source_height: u32, viewport_aspect: f64) -> (u32, u32, u32, u32) {
    debug_assert!(source_width > 0 && source_height > 0);
    debug_assert!(viewport_aspect.is_finite() && viewport_aspect > 0.0);
    let source_aspect = f64::from(source_width) / f64::from(source_height);
    if source_aspect > viewport_aspect {
        let crop_width = (f64::from(source_height) * viewport_aspect)
            .round()
            .clamp(1.0, f64::from(source_width)) as u32;
        (
            (source_width - crop_width) / 2,
            0,
            crop_width,
            source_height,
        )
    } else if source_aspect < viewport_aspect {
        let crop_height = (f64::from(source_width) / viewport_aspect)
            .round()
            .clamp(1.0, f64::from(source_height)) as u32;
        (
            0,
            (source_height - crop_height) / 2,
            source_width,
            crop_height,
        )
    } else {
        (0, 0, source_width, source_height)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use femtovg::{PixelFormat, renderer::Void};
    use thing_core::{ImageCandidate, ImagePixmap, ImageSource};

    #[test]
    fn memory_sources_fall_back_from_names_and_choose_a_suitable_resolution() {
        let attention = ImageCandidate::new(
            Some("thing-missing-attention-icon-9d52f20f".into()),
            Vec::new(),
        )
        .unwrap();
        let normal = ImageCandidate::new(
            Some("thing-missing-normal-icon-9d52f20f".into()),
            vec![
                ImagePixmap::new(16, 16, vec![255; 16 * 16 * 4]).unwrap(),
                ImagePixmap::new(32, 32, [10, 20, 30, 128].repeat(32 * 32)).unwrap(),
            ],
        )
        .unwrap();
        let image = Image {
            memory: Some(ImageSource::new("tray1-image1".into(), vec![attention, normal]).unwrap()),
            theme: "Adwaita".into(),
            ..Image::default()
        };
        let key = Key {
            image,
            fit: ImageFit::Fill,
            width: 24,
            height: 24,
            cover_aspect_bits: None,
        };

        let pixels = decode(&key, &mut icons::Icons::default()).unwrap();
        assert_eq!((pixels.width, pixels.height), (24, 24));
        assert_eq!(&pixels.rgba[..4], &[10, 20, 30, 128]);
        assert!(!pixels.premultiplied);
    }

    #[test]
    fn eviction_keeps_retired_bytes_resident_until_release() {
        let mut canvas = Canvas::new(Void).unwrap();
        let id = canvas
            .create_image_empty(2, 2, PixelFormat::Rgba8, ImageFlags::empty())
            .unwrap();
        let key = Key {
            image: Image::default(),
            fit: ImageFit::Contain,
            width: 2,
            height: 2,
            cover_aspect_bits: None,
        };
        let mut cache = BTreeMap::new();
        cache.insert(
            key,
            Entry {
                state: State::Ready {
                    id,
                    width: 2,
                    height: 2,
                },
                touched: 0,
            },
        );
        let mut images = Images {
            shared: Arc::new(Shared::default()),
            cache,
            retired: Vec::new(),
            bytes: 16,
            retired_bytes: 0,
            tick: 0,
        };

        assert!(images.evict(None));
        assert_eq!(images.bytes, 0);
        assert_eq!(images.retired_bytes, 16);
        assert_eq!(images.resident_bytes(), 16);
        assert!(canvas.image_info(id).is_ok());

        images.release(&mut canvas);
        assert_eq!(images.resident_bytes(), 0);
        assert!(canvas.image_info(id).is_err());
    }

    #[test]
    fn svg_uses_requested_size_preserves_aspect_ratio_and_premultiplies_alpha() {
        let pixels = decode_bytes(br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><path fill="#ff0000" fill-opacity=".5" d="M0 0h20v10H0z"/></svg>"##, true, 40, 40, ImageFit::Contain).unwrap();
        assert_eq!((pixels.width, pixels.height), (40, 20));
        assert!(pixels.premultiplied);
        assert_eq!(&pixels.rgba[..4], &[128, 0, 0, 128]);
    }

    #[test]
    fn raster_images_resize_and_invalid_files_fail_without_panicking() {
        let source = image::RgbaImage::from_pixel(20, 10, image::Rgba([255, 0, 0, 128]));
        let mut encoded = Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let pixels = decode_bytes(encoded.get_ref(), false, 10, 10, ImageFit::Contain).unwrap();
        assert_eq!((pixels.width, pixels.height), (10, 5));
        assert!(!pixels.premultiplied);
        assert_eq!(&pixels.rgba[..4], &[255, 0, 0, 128]);
        assert!(decode_bytes(b"broken", false, 10, 10, ImageFit::Contain).is_err());
        assert!(decode_bytes(b"<svg>", true, 10, 10, ImageFit::Contain).is_err());
    }

    #[test]
    fn cover_crops_before_resampling_raster_images() {
        let source = image::RgbaImage::from_fn(1000, 100, |x, _| {
            if x < 450 {
                image::Rgba([255, 0, 0, 255])
            } else if x < 550 {
                image::Rgba([0, 255, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 255])
            }
        });
        let mut encoded = Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();

        let contain = decode_bytes(encoded.get_ref(), false, 100, 100, ImageFit::Contain).unwrap();
        let cover = decode_bytes(encoded.get_ref(), false, 100, 100, ImageFit::Cover).unwrap();
        let fill = decode_bytes(encoded.get_ref(), false, 100, 100, ImageFit::Fill).unwrap();

        assert_eq!((contain.width, contain.height), (100, 10));
        assert_eq!((cover.width, cover.height), (100, 100));
        assert_eq!(&cover.rgba[..4], &[0, 255, 0, 255]);
        assert_eq!((fill.width, fill.height), (100, 100));
        assert_eq!(&fill.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(cover_crop(1000, 100, 1.0), (450, 0, 100, 100));
        assert_eq!(cover_crop(8192, 1, 1.0), (4095, 0, 1, 1));
        let (x, y, width, height) = cover_crop(101, 50, 64.0 / 37.0);
        assert_eq!((y, height), (0, 50));
        assert!((2 * x + width).abs_diff(101) <= 1);
        let (x, y, width, height) = cover_crop(50, 101, 37.0 / 64.0);
        assert_eq!((x, width), (0, 50));
        assert!((2 * y + height).abs_diff(101) <= 1);
    }

    #[test]
    fn cover_and_fill_render_svg_at_the_requested_bounds() {
        let source = br##"<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="100"><path fill="#f00" d="M0 0h450v100H0z"/><path fill="#0f0" d="M450 0h100v100H450z"/><path fill="#00f" d="M550 0h450v100H550z"/></svg>"##;
        let contain = decode_bytes(source, true, 100, 100, ImageFit::Contain).unwrap();
        let cover = decode_bytes(source, true, 100, 100, ImageFit::Cover).unwrap();
        let fill = decode_bytes(source, true, 100, 100, ImageFit::Fill).unwrap();

        assert_eq!((contain.width, contain.height), (100, 10));
        assert_eq!((cover.width, cover.height), (100, 100));
        assert_eq!(&cover.rgba[..4], &[0, 255, 0, 255]);
        assert_eq!((fill.width, fill.height), (100, 100));
        assert_eq!(&fill.rgba[..4], &[255, 0, 0, 255]);

        let transform = svg_cover_transform(101.0, 50.0, 64, 37, 64.0 / 37.0).unwrap();
        assert!((transform.tx + 101.0 * transform.sx / 2.0 - 32.0).abs() < 0.0001);
        assert!((transform.ty + 50.0 * transform.sy / 2.0 - 18.5).abs() < 0.0001);
    }

    #[test]
    fn capped_raster_cover_preserves_an_oversized_same_ratio_source() {
        let source = image::RgbaImage::from_fn(1000, 100, |x, _| {
            if x < 100 {
                image::Rgba([255, 0, 0, 255])
            } else if x < 900 {
                image::Rgba([0, 255, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 255])
            }
        });
        let mut encoded = Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bounds = Rect {
            width: 10_000.0,
            height: 1_000.0,
            ..Rect::default()
        };
        let request = request_key(&Image::default(), ImageFit::Cover, bounds, 1.0);
        assert_eq!((request.width, request.height), (4096, 1000));

        let cover = decode_bytes_with_aspect(
            encoded.get_ref(),
            false,
            request.width,
            request.height,
            ImageFit::Cover,
            request.viewport_aspect(),
        )
        .unwrap();
        assert_eq!(&cover.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(
            &cover.rgba[(cover.width as usize - 1) * 4..cover.width as usize * 4],
            &[0, 0, 255, 255]
        );
    }

    #[test]
    fn capped_svg_cover_preserves_an_oversized_same_ratio_source() {
        let source = br##"<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="100"><path fill="#f00" d="M0 0h100v100H0z"/><path fill="#0f0" d="M100 0h800v100H100z"/><path fill="#00f" d="M900 0h100v100H900z"/></svg>"##;
        let bounds = Rect {
            width: 10_000.0,
            height: 1_000.0,
            ..Rect::default()
        };
        let request = request_key(&Image::default(), ImageFit::Cover, bounds, 1.0);

        let cover = decode_bytes_with_aspect(
            source,
            true,
            request.width,
            request.height,
            ImageFit::Cover,
            request.viewport_aspect(),
        )
        .unwrap();
        assert_eq!(&cover.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(
            &cover.rgba[(cover.width as usize - 1) * 4..cover.width as usize * 4],
            &[0, 0, 255, 255]
        );
    }

    #[test]
    fn image_requests_round_fractional_output_sizes_up_within_cache_bounds() {
        let bounds = Rect {
            width: 100.1,
            height: 40.1,
            ..Rect::default()
        };
        assert_eq!(request_dimensions(bounds, 1.25), (126, 51));
        assert_eq!(
            request_dimensions(
                Rect {
                    width: 10_000.0,
                    height: 0.1,
                    ..Rect::default()
                },
                2.0,
            ),
            (4096, 1)
        );
    }

    #[test]
    fn cover_cache_identity_keeps_the_uncapped_viewport_aspect() {
        let image = Image::default();
        let wide = request_key(
            &image,
            ImageFit::Cover,
            Rect {
                width: 10_000.0,
                height: 1_000.0,
                ..Rect::default()
            },
            1.0,
        );
        let capped_ratio = request_key(
            &image,
            ImageFit::Cover,
            Rect {
                width: 4_096.0,
                height: 1_000.0,
                ..Rect::default()
            },
            1.0,
        );

        assert_eq!((wide.width, wide.height), (4096, 1000));
        assert_eq!((capped_ratio.width, capped_ratio.height), (4096, 1000));
        assert_ne!(wide.cover_aspect_bits, capped_ratio.cover_aspect_bits);
        assert_ne!(wide, capped_ratio);
    }

    #[test]
    fn subpixel_cover_uses_the_logical_aspect_after_allocation_rounds_to_one_pixel() {
        let request = request_key(
            &Image::default(),
            ImageFit::Cover,
            Rect {
                width: 10_000.0,
                height: 0.1,
                ..Rect::default()
            },
            1.0,
        );
        assert_eq!((request.width, request.height), (4096, 1));
        assert_eq!(
            cover_crop(8192, 100, request.viewport_aspect()),
            (0, 49, 8192, 1)
        );
        assert_eq!(cover_crop(8192, 100, 4096.0), (0, 49, 8192, 2));

        let transform = svg_cover_transform(
            8192.0,
            100.0,
            request.width,
            request.height,
            request.viewport_aspect(),
        )
        .unwrap();
        assert!(
            [transform.sx, transform.sy, transform.tx, transform.ty]
                .into_iter()
                .all(f32::is_finite)
        );
    }

    #[test]
    fn svg_cover_rejects_a_transform_that_cannot_be_represented_finitely() {
        let request = request_key(
            &Image::default(),
            ImageFit::Cover,
            Rect {
                width: 100_000.0,
                height: f32::from_bits(1),
                ..Rect::default()
            },
            1.0,
        );
        let error = svg_cover_transform(
            1000.0,
            100.0,
            request.width,
            request.height,
            request.viewport_aspect(),
        )
        .unwrap_err();
        assert!(error.contains("supported range"), "{error}");
    }
}
