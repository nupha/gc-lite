// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::VecDeque, marker::PhantomData, ptr::NonNull};

use crate::{
    GcHeap, GcPartitionId, GcRef,
    gctype::TypeRegistry,
    node::{GcHead, GcNodeFlag},
    node_iterator::NodeLinkIter,
};

/// Garbage collection object tracing trait
///
/// Any type that wants to be managed by the garbage collection system must implement this trait.
/// This ensures that only types that explicitly support garbage collection can be allocated.
pub unsafe trait GcTracable: 'static {
    /// Collect directly referenced children gc nodes
    fn trace(&self, gcx: &mut GcTraceCtx);

    /// Set cross scope for `self` node and its direct children nodes
    fn set_xref(&self, heap: &mut GcHeap, scope: GcPartitionId) {
        let mut gcx = GcTraceCtx::new(heap, GcTraceRestrict::No, false);
        self.trace(&mut gcx);

        for ch in gcx.take_traced_nodes() {
            heap.set_xref(scope, ch);
        }
    }
}

impl GcHead {
    /// get trace func of node
    pub(crate) fn trace_fn(&self) -> fn(NonNull<GcHead>, &mut GcTraceCtx<'_>) {
        TypeRegistry::with_node_gc_type(NonNull::from_ref(self), |ty| ty.trace_fn)
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GcTraceRestrict {
    /// no restrict
    No,
    /// trace no restrict, collect restrict
    Collect(GcPartitionId),
    /// trace restrict, collect restrict
    TraceCollect(GcPartitionId),
}

pub struct GcTraceCtx<'a> {
    pub(super) heap: NonNull<GcHeap>,
    pub(super) restrict: GcTraceRestrict,
    pub(super) traced_nodes: VecDeque<NonNull<GcHead>>, // SmallVec<[NonNull<GcHead>; 16]>,
    _mark: PhantomData<&'a ()>,
}

impl<'a> GcTraceCtx<'a> {
    #[allow(non_snake_case)]
    pub fn MARK_FUNC(mut h: NonNull<GcHead>, _: &GcHeap) {
        unsafe {
            if !h.as_ref().is_marked() {
                h.as_mut().set_marked(true);
            }
        }
    }

    /// create new trace ctx, optionally clear all nodes' visit and mark flags.
    pub fn new(heap: &GcHeap, restrict: GcTraceRestrict, clear_flags: bool) -> Self {
        #[cfg(debug_assertions)]
        if let GcTraceRestrict::Collect(pid) | GcTraceRestrict::TraceCollect(pid) = restrict {
            debug_assert!(heap.partition(pid).is_some());
        }

        if clear_flags {
            heap.clear_node_flags(GcNodeFlag::MARKED | GcNodeFlag::TRACED);
        }

        let ctx = GcTraceCtx {
            heap: NonNull::from_ref(heap),
            restrict,
            traced_nodes: VecDeque::new(),
            _mark: PhantomData,
        };

        // if clear_flags {
        //     ctx.clear_flags(GcNodeFlag::MARKED | GcNodeFlag::TRACED);
        // }

        ctx
    }

    #[deprecated(note = "use GcHeap::clear_node_flags() instead")]
    fn clear_flags(&mut self, mask_off: GcNodeFlag) {
        if let GcTraceRestrict::TraceCollect(pid) = self.restrict {
            let start = self.heap().partition_nodes.get(&pid).unwrap();
            let nodes = NodeLinkIter::new(*start);
            for mut n in nodes {
                unsafe {
                    let mut f = n.as_ref().flags();
                    let f0 = f;
                    f.remove(mask_off);
                    if f0 != f {
                        n.as_mut().set_flags(f);
                    }
                }
            }
        } else {
            let nodes = self
                .heap()
                .partition_nodes
                .values()
                .flat_map(|&n| NodeLinkIter::new(n));

            for mut n in nodes {
                unsafe {
                    let mut f = n.as_ref().flags();
                    let f0 = f;
                    f.remove(mask_off);
                    if f0 != f {
                        n.as_mut().set_flags(f);
                    }
                }
            }
        }
    }

    #[inline(always)]
    pub const fn heap(&self) -> &GcHeap {
        unsafe { self.heap.as_ref() }
    }

    #[inline(always)]
    pub const fn heap_mut(&mut self) -> &mut GcHeap {
        unsafe { self.heap.as_mut() }
    }

    #[deprecated(note = "use GcHeap::clear_node_flags() instead")]
    #[inline(always)]
    pub fn clear_marked_flag(&mut self) {
        self.clear_flags(GcNodeFlag::MARKED);
    }

    #[deprecated(note = "use GcHeap::clear_node_flags() instead")]
    #[inline(always)]
    pub fn clear_traced_flag(&mut self) {
        self.clear_flags(GcNodeFlag::TRACED);
    }

    #[inline(always)]
    fn can_trace(&self, partition_id: GcPartitionId) -> bool {
        match self.restrict {
            GcTraceRestrict::TraceCollect(p) => p == partition_id,
            GcTraceRestrict::No | GcTraceRestrict::Collect(_) => true,
        }
    }

    fn can_collect(&self, partition_id: GcPartitionId) -> bool {
        match self.restrict {
            GcTraceRestrict::TraceCollect(p) | GcTraceRestrict::Collect(p) => p == partition_id,
            GcTraceRestrict::No => true,
        }
    }

    /// Apply callback on `node`, and optionally collect direct children nodes
    fn apply1(
        &mut self,
        node: NonNull<GcHead>,
        callback: impl Fn(NonNull<GcHead>, &GcHeap),
        ignore_trace_flag: bool,
    ) {
        unsafe {
            #[cfg(debug_assertions)]
            node.as_ref().debug_assert_node_valid(self.heap()); // O.o

            let pid = node.as_ref().scope_id();

            if ignore_trace_flag || !node.as_ref().flags().contains(GcNodeFlag::TRACED) {
                if self.can_collect(pid) {
                    callback(node, self.heap());
                }

                if !ignore_trace_flag {
                    (*node.as_ptr()).set_flags(node.as_ref().flags().union(GcNodeFlag::TRACED));
                }

                if self.can_trace(pid) {
                    // collect direct children nodes of `node`
                    (TypeRegistry::with_node_gc_type(node, |ty| ty.trace_fn))(node, self);
                }
            }
        }
    }

    /// Trace single `node` recursively for all descendant nodes,
    /// apply `callback` on each of them.
    pub fn trace(&mut self, node: NonNull<GcHead>, callback: impl Fn(NonNull<GcHead>, &GcHeap)) {
        // Note: to avoid cyclic dead loop, must check TRACED flag.
        self.apply1(node, &callback, false);

        while let Some(ch) = self.traced_nodes.pop_front() {
            self.apply1(ch, &callback, false);
        }
    }

    /// Trace multiple nodes recursively.
    pub fn trace_iter(
        &mut self,
        iter: impl Iterator<Item = NonNull<GcHead>>,
        callback: impl Fn(NonNull<GcHead>, &GcHeap),
    ) {
        for ptr in iter {
            self.trace(ptr, &callback);
        }
    }

    pub fn trace_roots(
        &mut self,
        partition_id: GcPartitionId,
        callback: impl Fn(NonNull<GcHead>, &GcHeap),
    ) {
        if let Some(roots) = unsafe { self.heap.as_ref().partition_root_nodes.get(&partition_id) } {
            for p in roots {
                self.trace(*p, &callback);
            }
        }
    }

    pub fn commit(&mut self, callback: impl Fn(NonNull<GcHead>, &GcHeap)) {
        while let Some(n) = self.traced_nodes.pop_front() {
            self.apply1(n, &callback, false);
        }
    }

    /// clear collected nodes
    pub fn clear(&mut self) {
        self.traced_nodes.clear();
    }

    /// take out collected nodes
    pub fn take_traced_nodes(&mut self) -> Vec<NonNull<GcHead>> {
        std::mem::take(&mut self.traced_nodes).into()
    }

    /// Submit a node to collected list
    #[inline]
    pub fn add_node(&mut self, node: NonNull<GcHead>) {
        unsafe {
            #[cfg(debug_assertions)]
            node.as_ref().debug_assert_node_valid(self.heap()); // O.o

            if self.can_collect(node.as_ref().scope_id())
                && !self.traced_nodes.iter().any(|&n| n == node)
            {
                self.traced_nodes.push_back(node);
            }
        }
    }

    /// Submit nodes to collected list
    pub fn add_nodes(&mut self, nodes: impl Iterator<Item = NonNull<GcHead>>) {
        for n in nodes {
            self.add_node(n);
        }
    }

    /// Submit a GcRef to collected list
    #[inline(always)]
    pub fn add<T: GcTracable>(&mut self, gc_ref: GcRef<T>) {
        self.add_node(gc_ref.head_ptr);
    }
}

impl GcHeap {
    fn clear_node_flags(&self, clear_mask: GcNodeFlag) {
        for pid in self.partition_ids() {
            unsafe {
                for mut n in self.nodes(pid) {
                    let f0 = n.as_ref().flags();
                    let mut f = f0;
                    f.remove(clear_mask);
                    if f != f0 {
                        n.as_mut().set_flags(f);
                    }
                }
            }
        }
    }

    fn traverse_internal(
        parent: Option<NonNull<GcHead>>,
        mut this: NonNull<GcHead>,
        ctx: &mut GcTraceCtx,
        filter: GcPartitionId,
        callback: &mut impl FnMut(NonNull<GcHead>, Option<NonNull<GcHead>>),
    ) {
        unsafe {
            if filter.is_null() || filter == this.as_ref().scope_id() {
                callback(this, parent);
            }

            let f = this.as_ref().flags();
            this.as_mut().set_flags(f.union(GcNodeFlag::TRACED));

            (this.as_ref().trace_fn())(this, ctx);

            let mut children = ctx.take_traced_nodes();
            while let Some(ch) = children.pop() {
                if !ch.as_ref().is_traced() {
                    Self::traverse_internal(Some(this), ch, ctx, filter, callback);
                }
            }
        }
    }

    /// Traverses the subtree starting at `node` in depth-first order,
    /// invoking `callback` on each visited node with its optional parent.
    /// If `filter` is non-null, only nodes in the specified partition are visited.
    pub fn traverse_subtree(
        &self,
        node: NonNull<GcHead>,
        filter: GcPartitionId,
        mut callback: impl FnMut(NonNull<GcHead>, Option<NonNull<GcHead>>),
    ) {
        // clear nodes's traced flag for all scopes
        self.clear_node_flags(GcNodeFlag::TRACED);
        let mut ctx = GcTraceCtx::new(self, GcTraceRestrict::No, false);
        Self::traverse_internal(None, node, &mut ctx, filter, &mut callback);
    }

    /// Collects all nodes in the subtree starting at `node`.
    /// If `filter` is non-null, only nodes in the specified partition are returned.
    pub fn collect_subtree_nodes(
        &self,
        node: NonNull<GcHead>,
        filter: GcPartitionId,
    ) -> Vec<NonNull<GcHead>> {
        let mut nodes = Vec::new();
        self.traverse_subtree(node, filter, |n, _p| {
            nodes.push(n);
        });
        nodes
    }

    /// Collects all edges (parent, child) pairs in the subtree starting at `node`.
    /// If `filter` is non-null, only edges within the specified partition are returned.
    pub fn collect_subtree_edges(
        &self,
        node: NonNull<GcHead>,
        filter: GcPartitionId,
    ) -> Vec<(NonNull<GcHead>, NonNull<GcHead>)> {
        let mut edges = Vec::new();
        self.traverse_subtree(node, filter, |n, p| {
            if let Some(parent) = p {
                edges.push((parent, n));
            }
        });
        edges
    }

    /// Collects both nodes and edges of the subtree starting at `node`.
    /// Returns `(nodes, edges)`; `edges` are (parent, child) pairs.
    /// If `filter` is non-null, only data in the specified partition are collected.
    pub fn collect_subtree(
        &self,
        node: NonNull<GcHead>,
        filter: GcPartitionId,
    ) -> (
        Vec<NonNull<GcHead>>,
        Vec<(NonNull<GcHead>, NonNull<GcHead>)>,
    ) {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        self.traverse_subtree(node, filter, |n, p| {
            nodes.push(n);
            if let Some(parent) = p {
                edges.push((parent, n));
            }
        });
        (nodes, edges)
    }
}

macro_rules! impl_dummy_trace_for_primitive {
    ($($ty:ty),*) => {
        $(
            unsafe impl GcTracable for $ty {
                #[inline(always)]
                fn trace(&self, _: &mut GcTraceCtx) { }
            }
            unsafe impl GcTracable for [$ty] {
                #[inline(always)]
                fn trace(&self, _: &mut GcTraceCtx) { }
            }
            unsafe impl GcTracable for Vec<$ty> {
                #[inline(always)]
                fn trace(&self, _: &mut GcTraceCtx) { }
            }
            unsafe impl GcTracable for Box<[$ty]> {
                #[inline(always)]
                fn trace(&self, _: &mut GcTraceCtx) { }
            }
        )*
    };
}

// Implement GcTracable for basic types
impl_dummy_trace_for_primitive!(
    u8, u16, u32, u64, u128, i8, i16, i32, i64, i128, f32, f64, usize, isize, bool, char
);

unsafe impl GcTracable for str {
    #[inline(always)]
    fn trace(&self, _: &mut GcTraceCtx) {}
}
unsafe impl GcTracable for &'static str {
    #[inline(always)]
    fn trace(&self, _: &mut GcTraceCtx) {}
}
unsafe impl GcTracable for String {
    #[inline(always)]
    fn trace(&self, _: &mut GcTraceCtx) {}
}
unsafe impl GcTracable for &'static String {
    #[inline(always)]
    fn trace(&self, _: &mut GcTraceCtx) {}
}

#[cfg(test)]
mod tests {
    use std::ops::DerefMut;

    use super::*;
    use crate::{GcHeap, GcRef};

    /// Test node structure for tracing tests
    #[derive(Debug)]
    struct TestNode {
        id: u32,
        children: Vec<GcRef<TestNode>>,
    }

    impl TestNode {
        fn new(id: u32) -> Self {
            Self {
                id,
                children: Vec::new(),
            }
        }

        fn add_child(&mut self, child: GcRef<TestNode>) {
            self.children.push(child);
        }
    }

    unsafe impl GcTracable for TestNode {
        fn trace(&self, tr: &mut GcTraceCtx) {
            println!(
                "TestNode::trace({self:p}), {} children",
                self.children.len()
            );

            for (i, child) in self.children.iter().enumerate() {
                println!("  Tracing child {}: {:?}", i, child.node_ptr());
                tr.add(*child);
            }
        }
    }

    /// Helper function to count marked nodes in a partition
    fn count_marked_nodes(heap: &GcHeap, partition_id: GcPartitionId) -> usize {
        let mut count = 0;
        if let Some(head) = heap.partition_nodes.get(&partition_id).copied().flatten() {
            let mut current = Some(head);
            while let Some(node) = current {
                unsafe {
                    let marked = node.as_ref().is_marked();
                    if marked {
                        count += 1;
                    }
                    current = node.as_ref().next;
                }
            }
        }
        count
    }

    /// Helper function to get all node IDs in a partition
    fn get_all_node_ids(heap: &GcHeap, partition_id: GcPartitionId) -> Vec<u32> {
        let mut ids = Vec::new();
        if let Some(head) = heap.partition_nodes.get(&partition_id).copied().flatten() {
            let mut current = Some(head);
            while let Some(node) = current {
                unsafe {
                    // Calculate pointer to TestNode payload
                    let payload_ptr = node.as_ref().payload();
                    // ID field is at offset 24 bytes within TestNode (due to field reordering)
                    let id_addr = payload_ptr.add(24);
                    let id = *(id_addr.as_ptr() as *const u32);
                    ids.push(id);
                    current = node.as_ref().next;
                }
            }
        }
        ids
    }

    /// Test 1: Simple tree structure with Propagate (depth-first)
    #[test]
    fn test_trace_propagate_simple_tree() {
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(4096);

        // Create a simple tree: root -> child1, child2
        let child1 = heap.alloc(partition_id, TestNode::new(1)).unwrap();
        let child2 = heap.alloc(partition_id, TestNode::new(2)).unwrap();

        let mut root = TestNode::new(0);
        root.add_child(child1);
        root.add_child(child2);
        let root_ref = heap.alloc(partition_id, root).unwrap();

        // Debug: print node pointers
        println!("Root: {:?}", root_ref.node_ptr());
        println!("Child1: {:?}", child1.node_ptr());
        println!("Child2: {:?}", child2.node_ptr());

        // Create trace ctx and trace (using MARK_FUNC)
        let mut ctx = GcTraceCtx::new(&heap, GcTraceRestrict::Collect(partition_id), true);
        ctx.trace(root_ref.node_ptr(), GcTraceCtx::MARK_FUNC);

        // check marks after tracing
        println!(
            "Marks after tracing: {}",
            count_marked_nodes(&heap, partition_id)
        );

        // Verify all nodes are marked
        assert_eq!(count_marked_nodes(&heap, partition_id), 3);

        // Verify all node IDs are present
        let ids = get_all_node_ids(&heap, partition_id);
        assert!(ids.contains(&0));
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
    }

    /// Test 2: Simple tree structure with Continue (breadth-first)
    #[test]
    fn test_trace_continue_simple_tree() {
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(4096);

        // Create a simple tree: root -> child1, child2
        let child1 = heap.alloc(partition_id, TestNode::new(1)).unwrap();
        let child2 = heap.alloc(partition_id, TestNode::new(2)).unwrap();

        let mut root = TestNode::new(0);
        root.add_child(child1);
        root.add_child(child2);
        let root_ref = heap.alloc(partition_id, root).unwrap();

        // Create tracer and trace with Continue
        let mut ctx = GcTraceCtx::new(&heap, GcTraceRestrict::Collect(partition_id), true);
        ctx.trace(root_ref.node_ptr(), GcTraceCtx::MARK_FUNC);

        // Verify all nodes are marked
        assert_eq!(count_marked_nodes(&heap, partition_id), 3);

        // Verify pendings is empty after processing
        assert!(ctx.traced_nodes.is_empty());
    }

    /// Test 3: Deep nested tree with both algorithms
    #[test]
    fn test_trace_deep_nested_tree() {
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(8192);

        // Create a deep tree: level0 -> level1 -> level2 -> level3
        let level3 = heap.alloc(partition_id, TestNode::new(3)).unwrap();

        let mut level2 = TestNode::new(2);
        level2.add_child(level3);
        let level2_ref = heap.alloc(partition_id, level2).unwrap();

        let mut level1 = TestNode::new(1);
        level1.add_child(level2_ref);
        let level1_ref = heap.alloc(partition_id, level1).unwrap();

        let mut level0 = TestNode::new(0);
        level0.add_child(level1_ref);
        let level0_ref = heap.alloc(partition_id, level0).unwrap();

        // Test with Propagate
        let mut ctx1 = GcTraceCtx::new(&heap, GcTraceRestrict::Collect(partition_id), true);
        ctx1.trace(level0_ref.node_ptr(), GcTraceCtx::MARK_FUNC);
        assert_eq!(count_marked_nodes(&heap, partition_id), 4);

        // Test with Continue
        let mut ctx2 = GcTraceCtx::new(&heap, GcTraceRestrict::Collect(partition_id), true);
        ctx2.trace(level0_ref.node_ptr(), GcTraceCtx::MARK_FUNC);
        assert_eq!(count_marked_nodes(&heap, partition_id), 4);
    }

    /// Test 4: Complex tree with multiple branches
    #[test]
    fn test_trace_complex_tree() {
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(16384);

        // Create a complex tree:
        //        root
        //       /    \
        //      a      b
        //     / \    / \
        //    c   d  e   f

        let c = heap.alloc(partition_id, TestNode::new(3)).unwrap();
        let d = heap.alloc(partition_id, TestNode::new(4)).unwrap();
        let e = heap.alloc(partition_id, TestNode::new(5)).unwrap();
        let f = heap.alloc(partition_id, TestNode::new(6)).unwrap();

        let mut a = TestNode::new(1);
        a.add_child(c);
        a.add_child(d);
        let a_ref = heap.alloc(partition_id, a).unwrap();

        let mut b = TestNode::new(2);
        b.add_child(e);
        b.add_child(f);
        let b_ref = heap.alloc(partition_id, b).unwrap();

        let mut root = TestNode::new(0);
        root.add_child(a_ref);
        root.add_child(b_ref);
        let root_ref = heap.alloc(partition_id, root).unwrap();

        // Test with Propagate
        let mut ctx1 = GcTraceCtx::new(&heap, GcTraceRestrict::Collect(partition_id), true);
        ctx1.trace(root_ref.node_ptr(), GcTraceCtx::MARK_FUNC);
        assert_eq!(count_marked_nodes(&heap, partition_id), 7);

        // Test with Continue
        let mut ctx2 = GcTraceCtx::new(&heap, GcTraceRestrict::Collect(partition_id), true);
        ctx2.trace(root_ref.node_ptr(), GcTraceCtx::MARK_FUNC);
        assert_eq!(count_marked_nodes(&heap, partition_id), 7);
    }

    /// Test 5: Verify both algorithms produce same result
    #[test]
    fn test_trace_algorithms_equivalence() {
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(8192);

        // Create a tree with 10 nodes in a balanced structure
        let mut nodes = Vec::new();
        for i in 0..10 {
            nodes.push(heap.alloc(partition_id, TestNode::new(i as u32)).unwrap());
        }

        // Build tree: 0 -> 1,2; 1 -> 3,4; 2 -> 5,6; 3 -> 7,8; 4 -> 9
        {
            let n = nodes[1];
            nodes[0].add_child(n);

            let n = nodes[2];
            nodes[0].deref_mut().add_child(n);

            let n = nodes[3];
            nodes[1].deref_mut().add_child(n);

            let n = nodes[4];
            nodes[1].deref_mut().add_child(n);

            let n = nodes[5];
            nodes[2].deref_mut().add_child(n);

            let n = nodes[6];
            nodes[2].deref_mut().add_child(n);

            let n = nodes[7];
            nodes[3].deref_mut().add_child(n);

            let n = nodes[8];
            nodes[3].deref_mut().add_child(n);

            let n = nodes[9];
            nodes[4].deref_mut().add_child(n);
        }

        // Test with Propagate
        let mut ctx1 = GcTraceCtx::new(&heap, GcTraceRestrict::Collect(partition_id), true);
        ctx1.trace(nodes[0].node_ptr(), GcTraceCtx::MARK_FUNC);
        let propagate_marked = count_marked_nodes(&heap, partition_id);

        // Test with Continue
        let mut ctx2 = GcTraceCtx::new(&heap, GcTraceRestrict::Collect(partition_id), true);
        ctx2.trace(nodes[0].node_ptr(), GcTraceCtx::MARK_FUNC);
        let continue_marked = count_marked_nodes(&heap, partition_id);

        // Both algorithms should mark the same number of nodes
        assert_eq!(propagate_marked, continue_marked);
        assert_eq!(propagate_marked, 10);
    }

    /// Test 6: Circular reference handling
    #[test]
    fn test_trace_circular_reference() {
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(4096);

        // Create two nodes that reference each other
        let mut node1 = heap.alloc(partition_id, TestNode::new(1)).unwrap();
        let mut node2 = heap.alloc(partition_id, TestNode::new(2)).unwrap();

        {
            node1.add_child(node2);
            node2.add_child(node1);
        }

        // Test with Propagate - should handle circular reference without infinite loop
        let mut ctx1 = GcTraceCtx::new(&heap, GcTraceRestrict::Collect(partition_id), true);
        ctx1.trace(node1.node_ptr(), GcTraceCtx::MARK_FUNC);

        // Both nodes should be marked
        assert_eq!(count_marked_nodes(&heap, partition_id), 2);

        // Test with Continue
        let mut ctx2 = GcTraceCtx::new(&heap, GcTraceRestrict::Collect(partition_id), true);
        ctx2.trace(node1.node_ptr(), GcTraceCtx::MARK_FUNC);
        assert_eq!(count_marked_nodes(&heap, partition_id), 2);
    }
}
