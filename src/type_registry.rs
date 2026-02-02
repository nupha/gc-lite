// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::HashMap, ptr::NonNull};

use crate::{GcHead, GcHeap, GcTracable, trace::GcTraceOp};

#[derive(Debug)]
pub struct GcTypeInfo {
    pub size: u32,
    pub(super) trace_fn: fn(NonNull<GcHead>, GcTraceOp),
    pub(super) dispose_fn: Option<unsafe fn(*mut u8)>,

    #[cfg(debug_assertions)]
    pub type_id: std::any::TypeId,
    #[cfg(debug_assertions)]
    pub type_name: &'static str,
}

/// Type registry
pub(crate) struct TypeRegistry {
    /// type_info registry list
    entries: Vec<GcTypeInfo>,
    /// type ident to idx lookup table
    type_to_idx: HashMap<std::any::TypeId, u8>,
}

impl TypeRegistry {
    pub(crate) fn new() -> Self {
        let mut entries: Vec<GcTypeInfo> = Vec::with_capacity(16);

        // type slot #0 is not used.
        entries.push(GcTypeInfo {
            size: 0,
            trace_fn: noop_trace_fn,
            dispose_fn: None,

            #[cfg(debug_assertions)]
            type_id: std::any::TypeId::of::<()>(),
            #[cfg(debug_assertions)]
            type_name: "",
        });

        Self {
            entries,
            type_to_idx: HashMap::with_capacity(8),
        }
    }

    #[inline(always)]
    pub fn gc_type_of<T: GcTracable + 'static>(&self) -> Option<u8> {
        self.type_to_idx.get(&std::any::TypeId::of::<T>()).copied()
    }

    /// Register new type
    pub(crate) fn register<T: GcTracable + 'static>(&mut self) -> u8 {
        let type_id = std::any::TypeId::of::<T>();
        let type_name = std::any::type_name::<T>();

        if let Some(&idx) = self.type_to_idx.get(&type_id) {
            idx
        } else {
            // Create type entry
            let idx = self.entries.len();
            debug_assert!(idx != 0);
            if idx == u8::MAX as usize {
                panic!("too may node types: 255 in max");
            }
            let gc_type_id = idx as u8;

            let info = GcTypeInfo {
                size: std::mem::size_of::<T>() as u32,
                trace_fn: trace_fn::<T>,
                dispose_fn: if std::mem::needs_drop::<T>() {
                    Some(dispose_fn::<T>)
                } else {
                    None
                },

                #[cfg(debug_assertions)]
                type_id,
                #[cfg(debug_assertions)]
                type_name,
            };
            self.entries.push(info);
            self.type_to_idx.insert(type_id, gc_type_id);

            gc_type_id
        }
    }

    /// Get type information by type index
    #[inline(always)]
    pub(crate) fn with_type_id<R>(
        &self,
        type_id: u8,
        f: impl FnOnce(&GcTypeInfo) -> R,
    ) -> Option<R> {
        debug_assert!(type_id != 0);
        self.entries.get(type_id as usize).map(f)
    }
}

fn noop_trace_fn(_: NonNull<GcHead>, _: GcTraceOp) {}

pub(super) fn trace_fn<T: GcTracable>(node: NonNull<GcHead>, tr: GcTraceOp) {
    unsafe {
        node.as_ref().payload().cast::<T>().as_ref().trace(tr);
    }
}

/// Generic dispose function, used to call drop_in_place of specific type
pub(super) unsafe fn dispose_fn<T>(data_ptr: *mut u8) {
    unsafe { std::ptr::drop_in_place(data_ptr.cast::<T>()) };
}

impl GcHeap {
    #[inline(always)]
    pub fn type_id_of<T: GcTracable + 'static>(&self) -> Option<u8> {
        self.type_registry.gc_type_of::<T>()
    }

    pub fn get_node_gc_type(&self, node: NonNull<GcHead>) -> &GcTypeInfo {
        unsafe { &self.type_registry.entries[node.as_ref().gc_type_id() as usize] }
    }
}
