// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::collections::HashMap;

use crate::{GcHeap, GcTracable, GcTracer};

#[derive(Debug)]
pub(super) struct TypeInfo {
    pub(super) size: u32,
    pub(super) trace_fn: unsafe fn(*mut u8, &mut GcTracer),
    pub(super) dispose_fn: Option<unsafe fn(*mut u8)>,

    #[cfg(debug_assertions)]
    pub(super) type_name: &'static str,
}

/// Type registry
pub(crate) struct TypeRegistry {
    /// type_info registry list
    entries: Vec<TypeInfo>,
    /// type ident to idx lookup table
    type_to_idx: HashMap<std::any::TypeId, u8>,
}

impl TypeRegistry {
    pub(crate) fn new() -> Self {
        let mut entries: Vec<TypeInfo> = Vec::with_capacity(16);

        // type slot #0 is not used.
        entries.push(TypeInfo {
            type_name: "",
            size: 0,
            trace_fn: noop_trace_fn,
            dispose_fn: None,
        });

        Self {
            entries,
            type_to_idx: HashMap::with_capacity(8),
        }
    }

    #[inline(always)]
    pub fn type_id_of<T: GcTracable + 'static>(&self) -> Option<u8> {
        self.type_to_idx.get(&std::any::TypeId::of::<T>()).copied()
    }

    /// Register new type
    pub(crate) fn register<T: GcTracable + 'static>(&mut self) -> u8 {
        let type_ident = std::any::TypeId::of::<T>();
        let type_name = std::any::type_name::<T>();

        if let Some(&idx) = self.type_to_idx.get(&type_ident) {
            idx
        } else {
            // Create type entry
            let type_idx = self.entries.len();
            debug_assert!(type_idx != 0);
            if type_idx == u8::MAX as usize {
                panic!("too may node types: 255 in max");
            }
            let type_idx = type_idx as u8;

            let info = TypeInfo {
                size: std::mem::size_of::<T>() as u32,
                trace_fn: trace_fn::<T>,
                dispose_fn: if std::mem::needs_drop::<T>() {
                    Some(dispose_fn::<T>)
                } else {
                    None
                },
                #[cfg(debug_assertions)]
                type_name,
            };
            self.entries.push(info);
            self.type_to_idx.insert(type_ident, type_idx);

            type_idx
        }
    }

    /// Get type information by type index
    #[inline(always)]
    pub(crate) fn with_type_id<R>(&self, type_id: u8, f: impl FnOnce(&TypeInfo) -> R) -> Option<R> {
        debug_assert!(type_id != 0);
        self.entries.get(type_id as usize).map(f)
    }
}

/// Generic trace function, used to call trace method of specific type
unsafe fn trace_fn<T: GcTracable>(data_ptr: *mut u8, tracer: &mut GcTracer) {
    let typed_ref: &T = unsafe { &*data_ptr.cast::<T>() };
    typed_ref.trace(tracer);
}

unsafe fn noop_trace_fn(_: *mut u8, _: &mut GcTracer) {}

/// Generic dispose function, used to call drop_in_place of specific type
unsafe fn dispose_fn<T>(data_ptr: *mut u8) {
    unsafe { std::ptr::drop_in_place(data_ptr.cast::<T>()) };
}

impl GcHeap {
    #[inline(always)]
    pub fn type_id_of<T: GcTracable + 'static>(&self) -> Option<u8> {
        self.type_registry.type_id_of::<T>()
    }
}
