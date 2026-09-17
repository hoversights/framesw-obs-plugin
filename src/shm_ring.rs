// SPDX-License-Identifier: GPL-2.0-or-later
//
// FrameSW Companion Plugin for OBS Studio
// Copyright (C) 2026 Hoversights
//
// This program is free software; you can redistribute it and/or modify it
// under the terms of the GNU General Public License as published by the
// Free Software Foundation; either version 2 of the License, or (at your
// option) any later version.
//
// This program is distributed in the hope that it will be useful, but
// WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
// General Public License for more details.
//
// You should have received a copy of the GNU General Public License along
// with this program; if not, see <https://www.gnu.org/licenses/>.

//! The writer half of FrameSW's Preview/Program video transport: a
//! memory-mapped ring of NV12 frames, one per feed.
//!
//! **Why not NDI** (MONITOR_LAYOUTS_PLAN.md): this plugin is GPL-2.0 because
//! it loads into OBS and calls libobs, and NDI's runtime is closed source —
//! a question this project would rather remove than argue. Both ends of this
//! feed are ours, on one machine, so nothing proprietary needs to be in the
//! path. It is also faster: NDI measured ~22 ms for real picture content on
//! Windows against ~1 ms on the Mac, and a memory copy is content-blind.
//!
//! **Why a mapped file rather than POSIX shared memory**: the same
//! semantics, but both ends open it with ordinary file APIs on both
//! platforms, and a leftover file is inert — the header carries the writer's
//! pid and a heartbeat, so a reader can tell "paused" from "gone" without
//! any cleanup protocol to get wrong.
//!
//! Layout: `Header`, then `slot_count` slots of `slot_bytes` each. A slot is
//! its own `SlotHeader` followed by the NV12 planes, Y then interleaved CbCr,
//! both at `width` stride.
//!
//! Synchronisation is a seqlock per slot, which suits exactly this shape —
//! one writer that must never block (it runs on OBS's video thread) and
//! readers that would rather skip a frame than wait. The writer makes `seq`
//! odd, writes, then makes it even; a reader that sees an odd `seq`, or a
//! different one after copying, throws that read away and takes the next
//! frame. Nobody waits, and a torn frame can never be displayed.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// `FSW1` little-endian — a reader that maps a file from a different build,
/// or any other file, must be able to tell immediately.
pub const MAGIC: u32 = 0x3157_5346;
/// Bumped on any layout change. The reader refuses a version it does not
/// know rather than guessing at offsets.
pub const VERSION: u32 = 1;
/// Three is enough at 30 fps: the writer is a memcpy and the reader wakes on
/// the UI's own frame, so two in flight plus one being written never
/// collide in practice.
pub const SLOT_COUNT: u32 = 3;

/// Fixed-size head of the mapping. `#[repr(C)]` because a Rust struct's
/// field order is not guaranteed and the app maps this same bytes-on-disk
/// layout from a separate binary.
#[repr(C)]
pub struct Header {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    /// `video_format` as libobs numbers it; NV12 is 2. Present so a future
    /// format change is detectable rather than silently misread.
    pub format: AtomicU32,
    pub fps_num: AtomicU32,
    pub fps_den: AtomicU32,
    pub slot_count: AtomicU32,
    pub slot_bytes: AtomicU32,
    pub writer_pid: AtomicU32,
    /// Total frames written. `% slot_count` gives the slot being written;
    /// the newest complete frame is the one before it.
    pub frames: AtomicU64,
    /// Wall-clock of the last write, in 100 ns units since the Unix epoch —
    /// the same unit the NDI path used for its timecode, so latency numbers
    /// stay comparable with Phase 0's.
    pub heartbeat: AtomicU64,
}

/// Head of one slot. The frame's bytes follow immediately.
#[repr(C)]
pub struct SlotHeader {
    /// Seqlock: odd while being written, even when complete.
    pub seq: AtomicU64,
    /// Capture time, 100 ns units since the Unix epoch.
    pub timecode: AtomicU64,
}

pub const HEADER_BYTES: usize = std::mem::size_of::<Header>();
pub const SLOT_HEADER_BYTES: usize = std::mem::size_of::<SlotHeader>();

/// NV12 is one byte per pixel of luma plus half that of interleaved chroma.
pub fn frame_bytes(width: u32, height: u32) -> usize {
    let w = width as usize;
    let h = height as usize;
    w * h + w * (h / 2)
}

pub fn slot_bytes(width: u32, height: u32) -> usize {
    SLOT_HEADER_BYTES + frame_bytes(width, height)
}

pub fn mapping_bytes(width: u32, height: u32) -> usize {
    HEADER_BYTES + SLOT_COUNT as usize * slot_bytes(width, height)
}

/// Where a feed's ring lives. Same rule on both platforms: the user's own
/// temp directory, one file per feed, so two FrameSW users on one machine
/// cannot collide and nothing needs elevated rights.
pub fn ring_path(feed: &str) -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string());
    let safe: String = user.chars().filter(|c| c.is_ascii_alphanumeric()).take(24).collect();
    dir.push(format!("framesw-{safe}-{feed}.ring"));
    dir
}

/// A mapped ring this process writes to.
pub struct RingWriter {
    base: *mut u8,
    len: usize,
    width: u32,
    height: u32,
    slot_bytes: usize,
    path: std::path::PathBuf,
    // Kept alive for the mapping's lifetime on Windows; unused on unix,
    // where the mapping outlives the descriptor.
    #[cfg(target_os = "windows")]
    mapping: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: the pointer is a private mapping owned by this struct; all writes
// go through `&mut self`, and the cross-process synchronisation is the
// seqlock described above.
unsafe impl Send for RingWriter {}

impl RingWriter {
    /// Creates (or re-creates) the ring for `feed` at this size and stamps
    /// the header. Any previous file is replaced: a stale ring from a
    /// crashed run must never be mistaken for this one.
    pub fn create(feed: &str, width: u32, height: u32, fps_num: u32, fps_den: u32) -> Result<Self, String> {
        let path = ring_path(feed);
        let len = mapping_bytes(width, height);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| format!("creating {}: {e}", path.display()))?;
        file.set_len(len as u64).map_err(|e| format!("sizing {}: {e}", path.display()))?;

        let base = map_write(&file, len)?;
        let writer = RingWriter {
            base,
            len,
            width,
            height,
            slot_bytes: slot_bytes(width, height),
            path,
            #[cfg(target_os = "windows")]
            mapping: map_handle(&file, len)?,
        };
        // SAFETY: `base` is a valid mapping of at least `HEADER_BYTES`.
        let header = unsafe { &*(writer.base as *const Header) };
        header.version.store(VERSION, Ordering::Relaxed);
        header.width.store(width, Ordering::Relaxed);
        header.height.store(height, Ordering::Relaxed);
        header.format.store(2, Ordering::Relaxed); // VIDEO_FORMAT_NV12
        header.fps_num.store(fps_num, Ordering::Relaxed);
        header.fps_den.store(fps_den, Ordering::Relaxed);
        header.slot_count.store(SLOT_COUNT, Ordering::Relaxed);
        header.slot_bytes.store(writer.slot_bytes as u32, Ordering::Relaxed);
        header.writer_pid.store(std::process::id(), Ordering::Relaxed);
        header.frames.store(0, Ordering::Relaxed);
        header.heartbeat.store(0, Ordering::Relaxed);
        // Magic last, with a release: a reader that sees it is guaranteed to
        // see a fully-stamped header behind it.
        header.magic.store(MAGIC, Ordering::Release);
        Ok(writer)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Copies one NV12 frame in, straight from libobs's planes — the same
    /// single copy the NDI path made, with no encode after it.
    ///
    /// `y_stride`/`uv_stride` are libobs's own linesizes, which are padded;
    /// the ring stores rows packed at `width`.
    ///
    /// # Safety
    /// `y` and `uv` must be valid for `height`/`height / 2` rows at their
    /// strides — they are libobs's `video_data` planes, valid only inside
    /// the callback that hands them over.
    pub unsafe fn write_frame(&mut self, y: *const u8, y_stride: usize, uv: *const u8, uv_stride: usize, timecode: u64) {
        let w = self.width as usize;
        let h = self.height as usize;
        let header = &*(self.base as *const Header);
        let index = (header.frames.load(Ordering::Relaxed) % SLOT_COUNT as u64) as usize;
        let slot = self.base.add(HEADER_BYTES + index * self.slot_bytes);
        let slot_header = &*(slot as *const SlotHeader);

        // Odd: this slot is being written. A reader that catches it here
        // takes an older slot instead of a half-written frame.
        let seq = slot_header.seq.load(Ordering::Relaxed);
        slot_header.seq.store(seq | 1, Ordering::Release);

        let data = slot.add(SLOT_HEADER_BYTES);
        for row in 0..h {
            std::ptr::copy_nonoverlapping(y.add(row * y_stride), data.add(row * w), w);
        }
        let uv_out = data.add(w * h);
        for row in 0..h / 2 {
            std::ptr::copy_nonoverlapping(uv.add(row * uv_stride), uv_out.add(row * w), w);
        }
        slot_header.timecode.store(timecode, Ordering::Relaxed);

        // Even again, and only now is the frame counted: the release pairs
        // with the reader's acquire so the bytes are visible before the
        // count that advertises them.
        slot_header.seq.store((seq | 1) + 1, Ordering::Release);
        header.frames.fetch_add(1, Ordering::Release);
        header.heartbeat.store(timecode, Ordering::Relaxed);
    }
}

impl Drop for RingWriter {
    fn drop(&mut self) {
        // Clear the magic first: a reader mapping this file during teardown
        // must see "not a ring" rather than a valid header over unmapped
        // memory.
        // SAFETY: still mapped until `unmap` below.
        unsafe {
            (*(self.base as *const Header)).magic.store(0, Ordering::Release);
        }
        unmap(self.base, self.len);
        #[cfg(target_os = "windows")]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.mapping);
        }
        // Best effort: a leftover file is inert (magic is 0), but leaving
        // one behind on every OBS run is untidy.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::c_void;
    use std::os::fd::AsRawFd;

    // `<sys/mman.h>`: PROT_READ 0x1, PROT_WRITE 0x2, MAP_SHARED 0x0001,
    // MAP_FAILED (void*)-1. Declared here rather than taking a `libc`
    // dependency, the same way this crate treats libobs and NDI.
    const PROT_READ: i32 = 0x1;
    const PROT_WRITE: i32 = 0x2;
    const MAP_SHARED: i32 = 0x0001;

    extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> i32;
    }

    pub fn map_write(file: &std::fs::File, len: usize) -> Result<*mut u8, String> {
        // SAFETY: a fresh mapping of a file this process just sized.
        let ptr = unsafe {
            mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, file.as_raw_fd(), 0)
        };
        if ptr.is_null() || ptr as isize == -1 {
            return Err("mmap failed".to_string());
        }
        Ok(ptr as *mut u8)
    }

    pub fn unmap(base: *mut u8, len: usize) {
        // SAFETY: `base`/`len` are exactly what `map_write` returned.
        unsafe {
            munmap(base as *mut c_void, len);
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
    };

    pub fn map_handle(file: &std::fs::File, len: usize) -> Result<HANDLE, String> {
        // SAFETY: a file this process just created and sized.
        let handle = unsafe {
            CreateFileMappingW(
                file.as_raw_handle() as HANDLE,
                std::ptr::null(),
                PAGE_READWRITE,
                ((len as u64) >> 32) as u32,
                (len as u64 & 0xFFFF_FFFF) as u32,
                std::ptr::null(),
            )
        };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err("CreateFileMappingW failed".to_string());
        }
        Ok(handle)
    }

    pub fn map_write(file: &std::fs::File, len: usize) -> Result<*mut u8, String> {
        let handle = map_handle(file, len)?;
        // SAFETY: a mapping handle for exactly `len` bytes.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len) };
        if view.Value.is_null() {
            return Err("MapViewOfFile failed".to_string());
        }
        Ok(view.Value as *mut u8)
    }

    pub fn unmap(base: *mut u8, _len: usize) {
        // SAFETY: `base` is exactly what `map_write` returned.
        unsafe {
            UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: base as *mut core::ffi::c_void,
            });
        }
    }
}

use imp::{map_write, unmap};
#[cfg(windows)]
use imp::map_handle;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_holds_exactly_one_nv12_frame_plus_its_header() {
        // 640x360 NV12 = 230400 luma + 115200 chroma.
        assert_eq!(frame_bytes(640, 360), 345_600);
        assert_eq!(slot_bytes(640, 360), 345_600 + SLOT_HEADER_BYTES);
    }

    #[test]
    fn the_mapping_holds_the_header_and_every_slot() {
        assert_eq!(
            mapping_bytes(640, 360),
            HEADER_BYTES + 3 * (345_600 + SLOT_HEADER_BYTES)
        );
    }

    /// The app maps these same bytes from a separate binary, so the layout
    /// has to be stated, not inferred.
    #[test]
    fn the_header_layout_is_fixed() {
        assert_eq!(HEADER_BYTES, 56);
        assert_eq!(SLOT_HEADER_BYTES, 16);
    }
}
