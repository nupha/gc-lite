// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{GcHead, GcTracable, trace::GcTraceCtx};

#[derive(Debug, Copy, Clone)]
#[allow(dead_code)]
pub struct GcTypeInfo {
    pub size: u32,
    pub trace_fn: fn(NonNull<GcHead>, &mut GcTraceCtx),
    pub drop_fn: Option<unsafe fn(*mut u8)>,
    pub drop_pass: u8,
}

pub fn trace_fn<T: GcTracable>(node: NonNull<GcHead>, gcx: &mut GcTraceCtx) {
    unsafe {
        node.as_ref().payload().cast::<T>().as_ref().trace(gcx);
    }
}

/// Generic dispose function, used to call drop_in_place of specific type
pub unsafe fn drop_fn<T>(data_ptr: *mut u8) {
    unsafe { std::ptr::drop_in_place(data_ptr.cast::<T>()) };
}
