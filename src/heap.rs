// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ptr::NonNull};

use crate::{
    gctype::GcTypeRegistry,
    node::{GcHead, GcNodeFlag},
    partition::{GcPartition, GcPartitionId},
    scope::{GcScopeStackId, ScopeStack},
};

/// Minimum GC threshold: at least one GcHead plus the smallest possible
/// aligned payload (1 byte, aligned to GcHead's alignment).
const MIN_GC_THRESHOLD: usize = {
    const fn align_up(value: usize, align: usize) -> usize {
        let mask = align - 1;
        (value + mask) & !mask
    }
    let head_size = std::mem::size_of::<GcHead>();
    let min_payload = 1;
    let payload_align = std::mem::align_of::<GcHead>();
    head_size + align_up(min_payload, payload_align)
};

pub struct GcHeap {
    /// Registered GC data type info
    pub(super) node_dtypes: &'static GcTypeRegistry,

    /// Partition storage (indexed by `GcPartitionId`)
    pub(super) partitions: Vec<GcPartition>,
    /// Global memory usage limit for the entire heap, 0 for unlimited
    pub(super) memory_limit: usize,
    /// Global automatic GC threshold for the entire heap, 0 for disabled
    pub(super) gc_threshold: usize,
    /// Total memory used across all partitions
    pub(super) total_memory_used: usize,
    /// Weak reference list, each slot stores (version, GcHeader).
    /// The node pointer is wrapped in `Cell` to allow clearing through `&self`
    /// during `finalize_partition` — preventing use-after-free when a Drop
    /// callback upgrades a weak ref to a node whose payload was already dropped.
    pub(super) weak_slots: Vec<(u8, Cell<Option<NonNull<GcHead>>>)>,
    /// gc scope stack list
    pub(crate) scope_stacks: Vec<ScopeStack>, // DON'T use SmallVec here

    /// User provided opaque raw pointer
    opaque: *mut u8,

    #[cfg(debug_assertions)]
    pub(crate) dbg_dropping_root_partition: Option<GcPartitionId>,
    #[cfg(debug_assertions)]
    pub(crate) dbg_living_nodes: std::collections::HashSet<NonNull<GcHead>>,
}

impl GcHeap {}

impl Drop for GcHeap {
    fn drop(&mut self) {
        // heap world is gone, dealloc all nodes live in it, regardless their status.
        log::trace!("[heap::drop]");

        // Clear all scope caches first to remove LOCAL flags from nodes.
        // This must happen BEFORE disposing partitions so that nodes are not
        // protected by scope and can be reclaimed. GcScopeState::clear() only
        // touches node flags and does not access any GcHeap fields, so it is
        // safe to call during GcHeap::drop.
        for stack in &mut self.scope_stacks {
            for s in stack.list.drain(..) {
                s.clear();
            }
        }

        // Take all nodes from all partitions to avoid self-borrow conflicts
        let all_nodes: Vec<_> = self
            .partitions
            .iter_mut()
            .map(|par| std::mem::take(&mut par.nodes))
            .collect();

        for nodes in all_nodes {
            self.dispose_all_nodes(nodes, Self::DUMMY_DISPOSE_CALLBACK);
        }

        #[cfg(debug_assertions)]
        debug_assert!(
            self.dbg_living_nodes.is_empty(),
            "[O.o][heap drop] leaked nodes {:?}",
            self.dbg_living_nodes
        );
    }
}

impl GcHeap {
    pub const DUMMY_DISPOSE_CALLBACK: fn(&GcHeap, &GcHead) = |_, _| {};

    /// Create a new garbage collection heap with an explicit GC type registry.
    ///
    /// The heap starts with no partitions. Use [`create_partition`](GcHeap::create_partition)
    /// to add partitions as needed.
    pub fn new(registry: &'static GcTypeRegistry) -> Self {
        Self {
            partitions: Vec::with_capacity(1),
            memory_limit: 0,
            gc_threshold: 0,
            total_memory_used: 0,
            weak_slots: Vec::with_capacity(8),
            opaque: std::ptr::null_mut(),
            node_dtypes: registry,
            scope_stacks: vec![ScopeStack::new(None)],

            #[cfg(debug_assertions)]
            dbg_dropping_root_partition: None,
            #[cfg(debug_assertions)]
            dbg_living_nodes: std::collections::HashSet::with_capacity(128),
        }
    }

    #[inline(always)]
    pub const fn opaque(&self) -> *mut u8 {
        self.opaque
    }

    #[inline(always)]
    pub const fn set_opaque(&mut self, opaque: *mut u8) {
        self.opaque = opaque;
    }

    #[inline(always)]
    pub fn memory_limit(&self) -> usize {
        self.memory_limit
    }

    pub fn set_memory_limit(&mut self, limit: usize) -> usize {
        if limit == 0 {
            self.memory_limit = 0;
        } else {
            let used = self.total_memory_used;
            let applied = std::cmp::max(used, limit);
            self.memory_limit = applied;

            if self.gc_threshold > 0 && self.gc_threshold >= applied {
                let adjusted = std::cmp::max(applied - (applied >> 2), MIN_GC_THRESHOLD);
                self.gc_threshold = adjusted;
            }
        }

        self.memory_limit
    }

    #[inline(always)]
    pub fn gc_threshold(&self) -> usize {
        self.gc_threshold
    }

    pub fn set_gc_threshold(&mut self, threshold: usize) -> usize {
        if threshold > 0 && self.memory_limit > 0 {
            let capped = self.memory_limit.saturating_mul(8).saturating_div(10);
            self.gc_threshold = std::cmp::min(threshold, capped);
        } else {
            self.gc_threshold = threshold;
        }

        self.gc_threshold
    }

    /// Check if garbage collection is needed
    #[inline(always)]
    pub fn should_gc(&self) -> bool {
        // If GC threshold > 0 and memory usage reaches threshold, trigger GC
        // gc_threshold = 0 means automatic GC is disabled
        self.gc_threshold > 0 && self.total_memory_used >= self.gc_threshold
    }

    /// Attach a node to partition's nodes chain.
    ///
    /// This method does NOT update memory accounting — the caller is responsible
    /// for calling `update_mem_use` separately (typically done in `alloc_node_mem`).
    pub(crate) fn attach_node(&mut self, partition_id: GcPartitionId, mut node: NonNull<GcHead>) {
        let n = unsafe { node.as_mut() };
        debug_assert!(n.next.is_none());
        debug_assert_eq!(
            n.partition_id(),
            partition_id,
            "attach_node: node partition_id doesn't match"
        );

        let par = &mut self.partitions[partition_id.0 as usize];
        let mem_before = par.memory_used;
        par.nodes.prepend(node);
        debug_assert_eq!(
            par.memory_used, mem_before,
            "attach_node must not change memory accounting"
        );
    }

    pub fn set_root_node(&mut self, mut node: NonNull<GcHead>) {
        let n = unsafe { node.as_mut() };
        if !n.is_root() {
            n.insert_flag(GcNodeFlag::ROOT);
            let pid = n.partition_id();
            let par = &mut self.partitions[pid.0 as usize];
            if par.is_marking() {
                par.add_gray_node(node);
            }
        }
    }

    /// Check if `node` was allocated in this heap
    pub fn contains(&self, node: NonNull<GcHead>) -> bool {
        let pid = unsafe { node.as_ref().partition_id() };
        self.partitions
            .get(pid.0 as usize)
            .is_some_and(|par| par.nodes.iter().any(|p| p == node))
    }

    /// Protect node from being gc collected.
    ///
    /// 1. if node is local or root, it's protected, returns true
    /// 1. otherwise if has current scope, add node to current scope and returns true
    /// 1. can't protect, returns false
    pub fn protect_node(&mut self, scope_stack_id: GcScopeStackId, node: NonNull<GcHead>) -> bool {
        self.current_scope(scope_stack_id)
            .is_some_and(|s| s.add_non_local(node))
    }

    /// Protect nodes from being gc collected, for each node do following steps:
    ///
    /// 1. if node is local or root, do nothing
    /// 1. if has current scope, add node to current scope
    /// 1. can't protect, returns false
    pub fn protect_nodes_iter(
        &mut self,
        scope_stack_id: GcScopeStackId,
        nodes: impl Iterator<Item = NonNull<GcHead>>,
    ) {
        if let Some(s) = self.current_scope(scope_stack_id) {
            for n in nodes {
                s.add_non_local(n);
            }
        }
    }

    /// Protect nodes from being gc collected, for each node do following steps:
    ///
    /// 1. if node is local or root, do nothing
    /// 1. if has current scope, add node to current scope
    /// 1. can't protect, returns false
    pub fn protect_nodes(&mut self, scope_stack_id: GcScopeStackId, nodes: &[NonNull<GcHead>]) {
        self.protect_nodes_iter(scope_stack_id, nodes.iter().copied());
    }

    /// Update memory usage with rollup to parent partitions
    pub(crate) fn update_mem_use(&mut self, id: GcPartitionId, delta: i32) -> usize {
        let par = &mut self.partitions[id.0 as usize];
        if delta >= 0 {
            let d = delta as usize;
            par.memory_used += d;
            self.total_memory_used += d;
            par.memory_used
        } else {
            let d = (-delta) as usize;
            debug_assert!(
                par.memory_used >= d,
                "update_mem_use: partition memory underflow ({} < {})",
                par.memory_used,
                d,
            );
            debug_assert!(
                self.total_memory_used >= d,
                "update_mem_use: global memory underflow ({} < {})",
                self.total_memory_used,
                d,
            );
            par.memory_used -= d;
            self.total_memory_used -= d;
            par.memory_used
        }
    }

    #[inline(always)]
    pub const fn memory_used(&self) -> usize {
        self.total_memory_used
    }
}

#[cfg(test)]
mod heap_tests {
    use crate::arena::{ARENA_CAPACITY, MAX_ARENA_ALLOC};
    use crate::{GcRef, GcTraceCtx, node::GcNode, trace::GcTrace};

    use super::*;

    #[derive(Debug)]
    struct Node {
        next: Option<GcRef<Node>>,
        #[expect(dead_code)]
        value: i32,
    }

    impl GcTrace for Node {
        fn trace(&self, tr: &mut GcTraceCtx) {
            if let Some(next) = self.next {
                tr.add(next);
            }
        }
    }

    crate::gc_type_register! {
        Node, drop_pass = 0;
    }

    #[test]
    fn test_heap_with_context_alloc_and_cleanup() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(ARENA_CAPACITY, MAX_ARENA_ALLOC);
        let stack_id = heap.acquire_scope_stack(partition_id);

        let _head = heap.with_new_scope(stack_id, |ctx| {
            let node = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();
            ctx.clear();
            node.gc_head_ptr()
        });

        while !heap.mark(partition_id, 64) {}
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after > 0);
    }

    #[test]
    fn test_memory_used_symmetry_alloc_dispose() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let id = heap.create_partition(ARENA_CAPACITY, MAX_ARENA_ALLOC);

        let mem_before = heap.memory_used();
        let par_mem_before = heap.partition(id).unwrap().memory_used();

        let node = unsafe {
            heap.alloc_raw(
                id,
                Node {
                    next: None,
                    value: 42,
                },
            )
        }
        .unwrap();
        let gross_size = heap.memory_used() - mem_before;

        assert!(gross_size > 0);
        assert_eq!(
            heap.partition(id).unwrap().memory_used() - par_mem_before,
            gross_size
        );

        // Use two-phase partition removal to cleanly dispose all nodes and reclaim memory
        let link = heap.finalize_partition(id).unwrap();
        let freed = heap.dealloc_partition(id, link);
        assert_eq!(freed, gross_size);

        let mem_after = heap.memory_used();
        assert_eq!(
            mem_after, mem_before,
            "global memory should return to original after partition removal"
        );
    }

    #[test]
    fn test_memory_used_symmetry_sweep() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let id = heap.create_partition(ARENA_CAPACITY, MAX_ARENA_ALLOC);

        // Allocate 3 non-root nodes and 1 root node
        let mem_before = heap.memory_used();
        let par_mem_before = heap.partition(id).unwrap().memory_used();

        for i in 0..3 {
            unsafe {
                heap.alloc_raw(
                    id,
                    Node {
                        next: None,
                        value: i,
                    },
                )
            }
            .unwrap();
        }
        let root = unsafe {
            heap.alloc_root_raw(
                id,
                Node {
                    next: None,
                    value: 99,
                },
            )
        }
        .unwrap();

        let mem_after_alloc = heap.memory_used();
        let par_mem_after_alloc = heap.partition(id).unwrap().memory_used();
        assert!(mem_after_alloc > mem_before);
        assert!(par_mem_after_alloc > par_mem_before);

        // GC should collect the 3 non-root nodes
        let freed = heap.garbage_collect(id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(freed > 0);

        let mem_after_gc = heap.memory_used();
        let par_mem_after_gc = heap.partition(id).unwrap().memory_used();

        // Only the root node should remain
        let root_size = mem_after_alloc - mem_before - freed;
        assert_eq!(mem_after_gc, mem_before + root_size);
        assert_eq!(par_mem_after_gc, par_mem_before + root_size);

        // Remove the partition to clean up remaining root node
        let link = heap.finalize_partition(id).unwrap();
        let freed_rem = heap.dealloc_partition(id, link);
        assert_eq!(freed_rem, root_size);
        assert_eq!(heap.memory_used(), mem_before);
    }

    #[test]
    fn test_memory_used_update_mem_use_edge_cases() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let id = heap.create_partition(0, 0);

        // Normal add
        assert_eq!(heap.update_mem_use(id, 50), 50);
        assert_eq!(heap.partition(id).unwrap().memory_used(), 50);
        assert_eq!(heap.memory_used(), 50);

        // Normal subtract
        assert_eq!(heap.update_mem_use(id, -30), 20);
        assert_eq!(heap.partition(id).unwrap().memory_used(), 20);
        assert_eq!(heap.memory_used(), 20);

        // Subtract to zero
        assert_eq!(heap.update_mem_use(id, -20), 0);
        assert_eq!(heap.partition(id).unwrap().memory_used(), 0);
        assert_eq!(heap.memory_used(), 0);
    }
}
