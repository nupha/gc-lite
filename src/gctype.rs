// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{alloc::Layout, ptr::NonNull};

use crate::{GcHead, GcNode, GcTrace, trace::GcTraceCtx};

#[derive(Debug, Copy, Clone)]
#[allow(dead_code)]
pub struct GcTypeInfo {
    /// `std::mem::size_of::<T>()` — the Rust size of the payload type T
    pub size: usize,
    /// Byte offset from GcHead to the payload, equals `payload_offset_of::<T>()`
    pub payload_offset: usize,
    /// Total allocation size (GcHead + payload, aligned), equals `layout_size_of::<T>()`
    ///
    /// This value is compile-time constant, set by the `gc_type_table_internal` macro
    /// via `layout_size_of::<T>()`. It must match the Layout used in `alloc_node_mem`.
    pub layout_size: usize,
    /// Allocation alignment, equals `layout_align_of::<T>()`
    ///
    /// This value is compile-time constant, set by the `gc_type_table_internal` macro
    /// via `layout_align_of::<T>()`. It must match the Layout used in `alloc_node_mem`.
    pub layout_align: usize,
    pub trace_fn: fn(NonNull<GcHead>, &mut GcTraceCtx),
    pub drop_fn: Option<unsafe fn(*mut u8)>,
    pub drop_pass: u8,
}

impl GcTypeInfo {
    #[inline(always)]
    pub fn layout(&self) -> Layout {
        #[cfg(debug_assertions)]
        {
            Layout::from_size_align(self.layout_size, self.layout_align).unwrap()
        }

        #[cfg(not(debug_assertions))]
        unsafe {
            Layout::from_size_align_unchecked(self.layout_size, self.layout_align)
        }
    }

    #[inline(always)]
    pub fn payload_ptr(&self, node: NonNull<GcHead>) -> NonNull<u8> {
        unsafe { node.cast::<u8>().add(self.payload_offset) }
    }
}

#[inline(always)]
const fn align_up(value: usize, align: usize) -> usize {
    let mask = align - 1;
    (value + mask) & !mask
}

#[inline(always)]
pub const fn payload_offset_of<T>() -> usize {
    align_up(std::mem::size_of::<GcHead>(), std::mem::align_of::<T>())
}

/// Total allocation size for a GC node of type T: aligned size of (GcHead + payload).
///
/// This is a compile-time constant (`const fn`). The resulting value is stored in
/// `GcTypeInfo::layout_size` by the `gc_type_table_internal` macro, and must match
/// the Layout passed to `std::alloc::alloc` / `std::alloc::dealloc`.
#[inline(always)]
pub const fn layout_size_of<T>() -> usize {
    align_up(
        payload_offset_of::<T>() + std::mem::size_of::<T>(),
        layout_align_of::<T>(),
    )
}

/// Allocation alignment for a GC node of type T: max(GcHead alignment, T alignment).
///
/// This is a compile-time constant (`const fn`). The resulting value is stored in
/// `GcTypeInfo::layout_align` by the `gc_type_table_internal` macro, and must match
/// the Layout passed to `std::alloc::alloc` / `std::alloc::dealloc`.
#[inline(always)]
pub const fn layout_align_of<T>() -> usize {
    let head_align = std::mem::align_of::<GcHead>();
    let payload_align = std::mem::align_of::<T>();
    if head_align > payload_align {
        head_align
    } else {
        payload_align
    }
}

pub fn gctype_trace<T: GcTrace>(node: NonNull<GcHead>, gcx: &mut GcTraceCtx) {
    unsafe {
        node.cast::<u8>()
            .add(payload_offset_of::<T>())
            .cast::<T>()
            .as_ref()
            .trace(gcx);
    }
}

/// Generic dispose function, used to call drop_in_place of specific type
pub fn gctype_drop<T: GcNode>(data_ptr: *mut u8) {
    unsafe { std::ptr::drop_in_place(data_ptr.cast::<T>()) };
}

pub struct GcTypeRegistry {
    pub type_info_list: &'static [GcTypeInfo],
    pub drop_passes: &'static [u8],
}

impl GcTypeRegistry {
    pub const fn empty() -> &'static GcTypeRegistry {
        &GcTypeRegistry {
            type_info_list: &[],
            drop_passes: &[],
        }
    }
}
