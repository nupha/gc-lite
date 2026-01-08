// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{GcHeap, GcRef, node::GcHead};

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
pub struct GcTracer {
    /// GC objects pending Mark (stores raw pointer and type information)
    pub(super) mark_cache: Vec<NonNull<GcHead>>,
}

impl GcTracer {
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            mark_cache: Vec::new(),
        }
    }

    #[inline(always)]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            mark_cache: Vec::with_capacity(cap),
        }
    }

    /// Add a GC reference to pending list
    #[inline(always)]
    pub fn mark<T: GcTracable>(&mut self, gc_ref: GcRef<T>) {
        self.mark_cache.push(gc_ref.head_ptr);
    }

    /// Get next pending header pointer
    #[inline(always)]
    pub(crate) fn next_header(&mut self) -> Option<NonNull<GcHead>> {
        self.mark_cache.pop()
    }

    /// apply pending marks in cache
    #[inline(always)]
    pub fn commit_marks(&mut self, heap: &GcHeap) {
        while let Some(p) = self.mark_cache.pop() {
            unsafe {
                let head = p.as_ptr();
                if !(*head).is_marked() {
                    (*head).set_marked(true);

                    let payload = (head as *mut u8).add(std::mem::size_of::<GcHead>());
                    let trace_fn = (*head).get_trace_fn(&heap.type_registry);
                    trace_fn(payload, self);
                }
            }
        }
    }

    /// clear pending marks
    pub(crate) fn clear(&mut self) {
        self.mark_cache.clear();
    }
}

impl Default for GcTracer {
    fn default() -> Self {
        Self::new()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracer_basic() {
        let mut tracer = GcTracer::new();
        assert!(tracer.mark_cache.is_empty());
        assert_eq!(tracer.next_header(), None);
    }

    #[test]
    fn test_basic_types_collect() {
        let number = 42;
        let string = String::from("test");

        let mut tracer = GcTracer::new();
        number.trace(&mut tracer);
        string.trace(&mut tracer);

        assert!(tracer.mark_cache.is_empty());
    }
}
