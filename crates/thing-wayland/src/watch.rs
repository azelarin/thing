//! file watching for configuration reloads.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    ffi::{CString, OsStr},
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::PathBuf,
};

pub(crate) struct Watch {
    fd: OwnedFd,
    directories: RefCell<BTreeMap<PathBuf, i32>>,
    files: RefCell<BTreeSet<PathBuf>>,
}

impl AsFd for Watch {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Watch {
    pub fn new() -> io::Result<Self> {
        // SAFETY: inotify_init1 takes only flags and returns a new owned fd.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            // SAFETY: fd is a successful, uniquely owned inotify descriptor.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            directories: RefCell::default(),
            files: RefCell::default(),
        })
    }

    pub fn set_files(&self, files: &[PathBuf]) -> io::Result<()> {
        // watch parent directories because atomic saves replace the file's inode.
        let desired: BTreeSet<_> = files
            .iter()
            .filter_map(|path| path.parent().map(PathBuf::from))
            .collect();
        let mut directories = self.directories.borrow_mut();
        for path in &desired {
            if directories.contains_key(path) {
                continue;
            }
            let name = CString::new(path.as_os_str().as_bytes())?;
            let mask = libc::IN_CLOSE_WRITE
                | libc::IN_MOVED_TO
                | libc::IN_CREATE
                | libc::IN_DELETE
                | libc::IN_DELETE_SELF
                | libc::IN_MOVE_SELF;
            // SAFETY: fd remains live; name is nul-terminated and valid for this call.
            let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), name.as_ptr(), mask) };
            if wd < 0 {
                return Err(io::Error::last_os_error());
            }
            directories.insert(path.clone(), wd);
        }
        directories.retain(|path, wd| {
            if desired.contains(path) {
                true
            } else {
                // SAFETY: the descriptor is live and wd was returned by add_watch.
                unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), *wd) };
                false
            }
        });
        *self.files.borrow_mut() = files.iter().cloned().collect();
        Ok(())
    }

    /// drain readiness without polling files. queue overflow requests a reload.
    pub fn changed(&self) -> io::Result<bool> {
        let mut changed = false;
        let mut buffer = [0_u8; 8192];
        loop {
            // SAFETY: buffer is writable for its full length; fd remains live.
            let count = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::WouldBlock => return Ok(changed),
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(error),
                }
            }
            if count == 0 {
                return Ok(changed);
            }
            let mut bytes = &buffer[..count as usize];
            while bytes.len() >= size_of::<libc::inotify_event>() {
                // SAFETY: enough initialized bytes remain for the header. the
                // byte buffer has no alignment guarantee, hence read_unaligned.
                let event = unsafe {
                    bytes
                        .as_ptr()
                        .cast::<libc::inotify_event>()
                        .read_unaligned()
                };
                let size = size_of::<libc::inotify_event>() + event.len as usize;
                if size > bytes.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "truncated inotify event",
                    ));
                }
                if event.mask & libc::IN_IGNORED != 0 {
                    self.directories
                        .borrow_mut()
                        .retain(|_, wd| *wd != event.wd);
                } else if event.mask
                    & (libc::IN_Q_OVERFLOW | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF)
                    != 0
                {
                    changed = true;
                } else {
                    let name = &bytes[size_of::<libc::inotify_event>()..size];
                    let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
                    let directories = self.directories.borrow();
                    if let Some((directory, _)) =
                        directories.iter().find(|(_, wd)| **wd == event.wd)
                    {
                        changed |= self
                            .files
                            .borrow()
                            .contains(&directory.join(OsStr::from_bytes(name)));
                    }
                }
                bytes = &bytes[size..];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observes_atomic_saves_and_import_changes_without_idle_events() {
        let directory = tempfile::tempdir().unwrap();
        let panel = directory.path().join("panel.luau");
        let theme = directory.path().join("theme.luau");
        std::fs::write(&panel, "before").unwrap();
        let watch = Watch::new().unwrap();
        watch.set_files(&[panel.clone(), theme.clone()]).unwrap();
        assert!(!watch.changed().unwrap());
        let replacement = directory.path().join("temporary");
        std::fs::write(&replacement, "after").unwrap();
        std::fs::rename(&replacement, &panel).unwrap();
        assert!(watch.changed().unwrap());
        assert!(!watch.changed().unwrap());
        std::fs::write(&theme, "return {}").unwrap();
        assert!(watch.changed().unwrap());
        std::fs::write(directory.path().join("unrelated.txt"), "ignore me").unwrap();
        assert!(!watch.changed().unwrap());
        std::fs::write(directory.path().join("unrelated.luau"), "return {}").unwrap();
        assert!(!watch.changed().unwrap());
    }
}
