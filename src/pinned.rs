//! Descriptor-pinned SQLite open.
//!
//! A published graph image is validated through a retained descriptor, and every
//! later read must reach that same inode. SQLite cannot be handed one:
//! `sqlite3_open_v2` takes a filename, no built-in unix VFS accepts a
//! descriptor, and `rusqlite` exposes no VFS registration at all. Opening the
//! image again by name would re-resolve a mutable final component, so a rename
//! between validation and open could substitute a different inode.
//!
//! The unix VFS routes every `open` through a replaceable syscall table.
//! [`pin`] captures the table's original `open` once, installs an override, and
//! scopes the diversion to the calling thread and one exact path: while that pin
//! is live, an `open` of that path returns a duplicate of the pinned descriptor
//! and resolves nothing. Every other path, thread, and access mode reaches the
//! captured original unchanged.
//!
//! Two properties make the duplicate sound. SQLite reads and writes with
//! `pread`/`pwrite` on Linux and macOS — `sqlite3.c` defines `HAVE_PREAD` and
//! `HAVE_PWRITE` for `__APPLE__` and `__linux__`, so `seekAndRead` never uses
//! the shared file offset a duplicate carries. And the diversion is restricted
//! to read-only opens, so the pinned read-only descriptor always satisfies the
//! access mode SQLite asked for.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_int};
use std::fs::File;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::rc::Rc;
use std::sync::{LazyLock, OnceLock};

use rusqlite::ffi;

/// The unix VFS `open` slot: `int (*)(const char*, int, int)`.
type OpenCall = unsafe extern "C" fn(*const c_char, c_int, c_int) -> c_int;

struct Pinned {
    token: Rc<()>,
    path: Box<[u8]>,
    file: File,
    /// Set when the override answers an open for `path`. A pin that SQLite
    /// never asked for means the open resolved a name instead, which is the
    /// defect this module exists to remove.
    used: Cell<bool>,
}

thread_local! {
    /// Pins on this thread. The override reads the last entry without
    /// allocating or panicking.
    static PINNED: RefCell<Vec<Pinned>> = const { RefCell::new(Vec::new()) };
}

static ORIGINAL_OPEN: OnceLock<OpenCall> = OnceLock::new();

/// Live pin. Removes only its own entry on drop, so nesting remains safe even
/// when guards are dropped out of order.
pub(crate) struct Pin {
    token: Rc<()>,
}

impl Drop for Pin {
    fn drop(&mut self) {
        let _ = PINNED.try_with(|pins| {
            let Ok(mut pins) = pins.try_borrow_mut() else {
                return;
            };
            if let Some(index) = pins
                .iter()
                .rposition(|pinned| Rc::ptr_eq(&pinned.token, &self.token))
            {
                pins.remove(index);
            }
        });
    }
}

impl Pin {
    /// Fails unless the override actually answered an open for the pinned path.
    ///
    /// SQLite normalises a filename through `unixFullPathname` before the VFS
    /// sees it — `.` and `..` are collapsed and symlinks resolved — so a path
    /// shape that does not survive that round trip would miss the pin, fall
    /// through to the captured original, and resolve by name. That is the
    /// pre-`0.6.1` behaviour, and it is silent: the caller still receives a
    /// working database. Callers must ask, so the failure is loud instead.
    pub(crate) fn require_used(&self) -> Result<(), String> {
        let used = PINNED
            .try_with(|pins| {
                let pins = pins.try_borrow().ok()?;
                pins.iter()
                    .rfind(|pinned| Rc::ptr_eq(&pinned.token, &self.token))
                    .map(|pinned| (pinned.used.get(), pinned.path.clone()))
            })
            .ok()
            .flatten();
        match used {
            Some((true, _)) => Ok(()),
            Some((false, path)) => Err(format!(
                "SQLite resolved {} by name instead of the pinned descriptor",
                String::from_utf8_lossy(&path)
            )),
            None => Err("pinned descriptor was released before its open".to_owned()),
        }
    }
}

/// Diverts SQLite's `open` of `path` to `file` for the duration of the returned
/// guard, on this thread only.
pub(crate) fn pin(path: &Path, file: &File) -> Result<Pin, String> {
    INSTALLED.clone()?;
    let token = Rc::new(());
    let pinned = Pinned {
        token: Rc::clone(&token),
        path: path.as_os_str().as_bytes().into(),
        file: file
            .try_clone()
            .map_err(|error| format!("cannot duplicate pinned descriptor: {error}"))?,
        used: Cell::new(false),
    };
    PINNED.with(|pins| pins.borrow_mut().push(pinned));
    Ok(Pin { token })
}

/// Replacement for the unix VFS `open`. Runs on SQLite's thread inside
/// `sqlite3_open_v2`, so it allocates nothing and cannot unwind.
unsafe extern "C" fn pinned_open(path: *const c_char, flags: c_int, mode: c_int) -> c_int {
    if let Some(descriptor) = diverted_descriptor(path, flags) {
        // SAFETY: the descriptor belongs to the file owned by the thread-local
        // pin, and F_DUPFD_CLOEXEC only reads it. SQLite owns the duplicate and
        // closes it; the pinned descriptor is untouched.
        return unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, MINIMUM_DESCRIPTOR) };
    }
    match ORIGINAL_OPEN.get() {
        // SAFETY: the captured original is the unix VFS `open` and receives the
        // arguments SQLite passed, unchanged.
        Some(original) => unsafe { original(path, flags, mode) },
        None => -1,
    }
}

/// SQLite reopens a database whose descriptor is below this, so a duplicate must
/// clear it too. Matches `SQLITE_MINIMUM_FILE_DESCRIPTOR`.
const MINIMUM_DESCRIPTOR: c_int = 3;

fn diverted_descriptor(path: *const c_char, flags: c_int) -> Option<RawFd> {
    if path.is_null()
        || flags & libc::O_ACCMODE != libc::O_RDONLY
        || flags & (libc::O_CREAT | libc::O_TRUNC | libc::O_EXCL) != 0
    {
        return None;
    }
    // SAFETY: SQLite passes a NUL-terminated path for the duration of this call.
    let requested = unsafe { CStr::from_ptr(path) }.to_bytes();
    PINNED
        .try_with(|pins| {
            let pins = pins.try_borrow().ok()?;
            let pinned = pins.last()?;
            (requested == pinned.path.as_ref()).then(|| {
                pinned.used.set(true);
                pinned.file.as_raw_fd()
            })
        })
        .ok()
        .flatten()
}

/// Captures the unix VFS `open` and installs the override, once per process.
///
/// A concurrent first install is benign: the table slot holds either the
/// original or the override, and the override forwards every open it does not
/// divert, so both values open exactly what SQLite asked for.
static INSTALLED: LazyLock<Result<(), String>> = LazyLock::new(install_override);

fn install_override() -> Result<(), String> {
    // SAFETY: sqlite3_initialize is idempotent and callable before any
    // connection exists.
    if unsafe { ffi::sqlite3_initialize() } != ffi::SQLITE_OK {
        return Err("cannot initialize SQLite".to_owned());
    }
    // SAFETY: a null name requests the default VFS.
    let vfs = unsafe { ffi::sqlite3_vfs_find(std::ptr::null()) };
    if vfs.is_null() {
        return Err("SQLite has no default VFS".to_owned());
    }
    // SAFETY: sqlite3_vfs_find returned a VFS owned by SQLite for the life of
    // the process. Only iVersion is read until it proves the syscall members
    // are present.
    if unsafe { (*vfs).iVersion } < 3 {
        return Err("SQLite VFS predates the replaceable syscall table".to_owned());
    }
    // SAFETY: iVersion 3 or later defines both members.
    let (get, set) = unsafe { ((*vfs).xGetSystemCall, (*vfs).xSetSystemCall) };
    let (Some(get), Some(set)) = (get, set) else {
        return Err("SQLite VFS does not expose its syscall table".to_owned());
    };
    let name = c"open";
    // SAFETY: both calls receive the VFS they were read from and a
    // NUL-terminated syscall name.
    let original = unsafe { get(vfs, name.as_ptr()) }
        .ok_or_else(|| "SQLite VFS has no open syscall".to_owned())?;
    // SAFETY: the unix VFS stores `open` with the OpenCall signature, erased to
    // a bare function pointer by the syscall table.
    let original: OpenCall = unsafe { std::mem::transmute::<_, OpenCall>(original) };
    let _ = ORIGINAL_OPEN.set(original);
    // SAFETY: the override has the exact signature the slot requires.
    let override_open =
        unsafe { std::mem::transmute::<OpenCall, unsafe extern "C" fn()>(pinned_open) };
    // SAFETY: as above; the override outlives the process.
    if unsafe { set(vfs, name.as_ptr(), Some(override_open)) } != ffi::SQLITE_OK {
        return Err("SQLite VFS rejected the open override".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::sync::mpsc;

    use rusqlite::{Connection, OpenFlags};

    #[test]
    fn a_pin_diverts_that_path_to_the_pinned_descriptor() {
        let directory = temporary_directory("divert");
        let pinned_path = directory.join("pinned.db");
        let file = write_marked_database(&pinned_path, "validated");
        replace_with_marked_database(&pinned_path, "substituted");

        let unpinned = marker(&pinned_path);
        let pin = pin(&pinned_path, &file).unwrap();
        let pinned = marker(&pinned_path);
        drop(pin);
        let released = marker(&pinned_path);

        assert_eq!(pinned, "validated");
        assert_eq!(unpinned, "substituted");
        assert_eq!(released, "substituted");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_pin_leaves_every_other_path_resolving_by_name() {
        let directory = temporary_directory("other-path");
        let pinned_path = directory.join("pinned.db");
        let other_path = directory.join("other.db");
        let file = write_marked_database(&pinned_path, "validated");
        replace_with_marked_database(&pinned_path, "substituted");
        write_marked_database(&other_path, "other");

        let _pin = pin(&pinned_path, &file).unwrap();

        assert_eq!(marker(&other_path), "other");
        assert_eq!(marker(&pinned_path), "validated");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_pin_that_answered_an_open_reports_that_it_was_used() {
        let directory = temporary_directory("used");
        let pinned_path = directory.join("pinned.db");
        let file = write_marked_database(&pinned_path, "validated");

        let pin = pin(&pinned_path, &file).unwrap();
        assert_eq!(marker(&pinned_path), "validated");

        pin.require_used().unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    /// A pin SQLite never asks for is the silent failure this module exists to
    /// remove: the caller still gets a working database, opened by name. The
    /// path here differs only in a `.` component, which `unixFullPathname`
    /// collapses before the VFS sees it — the same shape a future caller could
    /// reach by accident.
    #[test]
    fn a_pin_sqlite_never_asked_for_is_an_error() {
        let directory = temporary_directory("unused");
        let pinned_path = directory.join("pinned.db");
        let file = write_marked_database(&pinned_path, "validated");
        let uncollapsed = directory.join(".").join("pinned.db");

        let pin = pin(&uncollapsed, &file).unwrap();
        let observed = marker(&pinned_path);
        let error = pin.require_used().unwrap_err();

        assert_eq!(observed, "validated");
        assert!(
            error.contains("by name instead of the pinned descriptor"),
            "{error}"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_pin_does_not_extend_to_a_sidecar_of_its_path() {
        let directory = temporary_directory("sidecar");
        let pinned_path = directory.join("pinned.db");
        let sidecar_path = directory.join("pinned.db-wal");
        let file = write_marked_database(&pinned_path, "validated");
        replace_with_marked_database(&pinned_path, "substituted");
        write_marked_database(&sidecar_path, "sidecar");

        let _pin = pin(&pinned_path, &file).unwrap();

        // The match is the whole path, not a prefix of it: a name SQLite derives
        // from a pinned database is still resolved as itself.
        assert_eq!(marker(&sidecar_path), "sidecar");
        assert_eq!(marker(&pinned_path), "validated");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_pin_is_confined_to_the_thread_that_took_it() {
        let directory = temporary_directory("thread");
        let pinned_path = directory.join("pinned.db");
        let file = write_marked_database(&pinned_path, "validated");
        replace_with_marked_database(&pinned_path, "substituted");

        let _pin = pin(&pinned_path, &file).unwrap();
        let elsewhere = std::thread::scope(|scope| {
            scope
                .spawn(|| marker(&pinned_path))
                .join()
                .expect("reader thread")
        });

        assert_eq!(elsewhere, "substituted");
        assert_eq!(marker(&pinned_path), "validated");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_pin_never_diverts_a_writable_open() {
        let directory = temporary_directory("writable");
        let pinned_path = directory.join("pinned.db");
        let file = write_marked_database(&pinned_path, "validated");
        replace_with_marked_database(&pinned_path, "substituted");

        let guard = pin(&pinned_path, &file).unwrap();
        let connection = Connection::open_with_flags(
            &pinned_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        connection
            .execute("UPDATE marker SET value = 'written'", [])
            .unwrap();
        drop(connection);
        let pinned = marker(&pinned_path);
        drop(guard);
        let by_name = marker(&pinned_path);

        assert_eq!(pinned, "validated");
        assert_eq!(by_name, "written");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn nested_pins_restore_the_enclosing_pin() {
        let directory = temporary_directory("nested");
        let outer_path = directory.join("outer.db");
        let inner_path = directory.join("inner.db");
        let outer = write_marked_database(&outer_path, "outer");
        let inner = write_marked_database(&inner_path, "inner");
        replace_with_marked_database(&outer_path, "outer-substituted");
        replace_with_marked_database(&inner_path, "inner-substituted");

        let _outer_pin = pin(&outer_path, &outer).unwrap();
        let inner_pin = pin(&inner_path, &inner).unwrap();
        let nested = (marker(&outer_path), marker(&inner_path));
        drop(inner_pin);
        let restored = (marker(&outer_path), marker(&inner_path));

        assert_eq!(nested, ("outer-substituted".into(), "inner".into()));
        assert_eq!(restored, ("outer".into(), "inner-substituted".into()));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dropping_an_outer_pin_keeps_the_inner_pin_active() {
        let directory = temporary_directory("out-of-order");
        let outer_path = directory.join("outer.db");
        let inner_path = directory.join("inner.db");
        let outer = write_marked_database(&outer_path, "outer");
        let inner = write_marked_database(&inner_path, "inner");
        replace_with_marked_database(&outer_path, "outer-substituted");
        replace_with_marked_database(&inner_path, "inner-substituted");

        let outer_pin = pin(&outer_path, &outer).unwrap();
        let inner_pin = pin(&inner_path, &inner).unwrap();
        drop(outer_pin);

        assert_eq!(marker(&inner_path), "inner");
        drop(inner_pin);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_pinned_readers_each_see_the_pinned_image() {
        let directory = temporary_directory("concurrent");
        let pinned_path = directory.join("pinned.db");
        let file = write_marked_database(&pinned_path, "validated");
        replace_with_marked_database(&pinned_path, "substituted");
        let (sender, receiver) = mpsc::channel();

        std::thread::scope(|scope| {
            for _ in 0..8 {
                let sender = sender.clone();
                let path = pinned_path.clone();
                let file = &file;
                scope.spawn(move || {
                    for _ in 0..16 {
                        let _pin = pin(&path, file).unwrap();
                        sender.send(marker(&path)).unwrap();
                    }
                });
            }
        });
        drop(sender);

        let seen = receiver.into_iter().collect::<Vec<_>>();
        assert_eq!(seen.len(), 8 * 16);
        assert!(seen.iter().all(|value| value == "validated"), "{seen:?}");
        fs::remove_dir_all(directory).unwrap();
    }

    fn write_marked_database(path: &Path, value: &str) -> File {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(&format!(
                "PRAGMA journal_mode=DELETE;
                 CREATE TABLE marker (value TEXT NOT NULL);
                 INSERT INTO marker (value) VALUES ('{value}');"
            ))
            .unwrap();
        drop(connection);
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .unwrap()
    }

    fn replace_with_marked_database(path: &Path, value: &str) {
        let substitute = path.with_extension("substitute");
        write_marked_database(&substitute, value);
        fs::rename(path, path.with_extension("validated")).unwrap();
        fs::rename(&substitute, path).unwrap();
    }

    fn marker(path: &Path) -> String {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .unwrap();
        connection
            .query_row("SELECT value FROM marker", [], |row| row.get(0))
            .unwrap()
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let path = fs::canonicalize(std::env::temp_dir())
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(format!(
                "graphr-pinned-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
