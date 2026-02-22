// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{GcHead, GcNode, GcTracable, trace::GcTraceCtx};

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
