// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    alloc::{Layout, alloc, dealloc},
    ptr::NonNull,
};

pub(crate) struct GcAllocator;

impl GcAllocator {
    pub(crate) fn allocate(size: usize) -> Option<NonNull<u8>> {
        debug_assert_ne!(size, 0);

        let layout = Layout::from_size_align(size, std::mem::align_of::<usize>()).ok()?;
        unsafe {
            let ptr = alloc(layout);
            if ptr.is_null() {
                None
            } else {
                Some(NonNull::new_unchecked(ptr))
            }
        }
    }

    pub(crate) fn deallocate(ptr: NonNull<u8>, size: usize) {
        debug_assert_ne!(size, 0);

        #[cfg(debug_assertions)]
        eprintln!("[DEALLOCATE] called: ptr={:p}, size={}", ptr, size);

        let ly = Layout::from_size_align(size, std::mem::align_of::<usize>());

        #[cfg(debug_assertions)]
        let layout = ly.unwrap();
        #[cfg(not(debug_assertions))]
        let layout = unsafe { ly.unwrap_unchecked() };

        unsafe {
            // debug: set mem to zeros before dealloc,
            // so that node MAGIC_NUM flag will be cleared, which marks the node invalid.
            #[cfg(debug_assertions)]
            {
                eprintln!("[DEALLOCATE] zeroing memory: ptr={:p}, size={}", ptr, size);
                std::ptr::write_bytes(ptr.as_ptr(), 0, size);
            }

            dealloc(ptr.as_ptr(), layout);
        }
    }
}
