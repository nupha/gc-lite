// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    cell::RefCell,
    collections::HashMap,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

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

pub(crate) struct TypeRegistry {
    /// type_info registry list
    types: Vec<GcTypeInfo>,
    /// type ident to idx lookup table
    type_to_idx: HashMap<std::any::TypeId, u8>,

    drop_passes: [u8; 4],
    drop_passes_count: u8,
}

impl TypeRegistry {
    pub(crate) fn new() -> Self {
        let mut entries: Vec<GcTypeInfo> = Vec::with_capacity(8);

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
            types: entries,
            type_to_idx: HashMap::with_capacity(8),
            drop_passes: [0; 4],
            drop_passes_count: 0,
        }
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
            let idx = self.types.len();
            debug_assert!(idx != 0);
            if idx == u8::MAX as usize {
                panic!("too may node types: 255 in max");
            }
            let gc_type_id = idx as u8;

            let info = GcTypeInfo {
                size: std::mem::size_of::<T>() as u32,
                trace_fn: trace_fn::<T>,
                drop_fn: {
                    //Some(drop_fn::<T>)
                    if std::mem::needs_drop::<T>() {
                        Some(drop_fn::<T>)
                    } else {
                        None
                    }
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
            self.types.push(info);
            self.type_to_idx.insert(type_id, gc_type_id);

            self.update_drop_passes();

            gc_type_id
        }
    }

    fn set_drop_pass<T: GcTracable + 'static>(&mut self, pass: u8) {
        debug_assert!(pass < 4);
        if let Some(t) = self.gc_dtype_id::<T>() {
            self.types[t as usize].drop_pass = pass;
            self.update_drop_passes();
        } else {
            self.register::<T>(pass);
        }
    }

    fn update_drop_passes(&mut self) {
        let mut toggles = [false; 4];
        for i in self.types.iter().skip(1).map(|t| t.drop_pass) {
            toggles[i as usize] = true;
        }

        self.drop_passes_count = 0;
        for (i, _) in toggles.iter().enumerate().filter(|(_, b)| **b) {
            self.drop_passes[self.drop_passes_count as usize] = i as u8;
            self.drop_passes_count += 1;
        }
    }

    #[inline(always)]
    pub(crate) fn with_gc_data_types<R>(f: impl FnOnce(&TypeRegistry) -> R) -> R {
        GC_TYPES.with(|s| f(s.borrow().deref()))
    }

    #[inline(always)]
    pub(crate) fn with_gc_data_types_mut<R>(f: impl FnOnce(&mut TypeRegistry) -> R) -> R {
        GC_TYPES.with(|s| f(s.borrow_mut().deref_mut()))
    }

    #[inline(always)]
    pub fn type_id_of<T: GcTracable + 'static>() -> Option<u8> {
        Self::with_gc_data_types(|tt| tt.gc_dtype_id::<T>())
    }

    pub fn set_gc_type_drop_pass<T: GcTracable + 'static>(pass: u8) {
        Self::with_gc_data_types_mut(|tt| {
            tt.set_drop_pass::<T>(pass);
        })
    }

    #[inline]
    pub(crate) fn with_node_gc_type<R>(
        node: NonNull<GcHead>,
        f: impl FnOnce(&GcTypeInfo) -> R,
    ) -> R {
        let id = unsafe { node.as_ref().gc_dtype() } as usize;
        Self::with_gc_data_types(|tt| unsafe {
            debug_assert!(id < tt.types.len());
            f(tt.types.get_unchecked(id))
        })
    }

    #[inline]
    pub(crate) fn gc_type_drop_passes<'a>(passes: &'a mut [u8; 4]) -> &'a [u8] {
        let cnt = Self::with_gc_data_types(|tt| {
            for i in 0..tt.drop_passes_count as usize {
                passes[i] = tt.drop_passes[i];
            }
            tt.drop_passes_count
        });

        &passes[0..cnt as usize]
    }
}

fn noop_trace_fn(_: NonNull<GcHead>, _: GcTraceOp) {}

#[inline(never)]
pub(super) fn trace_fn<T: GcTracable>(node: NonNull<GcHead>, tr: GcTraceOp) {
    unsafe {
        node.as_ref().payload().cast::<T>().as_ref().trace(tr);
    }
}

/// Generic dispose function, used to call drop_in_place of specific type
#[inline(never)]
pub(super) unsafe fn drop_fn<T>(data_ptr: *mut u8) {
    unsafe { std::ptr::drop_in_place(data_ptr.cast::<T>()) };
}

thread_local! {
    static GC_TYPES: RefCell<TypeRegistry> = RefCell::new(TypeRegistry::new());
}

impl GcHeap {
    #[inline(always)]
    pub fn type_id_of<T: GcTracable + 'static>() -> Option<u8> {
        TypeRegistry::type_id_of::<T>()
    }

    #[inline(always)]
    pub fn set_gc_type_drop_pass<T: GcTracable + 'static>(pass: u8) {
        TypeRegistry::set_gc_type_drop_pass::<T>(pass);
    }

    #[inline(always)]
    pub(crate) fn with_node_gc_type<R>(
        node: NonNull<GcHead>,
        f: impl FnOnce(&GcTypeInfo) -> R,
    ) -> R {
        TypeRegistry::with_node_gc_type(node, f)
    }

    #[inline(always)]
    pub(crate) fn gc_type_drop_passes<'a>(passes: &'a mut [u8; 4]) -> &'a [u8] {
        TypeRegistry::gc_type_drop_passes(passes)
    }
}
