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
    path::{Path, PathBuf},
};

const WATCH_MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF;

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
        let files: BTreeSet<_> = files.iter().cloned().collect();
        self.reconcile(&files)?;
        *self.files.borrow_mut() = files;
        Ok(())
    }

    fn reconcile(&self, files: &BTreeSet<PathBuf>) -> io::Result<()> {
        self.reconcile_after_watch(files, |_| {})
    }

    fn reconcile_after_watch(
        &self,
        files: &BTreeSet<PathBuf>,
        mut after_watch: impl FnMut(&Path),
    ) -> io::Result<()> {
        // watch parent directories because atomic saves replace the file's inode.
        // while a parent is missing, its nearest existing ancestor observes the
        // path being recreated without polling.
        let desired: BTreeSet<_> = files
            .iter()
            .filter_map(|path| path.parent().map(PathBuf::from))
            .collect();
        let existing: BTreeSet<_> = self.directories.borrow().values().copied().collect();
        let mut installed = BTreeSet::new();
        let mut next = BTreeMap::new();
        let result = desired.iter().try_for_each(|path| -> io::Result<()> {
            let (path, wd) = self.add_stable(path, &mut installed, &mut after_watch)?;
            next.insert(path, wd);
            Ok(())
        });
        if let Err(error) = result {
            // existing descriptors still belong to the active watch set. only
            // discard descriptors installed during the failed reconciliation.
            for wd in installed.difference(&existing) {
                self.remove_watch(*wd);
            }
            return Err(error);
        }

        let wanted: BTreeSet<_> = next.values().copied().collect();
        let known: BTreeSet<_> = existing.union(&installed).copied().collect();
        for wd in known.difference(&wanted) {
            self.remove_watch(*wd);
        }
        *self.directories.borrow_mut() = next;
        Ok(())
    }

    fn add_stable(
        &self,
        desired: &Path,
        installed: &mut BTreeSet<i32>,
        after_watch: &mut impl FnMut(&Path),
    ) -> io::Result<(PathBuf, i32)> {
        let mut path = self.nearest_directory(desired)?;
        loop {
            let name = CString::new(path.as_os_str().as_bytes())?;
            // SAFETY: fd remains live; name is nul-terminated and valid for this call.
            let wd =
                unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), name.as_ptr(), WATCH_MASK) };
            if wd >= 0 {
                installed.insert(wd);
                after_watch(&path);
                // recheck after installation so a directory created during the
                // handoff is covered before its fallback watch is removed.
                let current = self.nearest_directory(desired)?;
                if current == path {
                    return Ok((path, wd));
                }
                path = current;
                continue;
            }
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) {
                return Err(error);
            }
            path = self.nearest_directory(desired)?;
        }
    }

    fn nearest_directory(&self, desired: &Path) -> io::Result<PathBuf> {
        let mut path = desired;
        loop {
            match std::fs::metadata(path) {
                Ok(metadata) if metadata.is_dir() => return Ok(path.to_owned()),
                Ok(_) => {}
                Err(error)
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Err(error);
                }
                Err(_) => {}
            }
            path = path
                .parent()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no watchable ancestor"))?;
        }
    }

    fn remove_watch(&self, wd: i32) {
        // SAFETY: fd remains live. wd came from inotify_add_watch; it may already
        // have been removed by the kernel, in which case the error is harmless.
        unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
    }

    fn forget_watch(&self, wd: i32) -> bool {
        let before = self.directories.borrow().len();
        self.directories
            .borrow_mut()
            .retain(|_, value| *value != wd);
        self.directories.borrow().len() != before
    }

    fn forget_all(&self) {
        let watches = std::mem::take(&mut *self.directories.borrow_mut());
        for wd in watches.values().copied().collect::<BTreeSet<_>>() {
            self.remove_watch(wd);
        }
    }

    fn affects_watched_directory(&self, directory: &Path, name: &OsStr) -> bool {
        let child = directory.join(name);
        self.files
            .borrow()
            .iter()
            .filter_map(|path| path.parent())
            .any(|desired| desired == child || desired.starts_with(&child))
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
            let mut reconcile = false;
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
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    self.forget_all();
                    changed = true;
                    reconcile = true;
                } else if event.mask & libc::IN_IGNORED != 0 {
                    if self.forget_watch(event.wd) {
                        changed = true;
                        reconcile = true;
                    }
                } else if event.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0 {
                    if self.forget_watch(event.wd) {
                        self.remove_watch(event.wd);
                    }
                    changed = true;
                    reconcile = true;
                } else {
                    let name = &bytes[size_of::<libc::inotify_event>()..size];
                    let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
                    let name = OsStr::from_bytes(name);
                    let directory = self
                        .directories
                        .borrow()
                        .iter()
                        .find_map(|(path, wd)| (*wd == event.wd).then(|| path.clone()));
                    if let Some(directory) = directory {
                        changed |= self.files.borrow().contains(&directory.join(name));
                        if self.affects_watched_directory(&directory, name) {
                            changed = true;
                            reconcile = true;
                        }
                    }
                }
                bytes = &bytes[size..];
            }
            if reconcile {
                self.reconcile(&self.files.borrow())?;
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

    #[test]
    fn follows_a_configuration_directory_replaced_after_a_gap() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let old = root.path().join("config-old");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("panel.luau"), "before").unwrap();
        let panel = directory.join("panel.luau").canonicalize().unwrap();
        let watch = Watch::new().unwrap();
        watch.set_files(std::slice::from_ref(&panel)).unwrap();

        std::fs::rename(&directory, &old).unwrap();
        assert!(watch.changed().unwrap());
        assert_eq!(watch.directories.borrow().len(), 1);
        assert!(watch.directories.borrow().contains_key(root.path()));

        std::fs::write(root.path().join("unrelated"), "ignore me").unwrap();
        assert!(!watch.changed().unwrap());
        std::fs::write(old.join("panel.luau"), "also ignore me").unwrap();
        assert!(!watch.changed().unwrap());

        std::fs::create_dir(&directory).unwrap();
        std::fs::write(&panel, "invalid replacement").unwrap();
        assert!(watch.changed().unwrap());
        assert!(watch.directories.borrow().contains_key(&directory));
        assert!(!watch.changed().unwrap());

        std::fs::write(directory.join("unrelated.luau"), "return {}").unwrap();
        assert!(!watch.changed().unwrap());
        // a failed reload does not call set_files. the correction must still be
        // observed by the watch installed when the directory reappeared.
        std::fs::write(&panel, "corrected").unwrap();
        assert!(watch.changed().unwrap());
    }

    #[test]
    fn closes_the_creation_race_during_recovery_handoff() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("panel.luau"), "before").unwrap();
        let panel = directory.join("panel.luau").canonicalize().unwrap();
        std::fs::rename(&directory, root.path().join("config-old")).unwrap();

        let watch = Watch::new().unwrap();
        let files = BTreeSet::from([panel.clone()]);
        let mut create = true;
        watch
            .reconcile_after_watch(&files, |path| {
                if create && path == root.path() {
                    create = false;
                    std::fs::create_dir(&directory).unwrap();
                    std::fs::write(&panel, "invalid replacement").unwrap();
                }
            })
            .unwrap();
        *watch.files.borrow_mut() = files;

        assert_eq!(watch.directories.borrow().len(), 1);
        assert!(watch.directories.borrow().contains_key(&directory));
        std::fs::write(&panel, "corrected").unwrap();
        assert!(watch.changed().unwrap());
    }

    #[test]
    fn rebinds_when_a_populated_replacement_arrives_before_events_are_drained() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("config");
        let old = root.path().join("config-old");
        let staged = root.path().join("config-next");
        std::fs::create_dir(&directory).unwrap();
        std::fs::create_dir(&staged).unwrap();
        std::fs::write(directory.join("panel.luau"), "before").unwrap();
        std::fs::write(staged.join("panel.luau"), "replacement").unwrap();
        let panel = directory.join("panel.luau").canonicalize().unwrap();
        let watch = Watch::new().unwrap();
        watch.set_files(std::slice::from_ref(&panel)).unwrap();

        std::fs::rename(&directory, old).unwrap();
        std::fs::rename(staged, &directory).unwrap();
        assert!(watch.changed().unwrap());
        assert!(watch.directories.borrow().contains_key(&directory));
        watch.set_files(std::slice::from_ref(&panel)).unwrap();
        assert!(!watch.changed().unwrap());

        std::fs::write(&panel, "follow-up").unwrap();
        assert!(watch.changed().unwrap());
    }
}
