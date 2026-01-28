// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    alloc::{Layout, alloc, dealloc},
    ptr::NonNull,
};

pub(crate) struct GcAllocator;

impl GcAllocator {
    /// Allocate memory of specified size
    pub fn allocate(size: usize) -> Option<NonNull<u8>> {
        if size == 0 {
            None
        } else {
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
    }

    /// Deallocate memory
    pub fn deallocate(ptr: NonNull<u8>, size: usize) {
        if size != 0 {
            let layout = Layout::from_size_align(size, std::mem::align_of::<usize>())
                .expect("Invalid layout");

            unsafe {
                // for debug: set mem to zeros before dealloc
                #[cfg(debug_assertions)]
                std::ptr::write_bytes(ptr.as_ptr(), 0, size);

                dealloc(ptr.as_ptr(), layout);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocate_deallocate() {
        let ptr = GcAllocator::allocate(100).unwrap();
        GcAllocator::deallocate(ptr, 100);
    }

    #[test]
    fn test_zero_allocation() {
        debug_assert!(GcAllocator::allocate(0).is_none());
    }
}
