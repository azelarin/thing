use std::{
    io,
    os::fd::{FromRawFd, OwnedFd},
    sync::{Arc, Mutex, MutexGuard},
};

pub(crate) const TEXT_MIME_TYPES: [&str; 3] =
    ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// what one offer advertised: its preferred text type and, bounded, every
/// mime type in the order the source offered them.
#[derive(Default)]
pub(crate) struct OfferData {
    choice: Mutex<Option<MimeChoice>>,
    mime_types: Mutex<Vec<String>>,
}

impl OfferData {
    pub(crate) fn offer(&self, mime_type: String) {
        {
            let mut mime_types = lock(&self.mime_types);
            if mime_types.len() < thing_runtime::MAX_CLIPBOARD_FORMATS
                && !mime_type.is_empty()
                && mime_type.len() <= thing_runtime::MAX_CLIPBOARD_FORMAT_BYTES
                && !mime_type.chars().any(char::is_control)
                && !mime_types.contains(&mime_type)
            {
                mime_types.push(mime_type.clone());
            }
        }
        let Some(priority) = mime_priority(&mime_type) else {
            return;
        };
        let mut selected = lock(&self.choice);
        if selected
            .as_ref()
            .is_none_or(|current| priority < current.priority)
        {
            *selected = Some(MimeChoice {
                priority,
                mime_type,
            });
        }
    }

    pub(crate) fn selected(&self) -> Option<String> {
        lock(&self.choice)
            .as_ref()
            .map(|choice| choice.mime_type.clone())
    }

    /// every advertised mime type, at most 64, in offer order.
    pub(crate) fn mime_types(&self) -> Vec<String> {
        lock(&self.mime_types).clone()
    }
}

struct MimeChoice {
    priority: u8,
    mime_type: String,
}

/// the mime type a png source offers.
pub(crate) const PNG_MIME_TYPE: &str = "image/png";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceKind {
    Text,
    Png,
}

pub(crate) struct SourceData {
    key: u64,
    bytes: Arc<[u8]>,
    kind: SourceKind,
}

impl SourceData {
    pub(crate) fn new(key: u64, bytes: Arc<[u8]>) -> Self {
        Self {
            key,
            bytes,
            kind: SourceKind::Text,
        }
    }

    pub(crate) fn png(key: u64, bytes: Arc<[u8]>) -> Self {
        Self {
            key,
            bytes,
            kind: SourceKind::Png,
        }
    }

    /// whether this source's bytes answer a request for `mime_type`.
    pub(crate) fn serves(&self, mime_type: &str) -> bool {
        match self.kind {
            SourceKind::Text => supports_mime_type(mime_type),
            SourceKind::Png => mime_type.trim().eq_ignore_ascii_case(PNG_MIME_TYPE),
        }
    }

    pub(crate) fn is_png(&self) -> bool {
        self.kind == SourceKind::Png
    }

    pub(crate) fn key(&self) -> u64 {
        self.key
    }

    pub(crate) fn bytes(&self) -> Arc<[u8]> {
        self.bytes.clone()
    }
}

pub(crate) fn supports_mime_type(mime_type: &str) -> bool {
    mime_priority(mime_type).is_some()
}

pub(crate) fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0_i32; 2];
    // SAFETY: `fds` points to storage for two descriptors. both returned
    // descriptors are wrapped in owning types immediately on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two new owned descriptors.
    let reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: pipe2 returned two new owned descriptors.
    let writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((reader, writer))
}

fn mime_priority(mime_type: &str) -> Option<u8> {
    let normalized = mime_type
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    match normalized.as_str() {
        "text/plain;charset=utf-8" | "text/plain;charset=utf8" => Some(0),
        "utf8_string" => Some(1),
        "text/plain" => Some(2),
        _ => None,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsFd, AsRawFd};

    #[test]
    fn text_mime_preference_is_stable() {
        let offer = OfferData::default();
        offer.offer("image/png".into());
        offer.offer("text/plain".into());
        offer.offer("UTF8_STRING".into());
        offer.offer("text/plain; charset=UTF-8".into());
        assert_eq!(
            offer.selected().as_deref(),
            Some("text/plain; charset=UTF-8")
        );
    }

    #[test]
    fn offers_record_every_format_once_within_bounds() {
        let offer = OfferData::default();
        for mime_type in ["image/png", "text/plain", "image/png", "", "bad\ntype"] {
            offer.offer(mime_type.into());
        }
        offer.offer("x".repeat(300));
        for index in 0..100 {
            offer.offer(format!("application/x-{index}"));
        }
        let formats = offer.mime_types();
        assert_eq!(formats.len(), 64);
        assert_eq!(&formats[..2], ["image/png", "text/plain"]);
        assert_eq!(offer.selected().as_deref(), Some("text/plain"));
        let text = SourceData::new(1, Arc::from(&b"t"[..]));
        let png = SourceData::png(2, Arc::from(&b"p"[..]));
        assert!(text.serves("UTF8_STRING") && !text.serves("image/png"));
        assert!(png.serves("IMAGE/PNG") && !png.serves("text/plain"));
    }

    #[test]
    fn text_pipe_is_blocking() {
        let (_, writer) = pipe().unwrap();
        // SAFETY: the writer owns a valid descriptor for the duration of the call.
        let flags = unsafe { libc::fcntl(writer.as_fd().as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(flags & libc::O_NONBLOCK, 0);
    }
}
