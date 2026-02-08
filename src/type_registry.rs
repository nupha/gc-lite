// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::HashMap, ptr::NonNull};

use crate::{GcHead, GcHeap, GcTracable, trace::GcTraceOp};

#[derive(Debug)]
pub struct GcTypeInfo {
    pub size: u32,
    pub(super) trace_fn: fn(NonNull<GcHead>, GcTraceOp),
    pub(super) drop_fn: Option<unsafe fn(*mut u8)>,
    pub(super) drop_pass: u8,

    #[cfg(debug_assertions)]
    pub type_id: std::any::TypeId,
    #[cfg(debug_assertions)]
    pub type_name: &'static str,
}

/// Data type info manager
pub(crate) struct TypeRegistry {
    /// type_info registry list
    entries: Vec<GcTypeInfo>,
    /// type ident to idx lookup table
    type_to_idx: HashMap<std::any::TypeId, u8>,

    drop_passes: [u8; 4],
    drop_passes_count: u8,
}

impl TypeRegistry {
    pub(crate) fn new() -> Self {
        let mut entries: Vec<GcTypeInfo> = Vec::with_capacity(16);

        // type slot #0 is not used.
        entries.push(GcTypeInfo {
            size: 0,
            trace_fn: noop_trace_fn,
            drop_fn: None,
            drop_pass: 0,

            #[cfg(debug_assertions)]
            type_id: std::any::TypeId::of::<()>(),
            #[cfg(debug_assertions)]
            type_name: "",
        });

        Self {
            entries,
            type_to_idx: HashMap::with_capacity(8),
            drop_passes: [0; 4],
            drop_passes_count: 0,
        }
    }

    pub(crate) fn gc_dtype_info<T: GcTracable + 'static>(&self) -> Option<&GcTypeInfo> {
        self.type_to_idx
            .get(&std::any::TypeId::of::<T>())
            .and_then(|&i| self.entries.get(i as usize))
    }

    #[inline(always)]
    pub fn gc_dtype_id<T: GcTracable + 'static>(&self) -> Option<u8> {
        self.type_to_idx.get(&std::any::TypeId::of::<T>()).copied()
    }

    /// Register new type
    pub(crate) fn register<T: GcTracable + 'static>(&mut self, drop_pass: u8) -> u8 {
        debug_assert!(matches!(drop_pass, 0 | 1 | 2 | 3));

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
                drop_fn: {
                    Some(dispose_fn::<T>)
                    // if std::mem::needs_drop::<T>() {
                    //     Some(dispose_fn::<T>)
                    // } else {
                    //     None
                    // }
                },
                drop_pass: if std::mem::needs_drop::<T>() {
                    drop_pass
                } else {
                    0
                },

                #[cfg(debug_assertions)]
                type_id,
                #[cfg(debug_assertions)]
                type_name,
            };
            self.entries.push(info);
            self.type_to_idx.insert(type_id, gc_type_id);

            self.update_drop_passes();

            gc_type_id
        }
    }

    fn set_drop_pass<T: GcTracable + 'static>(&mut self, pass: u8) {
        debug_assert!(pass < 4);
        if let Some(t) = self.gc_dtype_id::<T>() {
            self.entries[t as usize].drop_pass = pass;
            self.update_drop_passes();
        } else {
            self.register::<T>(pass);
        }
    }

    fn update_drop_passes(&mut self) {
        let mut toggles = [false; 4];
        for i in self.entries.iter().skip(1).map(|t| t.drop_pass) {
            toggles[i as usize] = true;
        }

        self.drop_passes_count = 0;
        for (i, _) in toggles.iter().enumerate().filter(|(_, b)| **b) {
            self.drop_passes[self.drop_passes_count as usize] = i as u8;
            self.drop_passes_count += 1;
        }
    }
}

fn noop_trace_fn(_: NonNull<GcHead>, _: GcTraceOp) {}

pub(super) fn trace_fn<T: GcTracable>(node: NonNull<GcHead>, tr: GcTraceOp) {
    unsafe {
        node.as_ref().payload().cast::<T>().as_ref().trace(tr);
    }
}

/// Generic dispose function, used to call drop_in_place of specific type
#[inline(never)]
pub(super) unsafe fn dispose_fn<T>(data_ptr: *mut u8) {
    unsafe { std::ptr::drop_in_place(data_ptr.cast::<T>()) };
}

impl GcHeap {
    #[inline(always)]
    pub fn type_id_of<T: GcTracable + 'static>(&self) -> Option<u8> {
        self.gc_data_types.gc_dtype_id::<T>()
    }

    pub fn set_gc_type_drop_order<T: GcTracable + 'static>(&mut self, pass: u8) {
        self.gc_data_types.set_drop_pass::<T>(pass);
    }

    #[inline]
    pub(crate) fn get_node_gc_type(&self, node: NonNull<GcHead>) -> &GcTypeInfo {
        #[cfg(debug_assertions)]
        {
            &self.gc_data_types.entries[unsafe { node.as_ref().gc_dtype() as usize }]
        }

        #[cfg(not(debug_assertions))]
        unsafe {
            self.gc_data_types
                .entries
                .get_unchecked(node.as_ref().gc_dtype() as usize)
        }
    }

    #[inline]
    pub(crate) fn gc_type_drop_passes<'a>(&self, passes: &'a mut [u8; 4]) -> &'a [u8] {
        for i in 0..self.gc_data_types.drop_passes_count as usize {
            passes[i] = self.gc_data_types.drop_passes[i];
        }

        &passes[0..self.gc_data_types.drop_passes_count as usize]
    }
}
