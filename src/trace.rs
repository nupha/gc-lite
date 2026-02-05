// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::VecDeque, marker::PhantomData, ptr::NonNull};

use crate::{
    GcHeap, GcPartitionId, GcRef,
    node::{GcHead, GcHeadFlag},
};

/// Garbage collection object tracing trait
///
/// Any type that wants to be managed by the garbage collection system must implement this trait.
/// This ensures that only types that explicitly support garbage collection can be allocated.
pub unsafe trait GcTracable: 'static {
    /// Trace all GC references within the object
    ///
    /// This method is called during the marking phase to find all other GC objects referenced within the object.
    /// Implementers should call the `trace` closure to mark all referenced objects.
    fn trace(&self, tr: GcTraceOp);
}

/// Tracer, used to trace object references during marking phase
pub struct GcTracer<'a> {
    pub(super) heap: NonNull<GcHeap>,
    pub(super) partition_id: GcPartitionId,

    /// collected nodes during tracing
    pub(super) traced_nodes: VecDeque<NonNull<GcHead>>,

    _mark: PhantomData<&'a ()>,
}

impl<'a> GcTracer<'a> {
    #[allow(non_snake_case)]
    pub fn MARK_FUNC(mut h: NonNull<GcHead>, _: &GcHeap) -> bool {
        unsafe {
            if !h.as_ref().is_marked() {
                h.as_mut().set_marked(true);
                true
            } else {
                false
            }
        }
    }

    /// create new tracer for specified partition.
    pub(crate) fn new_internal(heap: NonNull<GcHeap>, partition_id: GcPartitionId) -> Self {
        unsafe {
            debug_assert!(heap.as_ref().partition(partition_id).is_some());
        }

        GcTracer {
            heap,
            partition_id,
            traced_nodes: VecDeque::new(),
            _mark: PhantomData,
        }
    }

    /// create new tracer for specified partition.
    /// clear all visit and mark flags to be ready for new tracing.
    pub fn new(heap: NonNull<GcHeap>, partition_id: GcPartitionId) -> Self {
        let tr = GcTracer::new_internal(heap, partition_id);

        // clear node flags
        unsafe {
            heap.as_ref().nodes_iter(partition_id).for_each(|mut n| {
                let mut f = n.as_ref().flags();
                let f0 = f;
                f.remove(GcHeadFlag::TRACED | GcHeadFlag::MARKED);
                if f0 != f {
                    n.as_mut().set_flags(f);
                }
            });
        }

        tr
    }

    #[inline(always)]
    pub const fn heap(&self) -> &GcHeap {
        unsafe { self.heap.as_ref() }
    }

    #[inline(always)]
    pub const fn heap_mut(&mut self) -> &mut GcHeap {
        unsafe { self.heap.as_mut() }
    }

    /// clear mark of each node in partition
    pub fn clear_marks(&mut self) {
        unsafe {
            self.heap
                .as_mut()
                .nodes_iter(self.partition_id)
                .for_each(|mut n| {
                    n.as_mut().set_marked(false);
                });
        }
    }

    /// clear visited flag of each node
    pub fn clear_visit_flags(&mut self) {
        unsafe {
            self.heap
                .as_mut()
                .nodes_iter(self.partition_id)
                .for_each(|mut n| {
                    let mut f = n.as_ref().flags();
                    let f0 = f;
                    f.remove(GcHeadFlag::TRACED);
                    if f0 != f {
                        n.as_mut().set_flags(f);
                    }
                });
        }
    }

    /// make trace ctx
    #[inline(always)]
    pub const fn ctx(&mut self) -> GcTraceOp<'_> {
        GcTraceOp {
            tr: NonNull::from_ref(self),
        }
    }

    /// Apply callback to `node`, and collect direct sub nodes for one depth
    pub(crate) fn trace_one(
        &mut self,
        node: NonNull<GcHead>,
        callback: impl Fn(NonNull<GcHead>, &GcHeap) -> bool,
        ignore_trace_flag: bool,
    ) {
        unsafe {
            if (ignore_trace_flag || !node.as_ref().flags().contains(GcHeadFlag::TRACED))
                && node.as_ref().get_partition_id() == self.partition_id
            {
                let propagate = callback(node, self.heap());

                if !ignore_trace_flag {
                    (*node.as_ptr()).set_flags(node.as_ref().flags().union(GcHeadFlag::TRACED));
                }

                if propagate {
                    // collect child nodes of `node`
                    (self.heap().get_node_gc_type(node).trace_fn)(node, self.ctx());
                }
            }
        }
    }

    /// Trace single `node` recursively for all descendant nodes,
    /// apply `callback` on each of them.
    pub fn trace(
        &mut self,
        node: NonNull<GcHead>,
        callback: impl Fn(NonNull<GcHead>, &GcHeap) -> bool,
    ) {
        // Note: to avoid cyclic dead loop, must check TRACED flag.
        self.trace_one(node, &callback, false);

        while let Some(n) = self.traced_nodes.pop_front() {
            self.trace_one(n, &callback, false); // may insert new traced nodes 
        }
    }

    /// Trace multiple nodes recursively.
    pub fn trace_iter(
        &mut self,
        iter: impl Iterator<Item = NonNull<GcHead>>,
        callback: impl Fn(NonNull<GcHead>, &GcHeap) -> bool,
    ) {
        for ptr in iter {
            self.trace(ptr, &callback);
        }
    }

    pub(crate) fn fix_xref_tree(&mut self, mut root_node: NonNull<GcHead>) {
        let n = unsafe { root_node.as_ref() };
        let xref = n.xref_partition();

        debug_assert!(!xref.is_null());
        debug_assert_eq!(n.get_partition_id(), self.partition_id);
        debug_assert!(n.is_root());

        let mut flags = n.flags();
        flags.insert(GcHeadFlag::TRACED);
        unsafe {
            root_node.as_mut().set_flags(flags);
        }

        (self.heap().get_node_gc_type(root_node).trace_fn)(root_node, self.ctx());

        for sub in std::mem::replace(&mut self.traced_nodes, VecDeque::new()) {
            unsafe {
                if sub.as_ref().get_partition_id() == self.partition_id {
                    self.set_xref_recursive(sub, xref);
                }
            }
        }
    }

    pub fn trace_roots(&mut self, handle: impl Fn(NonNull<GcHead>, &GcHeap) -> bool) {
        if let Some(roots) = unsafe { self.heap.as_ref().partition_roots.get(&self.partition_id) } {
            self.trace_iter(roots.iter().copied(), &handle);
        }
    }

    pub fn commit(&mut self, callback: impl Fn(NonNull<GcHead>, &GcHeap) -> bool) {
        while let Some(n) = self.traced_nodes.pop_front() {
            self.trace_one(n, &callback, false);
        }
    }

    /// clear collected nodes
    pub fn clear(&mut self) {
        self.traced_nodes.clear();
    }

    pub(crate) fn set_xref_recursive(&mut self, mut node: NonNull<GcHead>, xref: GcPartitionId) {
        let n = unsafe { node.as_ref() };

        debug_assert!(!xref.is_null());
        debug_assert_eq!(n.get_partition_id(), self.partition_id);

        let xref0 = n.xref_partition();
        let fix = if !xref0.is_null() {
            self.heap().common_parent2(xref, xref0)
        } else {
            xref
        };

        let trace_sub = if fix != xref0 {
            unsafe {
                node.as_mut().set_xref_partition(fix);
            }
            log::trace!("[fix_xref]: {n:?} -> {fix:?}");
            true
        } else {
            let mut flags = n.flags();
            flags.insert(GcHeadFlag::TRACED);
            unsafe {
                node.as_mut().set_flags(flags);
            }
            false
        };

        if trace_sub {
            (self.heap().get_node_gc_type(node).trace_fn)(node, self.ctx());

            let children = std::mem::replace(&mut self.traced_nodes, VecDeque::new());
            for sub in children {
                unsafe {
                    if sub.as_ref().get_partition_id() == self.partition_id
                        && !sub.as_ref().flags().contains(GcHeadFlag::TRACED)
                    {
                        self.set_xref_recursive(sub, fix);
                    }
                }
            }
        }
    }
}

/// tracer operation
#[derive(Clone, Copy)]
pub struct GcTraceOp<'a> {
    tr: NonNull<GcTracer<'a>>,
}

impl<'a> GcTraceOp<'a> {
    /// reference to heap
    #[inline(always)]
    pub const fn heap(&self) -> &GcHeap {
        unsafe { &*self.tr.as_ref().heap.as_ptr() }
    }

    /// Submit a gc refrence to tracer, whilch will be traced later.
    #[inline(always)]
    pub fn add<T: GcTracable>(&mut self, gc_ref: GcRef<T>) {
        self.add_node(gc_ref.head_ptr);
    }

    /// Submit a node to tracer, whilch will be traced later.
    #[inline(always)]
    pub fn add_node(&mut self, node: NonNull<GcHead>) {
        unsafe {
            self.tr.as_mut().traced_nodes.push_back(node);
        }
    }

    /// Submit nodes to tracer, whilch will be traced later.
    pub fn add_nodes(&mut self, nodes: impl Iterator<Item = NonNull<GcHead>>) {
        for n in nodes {
            self.add_node(n);
        }
    }
}

impl GcHeap {
    /// A shortcut to GcTracer::new()
    #[inline(always)]
    pub fn tracer(&self, partition_id: GcPartitionId) -> GcTracer<'_> {
        GcTracer::new(NonNull::from(self), partition_id)
    }
}

#[macro_export]
macro_rules! impl_trace_for_basic {
    ($($ty:ty),*) => {
        $(
            unsafe impl GcTracable for $ty {
                #[inline(always)]
                fn trace(&self, _: GcTraceOp) {
                    // This type doesn't contain any GC references, so trace method is empty
                }
            }
        )*
    };
}

// Implement GcTracable for basic types
impl_trace_for_basic!(
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
    fn trace(&self, _: GcTraceOp) {
        // String don't have gc ref
    }
}

unsafe impl<T: GcTracable> GcTracable for Option<T> {
    #[inline(always)]
    fn trace(&self, tr: GcTraceOp) {
        if let Some(v) = self {
            v.trace(tr);
        }
    }
}

unsafe impl<T: GcTracable> GcTracable for Box<T> {
    #[inline(always)]
    fn trace(&self, tr: GcTraceOp) {
        self.as_ref().trace(tr);
    }
}

unsafe impl<T: GcTracable> GcTracable for [T] {
    #[inline]
    fn trace(&self, tr: GcTraceOp) {
        for n in self {
            n.trace(tr);
        }
    }
}

unsafe impl<T: GcTracable> GcTracable for Vec<T> {
    #[inline]
    fn trace(&self, tr: GcTraceOp) {
        for n in self {
            n.trace(tr);
        }
    }
}

unsafe impl<T: GcTracable> GcTracable for Box<[T]> {
    #[inline(always)]
    fn trace(&self, tr: GcTraceOp) {
        for n in self {
            n.trace(tr);
        }
    }
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
        fn trace(&self, mut tr: GcTraceOp) {
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

        // Create tracer and trace with Propagate (using MARK_FUNC)
        let mut tracer = heap.tracer(partition_id);

        tracer.trace(root_ref.node_ptr(), GcTracer::MARK_FUNC);

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

        // Create a custom handle that marks on first visit, prevents on subsequent visits
        fn continue_handle(mut node: NonNull<GcHead>, _: &GcHeap) -> bool {
            unsafe {
                if !node.as_ref().is_marked() {
                    node.as_mut().set_marked(true);
                    true
                } else {
                    false
                }
            }
        }

        // Create tracer and trace with Continue
        let mut tracer = heap.tracer(partition_id);
        tracer.trace(root_ref.node_ptr(), continue_handle);

        // Verify all nodes are marked
        assert_eq!(count_marked_nodes(tracer.heap(), partition_id), 3);

        // Verify pendings is empty after processing
        assert!(tracer.traced_nodes.is_empty());
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
        let mut tracer1 = heap.tracer(partition_id);
        tracer1.trace(level0_ref.node_ptr(), GcTracer::MARK_FUNC);
        assert_eq!(count_marked_nodes(&heap, partition_id), 4);

        // Test with Continue
        let mut tracer2 = heap.tracer(partition_id);

        // Create a handle that marks nodes on first visit, prevents on subsequent visits
        fn continue_and_mark_handle(mut node: NonNull<GcHead>, _: &GcHeap) -> bool {
            unsafe {
                if !node.as_ref().is_marked() {
                    node.as_mut().set_marked(true);
                    true
                } else {
                    false
                }
            }
        }

        tracer2.trace(level0_ref.node_ptr(), continue_and_mark_handle);
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
        let mut tracer1 = heap.tracer(partition_id);
        tracer1.trace(root_ref.node_ptr(), GcTracer::MARK_FUNC);
        assert_eq!(count_marked_nodes(&heap, partition_id), 7);

        // Test with Continue
        let mut tracer2 = heap.tracer(partition_id);

        fn continue_and_mark_handle(mut node: NonNull<GcHead>, _: &GcHeap) -> bool {
            unsafe {
                if !node.as_ref().is_marked() {
                    node.as_mut().set_marked(true);
                    true
                } else {
                    false
                }
            }
        }

        tracer2.trace(root_ref.node_ptr(), continue_and_mark_handle);
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
        let mut tracer1 = heap.tracer(partition_id);
        tracer1.trace(nodes[0].node_ptr(), GcTracer::MARK_FUNC);
        let propagate_marked = count_marked_nodes(&heap, partition_id);

        // Test with Continue
        let mut tracer2 = heap.tracer(partition_id);

        fn continue_and_mark_handle(mut node: NonNull<GcHead>, _: &GcHeap) -> bool {
            unsafe {
                if !node.as_ref().is_marked() {
                    node.as_mut().set_marked(true);
                    true
                } else {
                    false
                }
            }
        }

        tracer2.trace(nodes[0].node_ptr(), continue_and_mark_handle);
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
        let mut tracer1 = heap.tracer(partition_id);
        tracer1.trace(node1.node_ptr(), GcTracer::MARK_FUNC);

        // Both nodes should be marked
        assert_eq!(count_marked_nodes(&heap, partition_id), 2);

        // Test with Continue
        let mut tracer2 = heap.tracer(partition_id);

        fn continue_and_mark_handle(mut node: NonNull<GcHead>, _: &GcHeap) -> bool {
            unsafe {
                if !node.as_ref().is_marked() {
                    node.as_mut().set_marked(true);
                    true
                } else {
                    false
                }
            }
        }

        tracer2.trace(node1.node_ptr(), continue_and_mark_handle);
        assert_eq!(count_marked_nodes(&heap, partition_id), 2);
    }
}
