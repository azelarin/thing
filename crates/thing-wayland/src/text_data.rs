use std::{
    io,
    os::fd::{FromRawFd, OwnedFd},
    sync::{Arc, Mutex, MutexGuard},
};

pub(crate) const TEXT_MIME_TYPES: [&str; 3] =
    ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

pub(crate) struct OfferData(Mutex<Option<MimeChoice>>);

impl Default for OfferData {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl OfferData {
    pub(crate) fn offer(&self, mime_type: String) {
        let Some(priority) = mime_priority(&mime_type) else {
            return;
        };
        let mut selected = lock(&self.0);
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
        lock(&self.0)
            .as_ref()
            .map(|choice| choice.mime_type.clone())
    }
}

struct MimeChoice {
    priority: u8,
    mime_type: String,
}

pub(crate) struct SourceData {
    key: u64,
    bytes: Arc<[u8]>,
}

impl SourceData {
    pub(crate) fn new(key: u64, bytes: Arc<[u8]>) -> Self {
        Self { key, bytes }
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
    fn text_pipe_is_blocking() {
        let (_, writer) = pipe().unwrap();
        // SAFETY: the writer owns a valid descriptor for the duration of the call.
        let flags = unsafe { libc::fcntl(writer.as_fd().as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(flags & libc::O_NONBLOCK, 0);
    }
}
