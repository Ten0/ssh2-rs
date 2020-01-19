use libc::{c_int, c_long, c_uint, c_ulong, size_t};
use parking_lot::{Mutex, MutexGuard};
use std::io::prelude::*;
use std::io::{self, ErrorKind, SeekFrom};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use util;
use {raw, Error, SessionInner};

struct SftpInner {
    raw: *mut raw::LIBSSH2_SFTP,
    sess: Arc<Mutex<SessionInner>>,
}

/// A handle to a remote filesystem over SFTP.
///
/// Instances are created through the `sftp` method on a `Session`.
pub struct Sftp {
    inner: Option<Arc<SftpInner>>,
}

struct LockedSftp<'sftp> {
    sess: MutexGuard<'sftp, SessionInner>,
    raw: *mut raw::LIBSSH2_SFTP,
}

struct FileInner {
    raw: *mut raw::LIBSSH2_SFTP_HANDLE,
    sftp: Arc<SftpInner>,
}

struct LockedFile<'file> {
    sess: MutexGuard<'file, SessionInner>,
    raw: *mut raw::LIBSSH2_SFTP_HANDLE,
}

/// A file handle to an SFTP connection.
///
/// Files behave similarly to `std::old_io::File` in that they are readable and
/// writable and support operations like stat and seek.
///
/// Files are created through `open`, `create`, and `open_mode` on an instance
/// of `Sftp`.
pub struct File {
    inner: Option<FileInner>,
}

/// Metadata information about a remote file.
///
/// Fields are not necessarily all provided
#[derive(Debug, Clone, Eq, PartialEq)]
#[allow(missing_copy_implementations)]
pub struct FileStat {
    /// File size, in bytes of the file.
    pub size: Option<u64>,
    /// Owner ID of the file
    pub uid: Option<u32>,
    /// Owning group of the file
    pub gid: Option<u32>,
    /// Permissions (mode) of the file
    pub perm: Option<u32>,
    /// Last access time of the file
    pub atime: Option<u64>,
    /// Last modification time of the file
    pub mtime: Option<u64>,
}

/// An structure representing a type of file.
pub struct FileType {
    perm: c_ulong,
}

bitflags! {
    /// Options that can be used to configure how a file is opened
    pub struct OpenFlags: c_ulong {
        /// Open the file for reading.
        const READ = raw::LIBSSH2_FXF_READ;
        /// Open the file for writing. If both this and `Read` are specified,
        /// the file is opened for both reading and writing.
        const WRITE = raw::LIBSSH2_FXF_WRITE;
        /// Force all writes to append data at the end of the file.
        const APPEND = raw::LIBSSH2_FXF_APPEND;
        /// If this flag is specified, then a new file will be created if one
        /// does not already exist (if `Truncate` is specified, the new file
        /// will be truncated to zero length if it previously exists).
        const CREATE = raw::LIBSSH2_FXF_CREAT;
        /// Forces an existing file with the same name to be truncated to zero
        /// length when creating a file by specifying `Create`. Using this flag
        /// implies the `Create` flag.
        const TRUNCATE = raw::LIBSSH2_FXF_TRUNC | Self::CREATE.bits;
        /// Causes the request to fail if the named file already exists. Using
        /// this flag implies the `Create` flag.
        const EXCLUSIVE = raw::LIBSSH2_FXF_EXCL | Self::CREATE.bits;
    }
}

bitflags! {
    /// Options to `Sftp::rename`.
    pub struct RenameFlags: c_long {
        /// In a rename operation, overwrite the destination if it already
        /// exists. If this flag is not present then it is an error if the
        /// destination already exists.
        const OVERWRITE = raw::LIBSSH2_SFTP_RENAME_OVERWRITE;
        /// Inform the remote that an atomic rename operation is desired if
        /// available.
        const ATOMIC = raw::LIBSSH2_SFTP_RENAME_ATOMIC;
        /// Inform the remote end that the native system calls for renaming
        /// should be used.
        const NATIVE = raw::LIBSSH2_SFTP_RENAME_NATIVE;
    }
}

/// How to open a file handle with libssh2.
#[derive(Copy, Clone)]
pub enum OpenType {
    /// Specify that a file shoud be opened.
    File = raw::LIBSSH2_SFTP_OPENFILE as isize,
    /// Specify that a directory should be opened.
    Dir = raw::LIBSSH2_SFTP_OPENDIR as isize,
}

impl Sftp {
    pub(crate) fn from_raw_opt(
        raw: *mut raw::LIBSSH2_SFTP,
        err: Option<Error>,
        sess: &Arc<Mutex<SessionInner>>,
    ) -> Result<Self, Error> {
        if raw.is_null() {
            Err(err.unwrap_or_else(Error::unknown))
        } else {
            Ok(Self {
                inner: Some(Arc::new(SftpInner {
                    raw,
                    sess: Arc::clone(sess),
                })),
            })
        }
    }

    /// Open a handle to a file.
    pub fn open_mode(
        &self,
        filename: &Path,
        flags: OpenFlags,
        mode: i32,
        open_type: OpenType,
    ) -> Result<File, Error> {
        let filename = util::path2bytes(filename)?;

        let locked = self.lock()?;
        unsafe {
            let ret = raw::libssh2_sftp_open_ex(
                locked.raw,
                filename.as_ptr() as *const _,
                filename.len() as c_uint,
                flags.bits() as c_ulong,
                mode as c_long,
                open_type as c_int,
            );
            if ret.is_null() {
                Err(locked.sess.last_error().unwrap_or_else(Error::unknown))
            } else {
                Ok(File::from_raw(self, ret))
            }
        }
    }

    /// Helper to open a file in the `Read` mode.
    pub fn open(&self, filename: &Path) -> Result<File, Error> {
        self.open_mode(filename, OpenFlags::READ, 0o644, OpenType::File)
    }

    /// Helper to create a file in write-only mode with truncation.
    pub fn create(&self, filename: &Path) -> Result<File, Error> {
        self.open_mode(
            filename,
            OpenFlags::WRITE | OpenFlags::TRUNCATE,
            0o644,
            OpenType::File,
        )
    }

    /// Helper to open a directory for reading its contents.
    pub fn opendir(&self, dirname: &Path) -> Result<File, Error> {
        self.open_mode(dirname, OpenFlags::READ, 0, OpenType::Dir)
    }

    /// Convenience function to read the files in a directory.
    ///
    /// The returned paths are all joined with `dirname` when returned, and the
    /// paths `.` and `..` are filtered out of the returned list.
    pub fn readdir(&self, dirname: &Path) -> Result<Vec<(PathBuf, FileStat)>, Error> {
        let mut dir = self.opendir(dirname)?;
        let mut ret = Vec::new();
        loop {
            match dir.readdir() {
                Ok((filename, stat)) => {
                    if &*filename == Path::new(".") || &*filename == Path::new("..") {
                        continue;
                    }

                    ret.push((dirname.join(&filename), stat))
                }
                Err(ref e) if e.code() == raw::LIBSSH2_ERROR_FILE => break,
                Err(e) => return Err(e),
            }
        }
        Ok(ret)
    }

    /// Create a directory on the remote file system.
    pub fn mkdir(&self, filename: &Path, mode: i32) -> Result<(), Error> {
        let filename = util::path2bytes(filename)?;
        let locked = self.lock()?;
        locked.sess.rc(unsafe {
            raw::libssh2_sftp_mkdir_ex(
                locked.raw,
                filename.as_ptr() as *const _,
                filename.len() as c_uint,
                mode as c_long,
            )
        })
    }

    /// Remove a directory from the remote file system.
    pub fn rmdir(&self, filename: &Path) -> Result<(), Error> {
        let filename = util::path2bytes(filename)?;
        let locked = self.lock()?;
        locked.sess.rc(unsafe {
            raw::libssh2_sftp_rmdir_ex(
                locked.raw,
                filename.as_ptr() as *const _,
                filename.len() as c_uint,
            )
        })
    }

    /// Get the metadata for a file, performed by stat(2)
    pub fn stat(&self, filename: &Path) -> Result<FileStat, Error> {
        let filename = util::path2bytes(filename)?;
        let locked = self.lock()?;
        unsafe {
            let mut ret = mem::zeroed();
            let rc = raw::libssh2_sftp_stat_ex(
                locked.raw,
                filename.as_ptr() as *const _,
                filename.len() as c_uint,
                raw::LIBSSH2_SFTP_STAT,
                &mut ret,
            );
            locked.sess.rc(rc)?;
            Ok(FileStat::from_raw(&ret))
        }
    }

    /// Get the metadata for a file, performed by lstat(2)
    pub fn lstat(&self, filename: &Path) -> Result<FileStat, Error> {
        let filename = util::path2bytes(filename)?;
        let locked = self.lock()?;
        unsafe {
            let mut ret = mem::zeroed();
            let rc = raw::libssh2_sftp_stat_ex(
                locked.raw,
                filename.as_ptr() as *const _,
                filename.len() as c_uint,
                raw::LIBSSH2_SFTP_LSTAT,
                &mut ret,
            );
            locked.sess.rc(rc)?;
            Ok(FileStat::from_raw(&ret))
        }
    }

    /// Set the metadata for a file.
    pub fn setstat(&self, filename: &Path, stat: FileStat) -> Result<(), Error> {
        let filename = util::path2bytes(filename)?;
        let locked = self.lock()?;
        locked.sess.rc(unsafe {
            let mut raw = stat.raw();
            raw::libssh2_sftp_stat_ex(
                locked.raw,
                filename.as_ptr() as *const _,
                filename.len() as c_uint,
                raw::LIBSSH2_SFTP_SETSTAT,
                &mut raw,
            )
        })
    }

    /// Create a symlink at `target` pointing at `path`.
    pub fn symlink(&self, path: &Path, target: &Path) -> Result<(), Error> {
        let path = util::path2bytes(path)?;
        let target = util::path2bytes(target)?;
        let locked = self.lock()?;
        locked.sess.rc(unsafe {
            raw::libssh2_sftp_symlink_ex(
                locked.raw,
                path.as_ptr() as *const _,
                path.len() as c_uint,
                target.as_ptr() as *mut _,
                target.len() as c_uint,
                raw::LIBSSH2_SFTP_SYMLINK,
            )
        })
    }

    /// Read a symlink at `path`.
    pub fn readlink(&self, path: &Path) -> Result<PathBuf, Error> {
        self.readlink_op(path, raw::LIBSSH2_SFTP_READLINK)
    }

    /// Resolve the real path for `path`.
    pub fn realpath(&self, path: &Path) -> Result<PathBuf, Error> {
        self.readlink_op(path, raw::LIBSSH2_SFTP_REALPATH)
    }

    fn readlink_op(&self, path: &Path, op: c_int) -> Result<PathBuf, Error> {
        let path = util::path2bytes(path)?;
        let mut ret = Vec::<u8>::with_capacity(128);
        let mut rc;
        let locked = self.lock()?;
        loop {
            rc = unsafe {
                raw::libssh2_sftp_symlink_ex(
                    locked.raw,
                    path.as_ptr() as *const _,
                    path.len() as c_uint,
                    ret.as_ptr() as *mut _,
                    ret.capacity() as c_uint,
                    op,
                )
            };
            if rc == raw::LIBSSH2_ERROR_BUFFER_TOO_SMALL {
                let cap = ret.capacity();
                ret.reserve(cap);
            } else {
                break;
            }
        }
        if rc < 0 {
            Err(Error::from_session_error_raw(locked.sess.raw, rc))
        } else {
            unsafe { ret.set_len(rc as usize) }
            Ok(mkpath(ret))
        }
    }

    /// Rename a filesystem object on the remote filesystem.
    ///
    /// The semantics of this command typically include the ability to move a
    /// filesystem object between folders and/or filesystem mounts. If the
    /// `Overwrite` flag is not set and the destfile entry already exists, the
    /// operation will fail.
    ///
    /// Use of the other flags (Native or Atomic) indicate a preference (but
    /// not a requirement) for the remote end to perform an atomic rename
    /// operation and/or using native system calls when possible.
    ///
    /// If no flags are specified then all flags are used.
    pub fn rename(&self, src: &Path, dst: &Path, flags: Option<RenameFlags>) -> Result<(), Error> {
        let flags =
            flags.unwrap_or(RenameFlags::ATOMIC | RenameFlags::OVERWRITE | RenameFlags::NATIVE);
        let src = util::path2bytes(src)?;
        let dst = util::path2bytes(dst)?;
        let locked = self.lock()?;
        locked.sess.rc(unsafe {
            raw::libssh2_sftp_rename_ex(
                locked.raw,
                src.as_ptr() as *const _,
                src.len() as c_uint,
                dst.as_ptr() as *const _,
                dst.len() as c_uint,
                flags.bits(),
            )
        })
    }

    /// Remove a file on the remote filesystem
    pub fn unlink(&self, file: &Path) -> Result<(), Error> {
        let file = util::path2bytes(file)?;
        let locked = self.lock()?;
        locked.sess.rc(unsafe {
            raw::libssh2_sftp_unlink_ex(locked.raw, file.as_ptr() as *const _, file.len() as c_uint)
        })
    }

    fn lock(&self) -> Result<LockedSftp, Error> {
        match self.inner.as_ref() {
            Some(inner) => {
                let sess = inner.sess.lock();
                Ok(LockedSftp {
                    sess,
                    raw: inner.raw,
                })
            }
            None => Err(Error::from_errno(raw::LIBSSH2_ERROR_BAD_USE)),
        }
    }

    // This method is used by the async ssh crate
    #[doc(hidden)]
    pub fn shutdown(&mut self) -> Result<(), Error> {
        {
            let locked = self.lock()?;
            locked
                .sess
                .rc(unsafe { raw::libssh2_sftp_shutdown(locked.raw) })?;
        }
        let _ = self.inner.take();
        Ok(())
    }
}

impl Drop for Sftp {
    fn drop(&mut self) {
        // Set ssh2 to blocking if sftp was not shutdown yet.
        if let Some(inner) = self.inner.take() {
            let sess = inner.sess.lock();
            let was_blocking = sess.is_blocking();
            sess.set_blocking(true);
            assert_eq!(unsafe { raw::libssh2_sftp_shutdown(inner.raw) }, 0);
            sess.set_blocking(was_blocking);
        }
    }
}

impl File {
    /// Wraps a raw pointer in a new File structure tied to the lifetime of the
    /// given session.
    ///
    /// This consumes ownership of `raw`.
    unsafe fn from_raw(sftp: &Sftp, raw: *mut raw::LIBSSH2_SFTP_HANDLE) -> File {
        File {
            inner: Some(FileInner {
                raw: raw,
                sftp: Arc::clone(
                    sftp.inner
                        .as_ref()
                        .expect("we have a live option during construction"),
                ),
            }),
        }
    }

    /// Set the metadata for this handle.
    pub fn setstat(&mut self, stat: FileStat) -> Result<(), Error> {
        let locked = self.lock()?;
        locked.sess.rc(unsafe {
            let mut raw = stat.raw();
            raw::libssh2_sftp_fstat_ex(locked.raw, &mut raw, 1)
        })
    }

    /// Get the metadata for this handle.
    pub fn stat(&mut self) -> Result<FileStat, Error> {
        let locked = self.lock()?;
        unsafe {
            let mut ret = mem::zeroed();
            locked
                .sess
                .rc(raw::libssh2_sftp_fstat_ex(locked.raw, &mut ret, 0))?;
            Ok(FileStat::from_raw(&ret))
        }
    }

    #[allow(missing_docs)] // sure wish I knew what this did...
    pub fn statvfs(&mut self) -> Result<raw::LIBSSH2_SFTP_STATVFS, Error> {
        let locked = self.lock()?;
        unsafe {
            let mut ret = mem::zeroed();
            locked
                .sess
                .rc(raw::libssh2_sftp_fstatvfs(locked.raw, &mut ret))?;
            Ok(ret)
        }
    }

    /// Reads a block of data from a handle and returns file entry information
    /// for the next entry, if any.
    ///
    /// Note that this provides raw access to the `readdir` function from
    /// libssh2. This will return an error when there are no more files to
    /// read, and files such as `.` and `..` will be included in the return
    /// values.
    ///
    /// Also note that the return paths will not be absolute paths, they are
    /// the filenames of the files in this directory.
    pub fn readdir(&mut self) -> Result<(PathBuf, FileStat), Error> {
        let locked = self.lock()?;

        let mut buf = Vec::<u8>::with_capacity(128);
        let mut stat = unsafe { mem::zeroed() };
        let mut rc;
        loop {
            rc = unsafe {
                raw::libssh2_sftp_readdir_ex(
                    locked.raw,
                    buf.as_mut_ptr() as *mut _,
                    buf.capacity() as size_t,
                    0 as *mut _,
                    0,
                    &mut stat,
                )
            };
            if rc == raw::LIBSSH2_ERROR_BUFFER_TOO_SMALL {
                let cap = buf.capacity();
                buf.reserve(cap);
            } else {
                break;
            }
        }
        if rc < 0 {
            return Err(Error::from_session_error_raw(locked.sess.raw, rc));
        } else if rc == 0 {
            return Err(Error::new(raw::LIBSSH2_ERROR_FILE, "no more files"));
        } else {
            unsafe {
                buf.set_len(rc as usize);
            }
        }
        Ok((mkpath(buf), FileStat::from_raw(&stat)))
    }

    /// This function causes the remote server to synchronize the file data and
    /// metadata to disk (like fsync(2)).
    ///
    /// For this to work requires fsync@openssh.com support on the server.
    pub fn fsync(&mut self) -> Result<(), Error> {
        let locked = self.lock()?;
        locked
            .sess
            .rc(unsafe { raw::libssh2_sftp_fsync(locked.raw) })
    }

    fn lock(&self) -> Result<LockedFile, Error> {
        match self.inner.as_ref() {
            Some(file_inner) => {
                let sess = file_inner.sftp.sess.lock();
                Ok(LockedFile {
                    sess,
                    raw: file_inner.raw,
                })
            }
            None => Err(Error::from_errno(raw::LIBSSH2_ERROR_BAD_USE)),
        }
    }

    #[doc(hidden)]
    pub fn close(&mut self) -> Result<(), Error> {
        {
            let locked = self.lock()?;
            Error::rc(unsafe { raw::libssh2_sftp_close_handle(locked.raw) })?;
        }
        let _ = self.inner.take();
        Ok(())
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let locked = self.lock()?;
        unsafe {
            let rc =
                raw::libssh2_sftp_read(locked.raw, buf.as_mut_ptr() as *mut _, buf.len() as size_t);
            if rc < 0 {
                Err(Error::from_session_error_raw(locked.sess.raw, rc as _).into())
            } else {
                Ok(rc as usize)
            }
        }
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let locked = self.lock()?;
        let rc = unsafe {
            raw::libssh2_sftp_write(locked.raw, buf.as_ptr() as *const _, buf.len() as size_t)
        };
        if rc < 0 {
            Err(Error::from_session_error_raw(locked.sess.raw, rc as _).into())
        } else {
            Ok(rc as usize)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for File {
    /// Move the file handle's internal pointer to an arbitrary location.
    ///
    /// libssh2 implements file pointers as a localized concept to make file
    /// access appear more POSIX like. No packets are exchanged with the server
    /// during a seek operation. The localized file pointer is simply used as a
    /// convenience offset during read/write operations.
    ///
    /// You MUST NOT seek during writing or reading a file with SFTP, as the
    /// internals use outstanding packets and changing the "file position"
    /// during transit will results in badness.
    fn seek(&mut self, how: SeekFrom) -> io::Result<u64> {
        let next = match how {
            SeekFrom::Start(pos) => pos,
            SeekFrom::Current(offset) => {
                let locked = self.lock()?;
                let cur = unsafe { raw::libssh2_sftp_tell64(locked.raw) };
                (cur as i64 + offset) as u64
            }
            SeekFrom::End(offset) => match self.stat() {
                Ok(s) => match s.size {
                    Some(size) => (size as i64 + offset) as u64,
                    None => return Err(io::Error::new(ErrorKind::Other, "no file size available")),
                },
                Err(e) => return Err(io::Error::new(ErrorKind::Other, e)),
            },
        };
        let locked = self.lock()?;
        unsafe { raw::libssh2_sftp_seek64(locked.raw, next) }
        Ok(next)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        // Set ssh2 to blocking if the file was not closed yet.
        if let Some(file_inner) = self.inner.take() {
            let sess_inner = file_inner.sftp.sess.lock();
            let was_blocking = sess_inner.is_blocking();
            sess_inner.set_blocking(true);
            assert_eq!(unsafe { raw::libssh2_sftp_close_handle(file_inner.raw) }, 0);
            sess_inner.set_blocking(was_blocking);
        }
    }
}

impl FileStat {
    /// Returns the file type for this filestat.
    pub fn file_type(&self) -> FileType {
        FileType {
            perm: self.perm.unwrap_or(0) as c_ulong,
        }
    }

    /// Returns whether this metadata is for a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    /// Returns whether this metadata is for a regular file.
    pub fn is_file(&self) -> bool {
        self.file_type().is_file()
    }

    /// Creates a new instance of a stat from a raw instance.
    pub fn from_raw(raw: &raw::LIBSSH2_SFTP_ATTRIBUTES) -> FileStat {
        fn val<T: Copy>(raw: &raw::LIBSSH2_SFTP_ATTRIBUTES, t: &T, flag: c_ulong) -> Option<T> {
            if raw.flags & flag != 0 {
                Some(*t)
            } else {
                None
            }
        }

        FileStat {
            size: val(raw, &raw.filesize, raw::LIBSSH2_SFTP_ATTR_SIZE),
            uid: val(raw, &raw.uid, raw::LIBSSH2_SFTP_ATTR_UIDGID).map(|s| s as u32),
            gid: val(raw, &raw.gid, raw::LIBSSH2_SFTP_ATTR_UIDGID).map(|s| s as u32),
            perm: val(raw, &raw.permissions, raw::LIBSSH2_SFTP_ATTR_PERMISSIONS).map(|s| s as u32),
            mtime: val(raw, &raw.mtime, raw::LIBSSH2_SFTP_ATTR_ACMODTIME).map(|s| s as u64),
            atime: val(raw, &raw.atime, raw::LIBSSH2_SFTP_ATTR_ACMODTIME).map(|s| s as u64),
        }
    }

    /// Convert this stat structure to its raw representation.
    pub fn raw(&self) -> raw::LIBSSH2_SFTP_ATTRIBUTES {
        fn flag<T>(o: &Option<T>, flag: c_ulong) -> c_ulong {
            if o.is_some() {
                flag
            } else {
                0
            }
        }

        raw::LIBSSH2_SFTP_ATTRIBUTES {
            flags: flag(&self.size, raw::LIBSSH2_SFTP_ATTR_SIZE)
                | flag(&self.uid, raw::LIBSSH2_SFTP_ATTR_UIDGID)
                | flag(&self.gid, raw::LIBSSH2_SFTP_ATTR_UIDGID)
                | flag(&self.perm, raw::LIBSSH2_SFTP_ATTR_PERMISSIONS)
                | flag(&self.atime, raw::LIBSSH2_SFTP_ATTR_ACMODTIME)
                | flag(&self.mtime, raw::LIBSSH2_SFTP_ATTR_ACMODTIME),
            filesize: self.size.unwrap_or(0),
            uid: self.uid.unwrap_or(0) as c_ulong,
            gid: self.gid.unwrap_or(0) as c_ulong,
            permissions: self.perm.unwrap_or(0) as c_ulong,
            atime: self.atime.unwrap_or(0) as c_ulong,
            mtime: self.mtime.unwrap_or(0) as c_ulong,
        }
    }
}

impl FileType {
    /// Test whether this file type represents a directory.
    pub fn is_dir(&self) -> bool {
        self.is(raw::LIBSSH2_SFTP_S_IFDIR)
    }

    /// Test whether this file type represents a regular file.
    pub fn is_file(&self) -> bool {
        self.is(raw::LIBSSH2_SFTP_S_IFREG)
    }

    /// Test whether this file type represents a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.is(raw::LIBSSH2_SFTP_S_IFLNK)
    }

    fn is(&self, perm: c_ulong) -> bool {
        (self.perm & raw::LIBSSH2_SFTP_S_IFMT) == perm
    }
}

#[cfg(unix)]
fn mkpath(v: Vec<u8>) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::prelude::*;
    PathBuf::from(OsStr::from_bytes(&v))
}
#[cfg(windows)]
fn mkpath(v: Vec<u8>) -> PathBuf {
    use std::str;
    PathBuf::from(str::from_utf8(&v).unwrap())
}
