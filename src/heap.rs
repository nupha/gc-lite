// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::HashMap, ptr::NonNull};

use crate::{
    gctype::GcTypeRegistry,
    node::{GcHead, GcNodeFlag},
    partition::{GcPartition, GcPartitionId},
    scope::{GcScopeStackId, ScopeStack},
};

pub struct GcHeap {
    /// Registered GC data type info
    pub(super) node_dtypes: &'static GcTypeRegistry,

    /// Partition management
    pub(super) partitions: HashMap<GcPartitionId, GcPartition>,
    /// Global memory usage limit for the entire heap, 0 for unlimited
    pub(super) memory_limit: usize,
    /// Global automatic GC threshold for the entire heap, 0 for disabled
    pub(super) gc_threshold: usize,
    /// Total memory used across all partitions
    pub(super) total_memory_used: usize,
    /// Weak reference list, each slot stores (version, GcHeader)
    pub(super) weak_slots: Vec<(u16, Option<NonNull<GcHead>>)>,
    /// gc scope stack list
    pub(crate) scope_stacks: Vec<ScopeStack>, // DON'T use SmallVec here

    /// User provided opaque raw pointer
    opaque: *mut u8,

    #[cfg(debug_assertions)]
    pub(crate) dbg_dropping_root_partition: Option<GcPartitionId>,
    #[cfg(debug_assertions)]
    pub(crate) dbg_living_nodes: std::collections::HashSet<NonNull<GcHead>>,
}

impl Drop for GcHeap {
    fn drop(&mut self) {
        // heap world is gone, dealloc all nodes live in it, regardless their status.
        log::trace!("[heap::drop]");

        for stack in &mut self.scope_stacks {
            for s in stack.list.drain(..) {
                s.clear();
            }
        }

        let pars = std::mem::take(&mut self.partitions);
        for (_, partition) in pars {
            self.dispose_all_nodes(partition.nodes, Self::DUMMY_DISPOSE_CALLBACK);
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

    /// Create a new garbage collection heap with an explicit GC type registry
    pub fn new(registry: &'static GcTypeRegistry) -> Self {
        Self {
            partitions: HashMap::new(),
            memory_limit: 0,
            gc_threshold: 0,
            total_memory_used: 0,
            weak_slots: Vec::new(),
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

    pub const fn set_opaque(&mut self, opaque: *mut u8) {
        self.opaque = opaque;
    }

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
                let adjusted = applied - (applied >> 2);
                self.gc_threshold = adjusted;
            }
        }

        self.memory_limit
    }

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
    /// # Note
    ///
    /// This method **DO NOT** increase partitions' mem_use.
    pub(crate) fn attach_node(&mut self, partition_id: GcPartitionId, mut node: NonNull<GcHead>) {
        debug_assert!(!partition_id.is_null());

        let n = unsafe { node.as_mut() };
        debug_assert!(n.partition_id().is_null());
        debug_assert!(n.next.is_none());
        n.set_partition_id(partition_id);

        let par = self.partitions.get_mut(&partition_id).unwrap();
        par.nodes.prepend(node);
    }

    pub fn set_root_node(&mut self, mut node: NonNull<GcHead>) {
        let n = unsafe { node.as_mut() };
        if !n.is_root() {
            let partition_id = n.partition_id();
            if let Some(p) = self.partitions.get_mut(&partition_id) {
                n.insert_flag(GcNodeFlag::ROOT);
                if p.is_marking() {
                    p.add_gray_node(node);
                }
            }
        }
    }

    /// Check if `node` was allocated in this heap
    pub fn contains(&self, node: NonNull<GcHead>) -> bool {
        self.nodes(unsafe { node.as_ref().partition_id() })
            .any(|p| p == node)
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
        if id.is_null() {
            return 0;
        }

        if let Some(par) = self.partitions.get_mut(&id) {
            if delta >= 0 {
                let d = delta as usize;
                par.memory_used += d;
                self.total_memory_used += d;
            } else {
                let d = (-delta) as usize;
                debug_assert!(par.memory_used >= d);
                debug_assert!(self.total_memory_used >= d);
                par.memory_used -= d;
                self.total_memory_used -= d;
            }
            par.memory_used
        } else {
            0
        }
    }

    #[inline(always)]
    pub const fn memory_used(&self) -> usize {
        self.total_memory_used
    }
}

#[cfg(test)]
mod heap_tests {
    use crate::{GcRef, GcTraceCtx, trace::GcTrace};

    use super::*;

    #[derive(Debug)]
    struct Node {
        next: Option<GcRef<Node>>,
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
        let partition_id = heap.create_partition();
        let stack_id = heap.acquire_scope_stack(partition_id);

        let head = heap.with_new_scope(stack_id, |ctx| {
            let node: GcRef<Node> = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();
            ctx.clear();
            node.head_ptr
        });

        while !heap.mark(partition_id, 64) {}
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after > 0);
    }
}
