// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::collections::HashMap;

use crate::{GcHeap, GcTracable, GcTracer};

#[derive(Debug)]
pub(super) struct TypeInfo {
    pub(super) type_name: &'static str,
    pub(super) size: usize,
    pub(super) needs_drop: bool,
    pub(super) trace_fn: unsafe fn(*mut u8, &mut GcTracer),
    pub(super) dispose_fn: unsafe fn(*mut u8),
}

/// Type registry
pub(crate) struct TypeRegistry {
    /// type_info registry list
    entries: Vec<TypeInfo>,
    /// type ident to idx lookup table
    type_to_id: HashMap<std::any::TypeId, u16>,
    /// type name to type id lookup table
    type_name_to_id: HashMap<&'static str, u16>,
}

impl TypeRegistry {
    pub(crate) fn new() -> Self {
        let mut entries: Vec<TypeInfo> = Vec::with_capacity(16);

        // type index #0 is not used.
        entries.push(TypeInfo {
            type_name: "",
            size: 0,
            needs_drop: false,
            trace_fn: noop_trace_fn,
            dispose_fn: noop_dispose_fn,
        });

        Self {
            entries,
            type_to_id: HashMap::new(),
            type_name_to_id: HashMap::new(),
        }
    }

    #[inline(always)]
    pub fn type_idx_of<T: GcTracable + 'static>(&self) -> Option<u16> {
        let type_ident = std::any::TypeId::of::<T>();
        self.type_to_id.get(&type_ident).copied()
    }

    /// Register new type
    fn register<T: GcTracable + 'static>(&mut self) -> u16 {
        let type_ident = std::any::TypeId::of::<T>();
        let type_name = std::any::type_name::<T>();

        if let Some(&idx) = self.type_to_id.get(&type_ident) {
            idx
        } else {
            // Create type entry
            let type_idx = self.entries.len() as u16;
            debug_assert!(type_idx != 0);

            let info = TypeInfo {
                type_name,
                size: std::mem::size_of::<T>(),
                needs_drop: std::mem::needs_drop::<T>(),
                trace_fn: trace_fn::<T>,
                dispose_fn: if std::mem::needs_drop::<T>() {
                    dispose_fn::<T>
                } else {
                    noop_dispose_fn
                },
            };
            self.entries.push(info);
            self.type_to_id.insert(type_ident, type_idx);
            self.type_name_to_id.insert(type_name, type_idx);

            type_idx
        }
    }

    /// Get or register type
    #[inline(always)]
    pub(crate) fn get_or_register<T: GcTracable + 'static>(&mut self) -> u16 {
        self.register::<T>()
    }

    /// Get type information by type index
    #[inline(always)]
    pub(crate) fn with_idx<R>(&self, type_idx: u16, f: impl FnOnce(&TypeInfo) -> R) -> Option<R> {
        debug_assert!(type_idx != 0);
        self.entries.get(type_idx as usize).map(f)
    }
}

/// Generic trace function, used to call trace method of specific type
pub(super) unsafe fn trace_fn<T: GcTracable>(data_ptr: *mut u8, tracer: &mut GcTracer) {
    let typed_ref: &T = unsafe { &*data_ptr.cast::<T>() };
    typed_ref.trace(tracer);
}

pub(super) unsafe fn noop_trace_fn(_: *mut u8, _: &mut GcTracer) {}

/// Generic dispose function, used to call drop_in_place of specific type
pub(super) unsafe fn dispose_fn<T>(data_ptr: *mut u8) {
    let typed_ptr = data_ptr as *mut T;
    unsafe { std::ptr::drop_in_place(typed_ptr) };
}

/// Empty dispose function, for types that don't need Drop
pub(super) unsafe fn noop_dispose_fn(_data_ptr: *mut u8) {
    // Do nothing
}

impl GcHeap {
    #[inline(always)]
    pub fn type_idx_of<T: GcTracable + 'static>(&self) -> Option<u16> {
        self.type_registry.type_idx_of::<T>()
    }
}
