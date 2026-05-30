// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{alloc::Layout, ptr::NonNull};

use crate::{GcHead, GcNode, GcTrace, trace::GcTraceCtx};

#[derive(Debug, Copy, Clone)]
#[allow(dead_code)]
pub struct GcTypeInfo {
    pub size: usize,
    pub payload_offset: usize,
    pub layout_size: usize,
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

#[inline(always)]
pub const fn layout_size_of<T>() -> usize {
    align_up(
        payload_offset_of::<T>() + std::mem::size_of::<T>(),
        layout_align_of::<T>(),
    )
}

pub fn trace_fn<T: GcTrace>(node: NonNull<GcHead>, gcx: &mut GcTraceCtx) {
    unsafe {
        node.cast::<u8>()
            .add(payload_offset_of::<T>())
            .cast::<T>()
            .as_ref()
            .trace(gcx);
    }
}

/// Generic dispose function, used to call drop_in_place of specific type
pub fn drop_fn<T: GcNode>(data_ptr: *mut u8) {
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
