// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

pub mod gctype;
mod heap;
mod helpers;
mod mark_sweep;
mod mem;
mod node;
mod node_link;
mod partition;
mod scope;
mod trace;
mod weak;
mod xref;

#[cfg(feature = "gc_arena")]
mod arena;

pub use {
    gctype::{GcTypeInfo, GcTypeRegistry, gctype_drop, gctype_trace},
    heap::GcHeap,
    helpers::{GcError, GcResult},
    node::{Gc, GcHead, GcNode, GcRef},
    node_link::GcNodeLink,
    partition::{GcPartition, GcPartitionId},
    scope::{GcScope, GcScopeStackId, GcScopeState},
    trace::{GcTrace, GcTraceCtx, GcTraceFn},
    weak::GcWeak,
};

#[cfg(feature = "gc_arena")]
pub use arena::{ARENA_CAPACITY, GcArena, MAX_ARENA_ALLOC};

pub(crate) use helpers::unlikely;

/// Derive macro for [`GcTrace`].
///
/// The derive unconditionally emits one `GcTrace::trace` call per field (or
/// per enum-variant field), so **every field is either traced or explicitly
/// opted out** — a field whose type does not implement `GcTrace` (e.g. a raw
/// pointer) is a compile error pointing at that field, never a silent gap.
/// Adding a field later automatically extends the generated trace.
///
/// Field attributes:
/// - `#[gc(skip)]` — do not trace this field (for raw pointers / external
///   handles that must not keep GC nodes alive).
/// - `#[gc(with = "path")]` — call `path(&field, ctx)` instead of the trait
///   impl (for foreign containers like `SmallVec` that gc-lite cannot impl).
///
/// Generic type parameters get a `GcTrace` bound automatically.
///
/// # Examples
///
/// Fields trace through the standard containers; non-GC fields are free
/// (their impls are no-ops); raw pointers must be explicitly skipped:
///
/// ```
/// use gc_lite::{GcHeap, GcRef, GcTrace, gc_type_register};
///
/// #[derive(Debug, GcTrace)]
/// struct Node {
///     next: Option<GcRef<Node>>,   // traced: keeps `next` alive
///     refs: Vec<GcRef<Node>>,      // traced element-wise
///     name: String,                // no-op impl: traced for free
///     #[gc(skip)]
///     raw: *mut u8,                // must be opted out explicitly
/// }
///
/// gc_type_register! { Node; }
///
/// let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
/// let pid = heap.create_partition(64 * 1024, 16 * 1024);
///
/// // c <- b <- a, with `a` also referenced by the root's Vec field
/// let c = Node::alloc_node(&mut heap, pid, Node { next: None, refs: Vec::new(), name: "c".into(), raw: std::ptr::null_mut() }).unwrap();
/// let b = Node::alloc_node(&mut heap, pid, Node { next: Some(c), refs: Vec::new(), name: "b".into(), raw: std::ptr::null_mut() }).unwrap();
/// let a = Node::alloc_node(&mut heap, pid, Node { next: Some(b), refs: vec![c], name: "a".into(), raw: std::ptr::null_mut() }).unwrap();
/// let root = unsafe { heap.alloc_root_raw(pid, Node { next: None, refs: vec![a, b], name: "root".into(), raw: std::ptr::null_mut() }) }.unwrap();
/// drop(root);
///
/// let alive_before = heap.nodes(pid).count();
/// while !heap.mark(pid, 16) {}
/// let freed = sweep(&mut heap, pid);
/// assert_eq!(freed, 0, "derived trace kept the whole chain alive");
/// assert_eq!(heap.nodes(pid).count(), alive_before);
///
/// # fn sweep(heap: &mut GcHeap, pid: gc_lite::GcPartitionId) -> usize {
/// #     if let Some(white) = heap.sweep_unlink(pid) {
/// #         let registry = heap.type_registry();
/// #         GcHeap::sweep_drop_payloads(&white, registry);
/// #         heap.sweep_dispose(pid, white)
/// #     } else { 0 }
/// # }
/// ```
///
/// An untraceable field without `#[gc(skip)]` fails to compile:
///
/// ```compile_fail
/// use gc_lite::GcTrace;
///
/// #[derive(GcTrace)]
/// struct Holder {
///     ptr: *mut u8, // ERROR: `*mut u8` does not implement `GcTrace`
/// }
/// ```
///
/// ```compile_fail
/// use gc_lite::GcTrace;
///
/// struct Opaque(*mut u8);
///
/// #[derive(GcTrace)]
/// struct Holder {
///     bad: Opaque, // ERROR: `Opaque` does not implement `GcTrace`
/// }
/// ```
#[doc(inline)]
pub use gc_lite_macros::{GcTrace, gc_type_table_internal};
