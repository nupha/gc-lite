// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::HashMap, marker::PhantomData, ptr::NonNull};

use smallvec::SmallVec;

use crate::{
    GcNode, GcRef,
    gctype::GcTypeRegistry,
    node::{GcHead, GcTriColor},
    partition::{GcPartition, GcPartitionId},
};

pub struct GcHeap {
    /// Registered GC data type info
    pub(super) node_dtypes: &'static GcTypeRegistry,

    /// Partition management
    pub(super) partitions: HashMap<GcPartitionId, GcPartition>,
    /// stacked node guards
    pub(crate) guard_stack: SmallVec<[GcNodeGuard<'static>; 8]>,
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

        for mut g in self.guard_stack.drain(..) {
            unsafe {
                g.abort();
            }
        }

        let mut pars = std::mem::take(&mut self.partitions);
        for (_, mut partition) in pars.drain() {
            if let Some(link) = partition.nodes.take() {
                self.dispose_all_nodes(link, Self::DUMMY_DISPOSE_CALLBACK);
            }
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
            guard_stack: SmallVec::new(),

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

        if let Some(mut par) = self.partitions.remove(&partition_id)
            && let Some(link) = par.nodes.take()
        {
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
        debug_assert!(n.scope_id().is_null());
        debug_assert!(n.next.is_none());
        n.set_scope_id(partition_id);

        let par = self.partitions.get_mut(&partition_id).unwrap();
        n.next = par.nodes.take();
        par.nodes = Some(node);
    }

    /// Set/unset a node to be root
    pub fn set_root_node(&mut self, mut node_ptr: NonNull<GcHead>, is_root: bool) {
        let node = unsafe { node_ptr.as_mut() };

        if node.is_root() != is_root {
            node.set_root(is_root);
            let par = self.partition_mut(node.scope_id()).unwrap();

            if is_root {
                // Add to partition's root object list
                if !par.root_nodes.contains(&node_ptr) {
                    par.root_nodes.push(node_ptr);
                    if par.is_marking() && node.color() != GcTriColor::Black {
                        par.add_gray_node(node_ptr);
                    }
                } else {
                    #[cfg(debug_assertions)]
                    debug_assert!(node.is_protected());
                }
            } else if !node.is_protected() {
                // Remove from partition's root nodes list
                let i = par.root_nodes.iter().position(|&n| n == node_ptr).unwrap();
                par.root_nodes.swap_remove(i);
            }
        }
    }

    /// Set/unset a gc_ref to be root
    #[inline(always)]
    pub fn set_root<T: GcNode>(&mut self, gc_ref: GcRef<T>, is_root: bool) {
        self.set_root_node(gc_ref.head_ptr, is_root);
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
        self.nodes(unsafe { node.as_ref().scope_id() })
            .any(|p| p == node)
    }

    fn protect_node1(&mut self, mut n: NonNull<GcHead>) {
        let node = unsafe { n.as_mut() };

        if node.inc_protect_count() == 1 && !node.is_root() {
            let par = self.partition_mut(node.scope_id()).unwrap();
            par.root_nodes.push(n);
            if par.is_marking() && node.color() == GcTriColor::White {
                par.add_gray_node(n);
            }
        }
    }

    #[must_use]
    pub fn protect_nodes(&self, nodes: &[NonNull<GcHead>]) -> GcNodeGuard<'_> {
        let mut lst = SmallVec::<[NonNull<GcHead>; 8]>::new();
        let heap_ptr = self as *const Self as *mut Self;

        for &n in nodes {
            unsafe {
                (*heap_ptr).protect_node1(n);
            }
            lst.push(n);
        }

        GcNodeGuard {
            nodes: lst,
            heap: heap_ptr,
            _mark: PhantomData,
        }
    }

    #[must_use]
    pub fn protect_node(&self, node: NonNull<GcHead>) -> GcNodeGuard<'_> {
        self.protect_nodes(&[node])
    }

    /// Check if the given partition ID is an ancestor of the specified partition
    pub fn is_ancestor_of(&self, this: GcPartitionId, ancestor: GcPartitionId) -> bool {
        debug_assert_ne!(this, GcPartitionId::NONE);
        debug_assert_ne!(ancestor, GcPartitionId::NONE);
        this == ancestor
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

    pub fn open_guard(&mut self) {
        let guard = GcNodeGuard {
            nodes: SmallVec::new(),
            heap: self as *mut GcHeap,
            _mark: PhantomData,
        };
        self.guard_stack.push(guard);
    }

    pub fn close_guard(&mut self) {
        let trans = self
            .guard_stack
            .pop()
            .expect("GcAllocTrans stack underflow");
        drop(trans);
    }

    /// get current node guard
    #[inline(always)]
    pub(crate) fn current_guard(&mut self) -> Option<&mut GcNodeGuard<'static>> {
        self.guard_stack.last_mut()
    }

    /// with current node guard
    pub fn with_current_guard<R, F: FnOnce(&mut GcNodeGuard) -> R>(&mut self, f: F) -> Option<R> {
        self.guard_stack.last_mut().map(f)
    }
}

pub struct GcNodeGuard<'a> {
    nodes: SmallVec<[NonNull<GcHead>; 8]>,
    heap: *mut GcHeap,
    _mark: PhantomData<&'a ()>,
}

impl<'a> Drop for GcNodeGuard<'a> {
    fn drop(&mut self) {
        for mut node in self.nodes.drain(..) {
            let n = unsafe { node.as_mut() };
            let count = n.dec_protect_count();

            if count == 0 && !n.is_root() {
                let heap = unsafe { &mut *self.heap };
                if let Some(par) = heap.partition_mut(n.scope_id())
                    && let Some(i) = par.root_nodes.iter().position(|&x| x == node)
                {
                    par.root_nodes.swap_remove(i);
                }
            }
        }
    }
}

impl<'a> GcNodeGuard<'a> {
    /// add an extra node to guard
    pub fn add(&mut self, node: NonNull<GcHead>) {
        if !self.nodes.contains(&node) {
            unsafe {
                (*self.heap).protect_node1(node);
            }
            self.nodes.push(node);
        }
    }

    /// # Safety
    ///
    /// 仅供 `GcHeap::drop` 在销毁事务栈时调用，用于跳过对
    /// `nodes` 中节点的保护计数更新与根集合维护逻辑。
    /// 调用方必须保证这些节点即将被整体释放，不再通过 GC 访问。
    unsafe fn abort(&mut self) {
        self.nodes.clear();
    }
}

#[cfg(test)]
mod heap_tests {
    use crate::{GcTraceCtx, trace::GcTrace};

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
    fn test_protect_count_and_guard_lifecycle() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let node: GcRef<Node> = heap
            .alloc(
                partition_id,
                Node {
                    next: None,
                    value: 1,
                },
            )
            .unwrap();

        let head = node.head_ptr;

        unsafe {
            assert_eq!(head.as_ref().protect_count(), 0);
        }

        {
            let _g1 = heap.protect_nodes(&[head]);
            unsafe {
                assert_eq!(head.as_ref().protect_count(), 1);
            }
        }

        unsafe {
            assert_eq!(head.as_ref().protect_count(), 0);
        }
    }

    #[test]
    fn test_alloc_trans_protects_nodes_during_transaction() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        heap.open_guard();

        let node: GcRef<Node> = heap
            .alloc(
                partition_id,
                Node {
                    next: None,
                    value: 1,
                },
            )
            .unwrap();

        let head = node.head_ptr;

        unsafe {
            assert_eq!(head.as_ref().protect_count(), 1);
        }

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(removed, 0);

        heap.close_guard();

        while !heap.mark(partition_id, 64) {}

        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after > 0);
    }

    #[test]
    fn test_alloc_trans_nested_transactions() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        heap.open_guard();
        let node1: GcRef<Node> = heap
            .alloc(
                partition_id,
                Node {
                    next: None,
                    value: 1,
                },
            )
            .unwrap();
        let head1 = node1.head_ptr;

        heap.open_guard();
        let node2: GcRef<Node> = heap
            .alloc(
                partition_id,
                Node {
                    next: None,
                    value: 2,
                },
            )
            .unwrap();
        let head2 = node2.head_ptr;

        unsafe {
            assert_eq!(head1.as_ref().protect_count(), 1);
            assert_eq!(head2.as_ref().protect_count(), 1);
        }

        while !heap.mark(partition_id, 64) {}

        let disposed_head1 = std::cell::Cell::new(false);
        let disposed_head2 = std::cell::Cell::new(false);
        let head1_ptr = head1;
        let head2_ptr = head2;
        let removed_inner = heap.sweep(partition_id, |_, h| {
            let ptr = h as *const GcHead;
            if ptr == head1_ptr.as_ptr() {
                disposed_head1.set(true);
            }
            if ptr == head2_ptr.as_ptr() {
                disposed_head2.set(true);
            }
        });
        assert_eq!(removed_inner, 0);
        assert!(!disposed_head1.get());
        assert!(!disposed_head2.get());

        heap.close_guard();

        while !heap.mark(partition_id, 64) {}

        let disposed_head1_after = std::cell::Cell::new(false);
        let disposed_head2_after = std::cell::Cell::new(false);
        let removed_after_inner = heap.sweep(partition_id, |_, h| {
            let ptr = h as *const GcHead;
            if ptr == head1_ptr.as_ptr() {
                disposed_head1_after.set(true);
            }
            if ptr == head2_ptr.as_ptr() {
                disposed_head2_after.set(true);
            }
        });
        assert!(removed_after_inner > 0);
        assert!(!disposed_head1_after.get());
        assert!(disposed_head2_after.get());

        heap.close_guard();

        while !heap.mark(partition_id, 64) {}

        let disposed_head1_final = std::cell::Cell::new(false);
        let removed_final = heap.sweep(partition_id, |_, h| {
            let ptr = h as *const GcHead;
            if ptr == head1_ptr.as_ptr() {
                disposed_head1_final.set(true);
            }
        });
        assert!(removed_final > 0);
        assert!(disposed_head1_final.get());
    }

    #[test]
    #[should_panic(expected = "GcHead protect count overflow")]
    fn test_protect_count_overflow_panics() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let node: GcRef<Node> = heap
            .alloc(
                partition_id,
                Node {
                    next: None,
                    value: 1,
                },
            )
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
}
