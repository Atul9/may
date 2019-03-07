mod fs_read;
mod fs_write;

pub use self::fs_read::FileRead;
pub use self::fs_write::FileWrite;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;

use super::linux_aio_bindings::EFD_NONBLOCK;
use super::IoData;

pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
    File::open(path)
}

pub fn create<P: AsRef<Path>>(path: P) -> io::Result<File> {
    File::create(path)
}

pub fn open_with_options<P: AsRef<Path>>(options: &mut OpenOptions, path: P) -> io::Result<File> {
    options.open(path.as_ref())
}

pub struct EventFd(RawFd);

impl AsRawFd for EventFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

pub struct FileIo {
    pub fd: EventFd,
    pub io: IoData,
}

impl Drop for FileIo {
    // drop the EventFd first
    fn drop(&mut self) {
        use nix::unistd::close;
        if self.fd.0 >= 0 {
            close(self.fd.0).ok();
        }
    }
}

impl ::std::ops::Deref for FileIo {
    type Target = IoData;
    fn deref(&self) -> &IoData {
        &self.io
    }
}

pub trait AsFileIo {
    fn as_file_io(&self) -> &FileIo;
}

impl FileIo {
    pub fn new(file: Option<&File>) -> io::Result<FileIo> {
        use libc::{eventfd, O_CLOEXEC};
        if file.is_some() {
            let flags = { O_CLOEXEC | EFD_NONBLOCK as i32 };

            let fd = unsafe { eventfd(0, flags) };

            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let ev_fd = EventFd(fd);
            let io = super::add_file(&ev_fd)?;
            Ok(FileIo { io, fd: ev_fd })
        } else {
            // don't close any fd, dummy one
            let fd = EventFd(-1);
            Ok(FileIo {
                io: IoData::new(&fd),
                fd,
            })
        }
    }
}
