//! Read-only file access: a memory map on Unix, a heap buffer elsewhere.
//!
//! Mapping lets the kernel page data in on demand and lets many threads
//! fault pages of different chunks concurrently, which keeps fast SSDs busy.
//! The hints in [`Advice`] let callers read ahead on slow storage and drop
//! pages behind them so resident memory stays small on huge files.
//!
//! As with every memory-mapped reader, truncating or rewriting the file
//! while it is mapped is outside the contract: the OS may deliver `SIGBUS`.

use crate::error::Result;
use std::fs::File;
use std::ops::Deref;
use std::path::Path;

/// Page-cache hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advice {
    /// Start asynchronous read-ahead for the range.
    WillNeed,
    /// Release the pages of the range from this process.
    DontNeed,
}

#[cfg(all(unix, target_pointer_width = "64"))]
/// Ranges passed to `madvise` are aligned to this, a multiple of every page
/// size in use (4 KiB, 16 KiB, 64 KiB).
const ALIGN: usize = 64 * 1024;

/// An immutable view of a whole file.
pub struct Mapping {
    inner: Inner,
}

enum Inner {
    #[cfg(all(unix, target_pointer_width = "64"))]
    Mapped { ptr: *const u8, len: usize },
    Owned(Vec<u8>),
}

// SAFETY: the mapping is read-only and never aliased mutably.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

#[cfg(all(unix, target_pointer_width = "64"))]
mod sys {
    use std::ffi::c_void;
    unsafe extern "C" {
        pub fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> i32;
        pub fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
    }
    pub const PROT_READ: i32 = 1;
    pub const MAP_PRIVATE: i32 = 2;
    pub const MADV_WILLNEED: i32 = 3;
    pub const MADV_DONTNEED: i32 = 4;
    pub const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
}

impl Mapping {
    /// Maps `path` read-only.
    pub fn open(path: impl AsRef<Path>) -> Result<Mapping> {
        let file = File::open(path)?;
        Self::from_file(&file)
    }

    /// Maps an already opened file.
    #[cfg(all(unix, target_pointer_width = "64"))]
    pub fn from_file(file: &File) -> Result<Mapping> {
        use std::os::fd::AsRawFd;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| crate::Error::Unsupported("file larger than the address space".into()))?;
        if len == 0 {
            return Ok(Mapping { inner: Inner::Owned(Vec::new()) });
        }
        // SAFETY: valid fd, non-zero length, kernel picks the address.
        let ptr = unsafe {
            sys::mmap(std::ptr::null_mut(), len, sys::PROT_READ, sys::MAP_PRIVATE, file.as_raw_fd(), 0)
        };
        if ptr == sys::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Mapping { inner: Inner::Mapped { ptr: ptr as *const u8, len } })
    }

    #[cfg(not(all(unix, target_pointer_width = "64")))]
    pub fn from_file(file: &File) -> Result<Mapping> {
        use std::io::Read;
        let mut v = Vec::new();
        (&*file).read_to_end(&mut v)?;
        Ok(Mapping { inner: Inner::Owned(v) })
    }

    /// Wraps bytes that are already in memory.
    pub fn from_vec(v: Vec<u8>) -> Mapping {
        Mapping { inner: Inner::Owned(v) }
    }

    /// Issues a page-cache hint for `[start, end)`. Hints are best effort:
    /// failures, and platforms without `madvise`, are ignored.
    ///
    /// `DontNeed` shrinks the range inward so it never drops a page that a
    /// neighbouring range still uses; `WillNeed` may round outward.
    pub fn advise(&self, start: u64, end: u64, advice: Advice) {
        #[cfg(all(unix, target_pointer_width = "64"))]
        if let Inner::Mapped { ptr, len } = &self.inner {
            let start = start as usize;
            let end = (end as usize).min(*len);
            let (a, b, code) = match advice {
                Advice::WillNeed => (start & !(ALIGN - 1), end, sys::MADV_WILLNEED),
                Advice::DontNeed => (start.div_ceil(ALIGN) * ALIGN, end & !(ALIGN - 1), sys::MADV_DONTNEED),
            };
            if a < b {
                // SAFETY: `a..b` lies inside the mapping and `a` is page aligned.
                unsafe {
                    sys::madvise(ptr.add(a) as *mut _, b - a, code);
                }
            }
        }
        #[cfg(not(all(unix, target_pointer_width = "64")))]
        let _ = (start, end, advice);
    }
}

impl Deref for Mapping {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        match &self.inner {
            #[cfg(all(unix, target_pointer_width = "64"))]
            // SAFETY: ptr/len describe a live read-only mapping owned by self.
            Inner::Mapped { ptr, len } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
            Inner::Owned(v) => v,
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        #[cfg(all(unix, target_pointer_width = "64"))]
        if let Inner::Mapped { ptr, len } = &self.inner {
            // SAFETY: undoing the mmap performed in from_file.
            unsafe {
                sys::munmap(*ptr as *mut _, *len);
            }
        }
    }
}
