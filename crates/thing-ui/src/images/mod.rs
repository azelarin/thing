//! bounded asynchronous image loading and gpu caching.

mod icons;

use std::{
    collections::{BTreeMap, VecDeque},
    io::{Cursor, Read},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    thread,
};

use femtovg::{Canvas, ImageFlags, ImageId, ImageSource, renderer::Renderer, rgb::FromSlice};
use thing_core::{Image, Rect};

const CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_INPUT: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    image: Image,
    width: u32,
    height: u32,
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

    pub fn get(&mut self, image: &Image, bounds: Rect, scale: f32) -> Option<(ImageId, u32, u32)> {
        if image.source.is_none() && image.icon.is_none() && image.app_id.is_none() {
            return None;
        }
        if bounds.width <= 0.0 || bounds.height <= 0.0 {
            return None;
        }
        let key = Key {
            image: image.clone(),
            width: (bounds.width * scale).ceil().clamp(1.0, 4096.0) as u32,
            height: (bounds.height * scale).ceil().clamp(1.0, 4096.0) as u32,
        };
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
    let path = icons.find(
        &key.image,
        key.width.max(key.height).min(u16::MAX as u32) as u16,
    )?;
    let file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
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
    decode_bytes(
        &bytes,
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("svg")),
        key.width,
        key.height,
    )
}

fn decode_bytes(bytes: &[u8], svg: bool, width: u32, height: u32) -> Result<Pixels, String> {
    if svg {
        let options = resvg::usvg::Options {
            image_href_resolver: resvg::usvg::ImageHrefResolver {
                resolve_data: Box::new(|_, _, _| None),
                resolve_string: Box::new(|_, _| None),
            },
            ..Default::default()
        };
        let tree = resvg::usvg::Tree::from_data(bytes, &options).map_err(|e| e.to_string())?;
        let factor = (width as f32 / tree.size().width()).min(height as f32 / tree.size().height());
        let width = (tree.size().width() * factor)
            .round()
            .clamp(1.0, width as f32) as u32;
        let height = (tree.size().height() * factor)
            .round()
            .clamp(1.0, height as f32) as u32;
        let mut pixmap =
            resvg::tiny_skia::Pixmap::new(width, height).ok_or("invalid SVG dimensions")?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::from_scale(factor, factor),
            &mut pixmap.as_mut(),
        );
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
        let image = decoded.thumbnail(width, height).into_rgba8();
        Ok(Pixels {
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
            premultiplied: false,
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use femtovg::{PixelFormat, renderer::Void};

    #[test]
    fn eviction_keeps_retired_bytes_resident_until_release() {
        let mut canvas = Canvas::new(Void).unwrap();
        let id = canvas
            .create_image_empty(2, 2, PixelFormat::Rgba8, ImageFlags::empty())
            .unwrap();
        let key = Key {
            image: Image::default(),
            width: 2,
            height: 2,
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
        let pixels = decode_bytes(br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><path fill="#ff0000" fill-opacity=".5" d="M0 0h20v10H0z"/></svg>"##, true, 40, 40).unwrap();
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
        let pixels = decode_bytes(encoded.get_ref(), false, 10, 10).unwrap();
        assert_eq!((pixels.width, pixels.height), (10, 5));
        assert!(!pixels.premultiplied);
        assert_eq!(&pixels.rgba[..4], &[255, 0, 0, 128]);
        assert!(decode_bytes(b"broken", false, 10, 10).is_err());
        assert!(decode_bytes(b"<svg>", true, 10, 10).is_err());
    }
}
