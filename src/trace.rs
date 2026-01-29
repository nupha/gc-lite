// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{marker::PhantomData, ptr::NonNull};

use crate::{GcHeap, GcPartitionId, GcRef, node::GcHead};

/// Garbage collection object tracing trait
///
/// Any type that wants to be managed by the garbage collection system must implement this trait.
/// This ensures that only types that explicitly support garbage collection can be allocated.
pub unsafe trait GcTracable: 'static {
    /// Trace all GC references within the object
    ///
    /// This method is called during the marking phase to find all other GC objects referenced within the object.
    /// Implementers should call the `trace` closure to mark all referenced objects.
    fn trace(&self, tracer: &mut GcTracer);
}

/// Tracer, used to trace object references during marking phase
pub struct GcTracer<'a> {
    pub(super) heap: NonNull<GcHeap>,
    pub(super) partition_id: GcPartitionId,

    /// pending nodes to be marked later
    pub(super) pendings: Vec<NonNull<GcHead>>,

    _mark: PhantomData<&'a ()>,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GcTraceOp {
    Stop,
    TraceInto,
    TraceLater,
}

impl GcTracer<'_> {
    /// trace handler to mark node
    #[allow(non_snake_case)]
    pub fn MARK_FUNC(mut h: NonNull<GcHead>) -> GcTraceOp {
        unsafe {
            if !h.as_ref().is_marked() {
                h.as_mut().set_marked(true);
                GcTraceOp::TraceInto
            } else {
                GcTraceOp::Stop
            }
        }
    }

    #[inline(always)]
    pub fn new(heap: &GcHeap, partition_id: GcPartitionId) -> Self {
        Self {
            heap: NonNull::from(heap),
            partition_id,
            pendings: Vec::new(),
            _mark: PhantomData,
        }
    }

    #[inline(always)]
    pub fn with_capacity(heap: &GcHeap, partition_id: GcPartitionId, cap: usize) -> Self {
        Self {
            heap: NonNull::from(heap),
            partition_id,
            pendings: Vec::with_capacity(cap),
            _mark: PhantomData,
        }
    }

    pub fn trace(&mut self, node: NonNull<GcHead>, handle: impl Fn(NonNull<GcHead>) -> GcTraceOp) {
        let tt = unsafe { &self.heap.as_ref().type_registry };

        unsafe {
            if node.as_ref().get_partition_id() == self.partition_id {
                match handle(node) {
                    GcTraceOp::TraceInto => {
                        let trace_fn = node.as_ref().get_trace_fn(tt);
                        let payload = node.cast::<u8>().add(std::mem::size_of::<GcHead>());
                        trace_fn(payload.as_ptr(), self);
                    }
                    GcTraceOp::TraceLater => {
                        self.pendings.push(node);
                    }
                    GcTraceOp::Stop => {}
                }
            }
        }

        if !self.pendings.is_empty() {
            self.commit_with(handle);
        }
    }

    pub fn trace_iter(
        &mut self,
        iter: impl Iterator<Item = NonNull<GcHead>>,
        handle: impl Fn(NonNull<GcHead>) -> GcTraceOp,
    ) {
        let tt = unsafe { &self.heap.as_ref().type_registry };

        for ptr in iter {
            unsafe {
                if ptr.as_ref().get_partition_id() == self.partition_id {
                    match handle(ptr) {
                        GcTraceOp::TraceInto => {
                            let trace_fn = ptr.as_ref().get_trace_fn(tt);
                            let payload = ptr.cast::<u8>().add(std::mem::size_of::<GcHead>());
                            trace_fn(payload.as_ptr(), self);
                        }
                        GcTraceOp::TraceLater => {
                            self.pendings.push(ptr);
                        }
                        GcTraceOp::Stop => {}
                    }
                }
            }
        }

        if !self.pendings.is_empty() {
            self.commit_with(handle);
        }
    }

    /// commit to trace pending nodes
    fn commit_with(&mut self, handle: impl Fn(NonNull<GcHead>) -> GcTraceOp) {
        let tt = unsafe { &self.heap.as_ref().type_registry };

        while let Some(ptr) = self.pendings.pop() {
            unsafe {
                debug_assert_eq!(ptr.as_ref().get_partition_id(), self.partition_id);

                match handle(ptr) {
                    GcTraceOp::TraceInto => {
                        let trace_fn = ptr.as_ref().get_trace_fn(tt);
                        let payload = ptr.cast::<u8>().add(std::mem::size_of::<GcHead>());
                        trace_fn(payload.as_ptr(), self);
                    }
                    GcTraceOp::TraceLater => {
                        self.pendings.push(ptr);
                    }
                    GcTraceOp::Stop => {}
                }
            }
        }
    }

    #[inline(always)]
    pub fn trace_roots(&mut self, handle: impl Fn(NonNull<GcHead>) -> GcTraceOp) {
        if let Some(roots) = unsafe { self.heap.as_ref().partition_roots.get(&self.partition_id) } {
            self.trace_iter(roots.iter().copied(), &handle);
        }
    }

    /// Add a node to be traced later
    #[inline(always)]
    pub fn add<T: GcTracable>(&mut self, gc_ref: GcRef<T>) {
        if unsafe { gc_ref.head_ptr().as_ref().get_partition_id() } == self.partition_id {
            self.pendings.push(gc_ref.head_ptr);
        }
    }

    #[deprecated(note = "use ::add() instead")]
    #[inline(always)]
    pub fn mark<T: GcTracable>(&mut self, gc_ref: GcRef<T>) {
        self.add(gc_ref);
    }

    /// commit to trace pending nodes
    #[deprecated]
    pub fn commit(&mut self) {
        self.commit_with(Self::MARK_FUNC);
    }

    /// clear pendings
    pub(crate) fn clear(&mut self) {
        self.pendings.clear();
    }
}

#[macro_export]
macro_rules! impl_collect_for_basic {
    ($($ty:ty),*) => {
        $(
            unsafe impl GcTracable for $ty {
                #[inline(always)]
                fn trace(&self, _: &mut GcTracer) {
                    // This type doesn't contain any GC references, so trace method is empty
                }
            }
        )*
    };
}

// Implement GcTracable for basic types
impl_collect_for_basic!(
    u8,
    u16,
    u32,
    u64,
    u128,
    i8,
    i16,
    i32,
    i64,
    i128,
    f32,
    f64,
    usize,
    isize,
    bool,
    char,
    str,
    ()
);

unsafe impl GcTracable for String {
    #[inline(always)]
    fn trace(&self, _tracer: &mut GcTracer) {
        // String don't have gc ref
    }
}

unsafe impl<T: GcTracable> GcTracable for Option<T> {
    #[inline(always)]
    fn trace(&self, tracer: &mut GcTracer) {
        if let Some(v) = self {
            v.trace(tracer);
        }
    }
}

unsafe impl<T: GcTracable> GcTracable for Box<T> {
    #[inline(always)]
    fn trace(&self, tracer: &mut GcTracer) {
        self.as_ref().trace(tracer);
    }
}
