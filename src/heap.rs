// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::HashMap, ptr::NonNull};

use crate::{
    GcContext,
    gctype::GcTypeRegistry,
    node::{GcHead, GcNodeFlag, GcTriColor},
    partition::{GcPartition, GcPartitionId},
};

pub struct GcHeap {
    /// Registered GC data type info
    pub(super) node_dtypes: &'static GcTypeRegistry,

    /// Partition management
    pub(super) partitions: HashMap<GcPartitionId, GcPartition>,
    pub(crate) scope_stack: Vec<GcContext<'static>>,
    /// Weak reference list, each slot stores (version, GcHeader)
    pub(super) weak_slots: Vec<(u16, Option<NonNull<GcHead>>)>,

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

        for s in self.scope_stack.drain(..) {
            unsafe {
                s.abort();
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
            weak_slots: Vec::new(),
            opaque: std::ptr::null_mut(),
            node_dtypes: registry,
            scope_stack: Vec::new(),

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

    /// Get garbage collection threshold for partition (bytes)
    ///
    /// A return value of 0 means automatic GC is disabled
    pub fn gc_threshold(&self, partition_id: GcPartitionId) -> Option<usize> {
        self.partition(partition_id)
            .map(|partition| partition.gc_threshold())
    }

    /// Set garbage collection threshold for partition (in bytes)
    ///
    /// # Parameters
    /// - `partition_id`: Partition ID
    /// - `threshold`: New garbage collection threshold (bytes)
    ///   - A value of 0 disables automatic GC
    ///   - If threshold exceeds partition memory limit, it's automatically set to the memory limit
    ///
    /// # Notes
    /// - If the partition doesn't exist, this method does nothing
    pub fn set_gc_threshold(&mut self, partition_id: GcPartitionId, threshold: usize) {
        if let Some(partition) = self.partition_mut(partition_id) {
            partition.set_gc_threshold(if threshold > 0 {
                let lim = partition.memory_limit();
                if lim > 0 && threshold > lim {
                    lim
                } else {
                    threshold
                }
            } else {
                threshold
            });
        }
    }

    pub fn drop_partition(
        &mut self,
        partition_id: GcPartitionId,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        if partition_id.is_null() {
            return 0;
        }

        #[cfg(debug_assertions)]
        {
            self.dbg_dropping_root_partition = Some(partition_id);
        }

        let mut freed_bytes = 0;

        if let Some(mut par) = self.partitions.remove(&partition_id) {
            let link = std::mem::take(&mut par.nodes);
            freed_bytes += self.dispose_all_nodes(link, &on_dispose);
        }

        #[cfg(debug_assertions)]
        {
            self.dbg_dropping_root_partition = None;
        }

        freed_bytes
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
                p.root_nodes.push(node);
                if p.is_marking() {
                    p.add_gray_node(node);
                }
            }
        }
    }

    pub fn get_roots(
        &self,
        partition_id: GcPartitionId,
    ) -> impl Iterator<Item = NonNull<GcHead>> + '_ {
        self.partitions
            .get(&partition_id)
            .map(|p| p.root_nodes.iter().copied())
            .into_iter()
            .flatten()
    }

    /// Check if `node` was allocated in this heap
    pub fn contains(&self, node: NonNull<GcHead>) -> bool {
        self.nodes(unsafe { node.as_ref().partition_id() })
            .any(|p| p == node)
    }

    pub(crate) fn do_protect_node(&mut self, mut n: NonNull<GcHead>) {
        let node = unsafe { n.as_mut() };

        #[cfg(debug_assertions)]
        node.debug_assert_node_valid_simple();

        let count = node.inc_protect_count();

        if count == 1 && !node.is_root() {
            let par = self.partition_mut(node.partition_id()).unwrap();
            par.root_nodes.push(n);
            if par.is_marking() && node.color() == GcTriColor::White {
                par.add_gray_node(n);
            }
        }
    }

    pub(crate) fn do_unprotect_node(&mut self, mut n: NonNull<GcHead>) {
        let node = unsafe { n.as_mut() };

        #[cfg(debug_assertions)]
        node.debug_assert_node_valid_simple();

        let count = node.dec_protect_count();
        if count == 0
            && !node.is_root()
            && let Some(par) = self.partition_mut(node.partition_id())
            && let Some(i) = par.root_nodes.iter().position(|&x| x == n)
        {
            par.root_nodes.swap_remove(i);
            node.set_color(GcTriColor::White);
        }
    }

    /// Protect node from being gc collected.
    ///
    /// 1. if node is local or root, it's protected, returns true
    /// 1. otherwise if has current scope, add node to current scope and returns true
    /// 1. can't protect, returns false
    pub fn protect_node(&mut self, node: NonNull<GcHead>) -> bool {
        self.current_scope().is_some_and(|s| s.add_non_local(node))
    }

    /// Protect nodes from being gc collected, for each node do following steps:
    ///
    /// 1. if node is local or root, do nothing
    /// 1. if has current scope, add node to current scope
    /// 1. can't protect, returns false
    pub fn protect_nodes_iter(&mut self, nodes: impl Iterator<Item = NonNull<GcHead>>) {
        if let Some(s) = self.current_scope() {
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
    pub fn protect_nodes(&mut self, nodes: &[NonNull<GcHead>]) {
        self.protect_nodes_iter(nodes.iter().copied());
    }

    /// Update memory usage with rollup to parent partitions
    pub(crate) fn update_mem_use(&mut self, id: GcPartitionId, delta: i32) -> usize {
        if id.is_null() {
            return 0;
        }

        if let Some(par) = self.partitions.get_mut(&id) {
            if delta >= 0 {
                par.memory_used += delta as usize;
            } else {
                debug_assert!(par.memory_used >= (-delta) as usize);
                par.memory_used -= (-delta) as usize;
            }
            par.memory_used
        } else {
            0
        }
    }
}

#[cfg(test)]
mod heap_tests {
    use crate::{GcLocal, GcRef, GcTraceCtx, trace::GcTrace};

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
    #[should_panic(expected = "GcHead protect count overflow")]
    fn test_protect_count_overflow_panics() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let node: GcRef<Node> = unsafe {
            heap.alloc_raw(
                partition_id,
                Node {
                    next: None,
                    value: 1,
                },
            )
        }
        .unwrap();

        let mut head = node.head_ptr;

        unsafe {
            let h = head.as_mut();
            for _ in 0..7 {
                h.inc_protect_count();
            }
            h.inc_protect_count();
        }
    }

    #[test]
    fn test_gc_local_keeps_node_alive_during_scope() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let node: GcRef<Node> = unsafe {
            heap.alloc_raw(
                partition_id,
                Node {
                    next: None,
                    value: 1,
                },
            )
        }
        .unwrap();

        let head = node.head_ptr;

        {
            unsafe {
                assert_eq!(head.as_ref().protect_count(), 0);
            }

            {
                let _local = GcLocal::new(&mut heap, node);
                unsafe {
                    assert_eq!(head.as_ref().protect_count(), 1);
                }

                let par = heap.partitions.get(&partition_id).unwrap();
                assert!(par.root_nodes.contains(&head));
            }

            unsafe {
                assert_eq!(head.as_ref().protect_count(), 0);
            }

            let par = heap.partitions.get(&partition_id).unwrap();
            assert!(!par.root_nodes.contains(&head));
        }
    }

    #[test]
    fn test_alloc_local_behaves_like_alloc_plus_gc_local() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let local: GcLocal<Node> = unsafe {
            heap.alloc_local_raw(
                partition_id,
                Node {
                    next: None,
                    value: 1,
                },
            )
            .unwrap()
        };

        let head = local.get().head_ptr;

        unsafe {
            assert_eq!(head.as_ref().protect_count(), 1);
        }

        drop(local);

        unsafe {
            assert_eq!(head.as_ref().protect_count(), 0);
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after > 0);
    }

    #[test]
    fn test_heap_with_context_alloc_and_cleanup() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let head = heap.with_new_scope(partition_id, |ctx| {
            let node: GcRef<Node> = ctx
                .alloc(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();
            ctx.flush();
            node.head_ptr
        });

        unsafe {
            assert_eq!(head.as_ref().protect_count(), 0);
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after > 0);
    }
}
