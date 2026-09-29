//! Opening a target: an image file or a device, read-only or writable,
//! optionally `--offset` bytes in (a partition inside a whole-disk image).

use std::ffi::OsString;
use std::sync::Arc;

use crate::common::CliError;
use fs_ext4::block_io::{BlockDevice, FileDevice};
use fs_ext4::Filesystem;

/// A device that starts `offset` bytes into another.
struct Offset<D> {
    inner: D,
    offset: u64,
    size: u64,
}

impl<D: BlockDevice> Offset<D> {
    fn check(&self, offset: u64, len: usize) -> fs_ext4::Result<u64> {
        let end = offset
            .checked_add(len as u64)
            .ok_or(fs_ext4::Error::OutOfBounds)?;
        if end > self.size {
            return Err(fs_ext4::Error::OutOfBounds);
        }
        Ok(self.offset + offset)
    }
}

impl<D: BlockDevice> BlockDevice for Offset<D> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_ext4::Result<()> {
        let at = self.check(offset, buf.len())?;
        self.inner.read_at(at, buf)
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_ext4::Result<()> {
        let at = self.check(offset, buf.len())?;
        self.inner.write_at(at, buf)
    }

    fn flush(&self) -> fs_ext4::Result<()> {
        self.inner.flush()
    }

    fn is_writable(&self) -> bool {
        self.inner.is_writable()
    }
}

/// Open `target`, `offset` bytes in, read-only or read-write.
pub fn open(
    target: &OsString,
    offset: u64,
    writable: bool,
) -> Result<Arc<dyn BlockDevice>, CliError> {
    let name = target.to_string_lossy();
    let dev = if writable {
        FileDevice::open_rw(&name)
    } else {
        FileDevice::open(&name)
    }
    .map_err(|e| {
        CliError::failed(format!(
            "open {name}{}: {e}",
            if writable { " read-write" } else { "" }
        ))
    })?;
    let size = dev.size_bytes();
    if offset == 0 {
        return Ok(Arc::new(dev));
    }
    if offset >= size {
        return Err(CliError::failed(format!(
            "--offset {offset} is past the end of {name} ({size} bytes)"
        )));
    }
    Ok(Arc::new(Offset {
        inner: dev,
        offset,
        size: size - offset,
    }))
}

/// Open and mount `target`.
pub fn mount(target: &OsString, offset: u64, writable: bool) -> Result<Filesystem, CliError> {
    let dev = open(target, offset, writable)?;
    Filesystem::mount(dev).map_err(|e| {
        CliError::failed(format!(
            "{} is not a readable ext4 filesystem: {e}",
            target.to_string_lossy()
        ))
    })
}
