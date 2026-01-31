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
    fn trace(&self, tr: GcTraceOps);
}

/// Tracer, used to trace object references during marking phase
pub struct GcTracer<'a> {
    pub(super) heap: NonNull<GcHeap>,
    pub(super) partition_id: GcPartitionId,

    /// pending nodes to be marked later
    pub(super) pendings: VecDeque<NonNull<GcHead>>,

    _mark: PhantomData<&'a ()>,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GcTraceOp {
    /// Depth first
    Propagate,
    /// Broad first
    Continue,
    /// Don't trace
    Prevent,
}

impl<'a> GcTracer<'a> {
    #[allow(non_snake_case)]
    pub fn MARK_FUNC(mut h: NonNull<GcHead>, _: &GcHeap) -> GcTraceOp {
        unsafe {
            if !h.as_ref().is_marked() {
                h.as_mut().set_marked(true);
                GcTraceOp::Propagate
            } else {
                GcTraceOp::Prevent
            }
        }
    }

    fn new(heap: NonNull<GcHeap>, partition_id: GcPartitionId) -> Self {
        let tr = GcTracer {
            heap,
            partition_id,
            pendings: VecDeque::new(),
            _mark: PhantomData,
        };

        // clear node flags
        unsafe {
            heap.as_ref().nodes_iter(partition_id).for_each(|mut n| {
                let mut f = n.as_ref().flags();
                let f0 = f;
                f.remove(GcHeadFlag::TRACE_DONE | GcHeadFlag::MARKED);
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

    /// get user opaque pointer on GcHeap
    #[inline(always)]
    pub const fn opaque(&self) -> *mut u8 {
        unsafe { self.heap.as_ref().opaque() }
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
                    f.remove(GcHeadFlag::TRACE_DONE);
                    if f0 != f {
                        n.as_mut().set_flags(f);
                    }
                });
        }
    }

    /// make trace ops
    #[inline(always)]
    pub(crate) const fn ops(&mut self) -> GcTraceOps<'_> {
        GcTraceOps(NonNull::from_ref(self))
    }

    fn do_trace(
        &mut self,
        node: NonNull<GcHead>,
        callback: impl Fn(NonNull<GcHead>, &GcHeap) -> GcTraceOp,
    ) {
        unsafe {
            if !node.as_ref().flags().contains(GcHeadFlag::TRACE_DONE)
                && node.as_ref().get_partition_id() == self.partition_id
            {
                match callback(node, self.heap()) {
                    GcTraceOp::Continue | GcTraceOp::Propagate => {
                        (*node.as_ptr())
                            .set_flags(node.as_ref().flags().union(GcHeadFlag::TRACE_DONE));

                        let tt = &self.heap.as_ref().type_registry;
                        let trace_fn = node.as_ref().get_trace_fn(tt);
                        trace_fn(node, self.ops());
                    }
                    // GcTraceOp::Continue => {
                    //     self.pendings.push_back(node);
                    // }
                    GcTraceOp::Prevent => {
                        (*node.as_ptr())
                            .set_flags(node.as_ref().flags().union(GcHeadFlag::TRACE_DONE));
                    }
                }
            }
        }
    }

    pub fn trace(
        &mut self,
        node: NonNull<GcHead>,
        callback: impl Fn(NonNull<GcHead>, &GcHeap) -> GcTraceOp,
    ) {
        self.do_trace(node, &callback);

        if !self.pendings.is_empty() {
            self.commit_with(callback);
        }
    }

    pub fn trace_iter(
        &mut self,
        iter: impl Iterator<Item = NonNull<GcHead>>,
        callback: impl Fn(NonNull<GcHead>, &GcHeap) -> GcTraceOp,
    ) {
        for ptr in iter {
            self.do_trace(ptr, &callback);
        }

        if !self.pendings.is_empty() {
            self.commit_with(callback);
        }
    }

    /// commit to trace pending nodes
    pub fn commit_with(&mut self, callback: impl Fn(NonNull<GcHead>, &GcHeap) -> GcTraceOp) {
        while let Some(ptr) = self.pendings.pop_front() {
            self.do_trace(ptr, &callback);
        }
    }

    pub fn trace_roots(&mut self, handle: impl Fn(NonNull<GcHead>, &GcHeap) -> GcTraceOp) {
        if let Some(roots) = unsafe { self.heap.as_ref().partition_roots.get(&self.partition_id) } {
            self.trace_iter(roots.iter().copied(), &handle);
        }
    }

    /// clear all pending nodes to be traced.
    pub fn clear(&mut self) {
        self.pendings.clear();
    }
}

#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct GcTraceOps<'a>(NonNull<GcTracer<'a>>);

impl<'a> GcTraceOps<'a> {
    /// Submit a refrenced node to tracer, whilch will be traced later.
    #[inline]
    pub fn submit<T: GcTracable>(&mut self, gc_ref: GcRef<T>) {
        unsafe {
            self.0.as_mut().pendings.push_back(gc_ref.head_ptr);
        }
    }
}

impl GcHeap {
    /// create new tracer for specified partition.
    /// clear all visit and mark flags to be ready for new tracing.
    #[inline(always)]
    pub fn tracer(&mut self, partition_id: GcPartitionId) -> GcTracer<'_> {
        GcTracer::new(NonNull::from(self), partition_id)
    }
}

#[macro_export]
macro_rules! impl_collect_for_basic {
    ($($ty:ty),*) => {
        $(
            unsafe impl GcTracable for $ty {
                #[inline(always)]
                fn trace(&self, _: GcTraceOps) {
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
    fn trace(&self, _: GcTraceOps) {
        // String don't have gc ref
    }
}

unsafe impl<T: GcTracable> GcTracable for Option<T> {
    #[inline]
    fn trace(&self, tr: GcTraceOps) {
        if let Some(v) = self {
            v.trace(tr);
        }
    }
}

unsafe impl<T: GcTracable> GcTracable for Box<T> {
    #[inline(always)]
    fn trace(&self, tr: GcTraceOps) {
        self.as_ref().trace(tr);
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
        fn trace(&self, mut tr: GcTraceOps) {
            println!(
                "TestNode::trace({self:p}), {} children",
                self.children.len()
            );

            for (i, child) in self.children.iter().enumerate() {
                println!("  Tracing child {}: {:?}", i, child.head_ptr());
                tr.submit(*child);
            }
        }
    }

    /// Helper function to count marked nodes in a partition
    fn count_marked_nodes(heap: &GcHeap, partition_id: GcPartitionId) -> usize {
        let mut count = 0;
        if let Some(head) = heap.partition_heads.get(&partition_id).copied().flatten() {
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
        if let Some(head) = heap.partition_heads.get(&partition_id).copied().flatten() {
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
        println!("Root: {:?}", root_ref.head_ptr());
        println!("Child1: {:?}", child1.head_ptr());
        println!("Child2: {:?}", child2.head_ptr());

        // Create tracer and trace with Propagate (using MARK_FUNC)
        let mut tracer = heap.tracer(partition_id);

        tracer.trace(root_ref.head_ptr(), GcTracer::MARK_FUNC);

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
        fn continue_handle(mut node: NonNull<GcHead>, _: &GcHeap) -> GcTraceOp {
            unsafe {
                if !node.as_ref().is_marked() {
                    node.as_mut().set_marked(true);
                    GcTraceOp::Continue
                } else {
                    GcTraceOp::Prevent
                }
            }
        }

        // Create tracer and trace with Continue
        let mut tracer = heap.tracer(partition_id);
        tracer.trace(root_ref.head_ptr(), continue_handle);

        // Verify all nodes are marked
        assert_eq!(count_marked_nodes(tracer.heap(), partition_id), 3);

        // Verify pendings is empty after processing
        assert!(tracer.pendings.is_empty());
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
        tracer1.trace(level0_ref.head_ptr(), GcTracer::MARK_FUNC);
        assert_eq!(count_marked_nodes(&heap, partition_id), 4);

        // Test with Continue
        let mut tracer2 = heap.tracer(partition_id);

        // Create a handle that marks nodes on first visit, prevents on subsequent visits
        fn continue_and_mark_handle(mut node: NonNull<GcHead>, _: &GcHeap) -> GcTraceOp {
            unsafe {
                if !node.as_ref().is_marked() {
                    node.as_mut().set_marked(true);
                    GcTraceOp::Continue
                } else {
                    GcTraceOp::Prevent
                }
            }
        }

        tracer2.trace(level0_ref.head_ptr(), continue_and_mark_handle);
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
        tracer1.trace(root_ref.head_ptr(), GcTracer::MARK_FUNC);
        assert_eq!(count_marked_nodes(&heap, partition_id), 7);

        // Test with Continue
        let mut tracer2 = heap.tracer(partition_id);

        fn continue_and_mark_handle(mut node: NonNull<GcHead>, _: &GcHeap) -> GcTraceOp {
            unsafe {
                if !node.as_ref().is_marked() {
                    node.as_mut().set_marked(true);
                    GcTraceOp::Continue
                } else {
                    GcTraceOp::Prevent
                }
            }
        }

        tracer2.trace(root_ref.head_ptr(), continue_and_mark_handle);
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
        tracer1.trace(nodes[0].head_ptr(), GcTracer::MARK_FUNC);
        let propagate_marked = count_marked_nodes(&heap, partition_id);

        // Test with Continue
        let mut tracer2 = heap.tracer(partition_id);

        fn continue_and_mark_handle(mut node: NonNull<GcHead>, _: &GcHeap) -> GcTraceOp {
            unsafe {
                if !node.as_ref().is_marked() {
                    node.as_mut().set_marked(true);
                    GcTraceOp::Continue
                } else {
                    GcTraceOp::Prevent
                }
            }
        }

        tracer2.trace(nodes[0].head_ptr(), continue_and_mark_handle);
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
        tracer1.trace(node1.head_ptr(), GcTracer::MARK_FUNC);

        // Both nodes should be marked
        assert_eq!(count_marked_nodes(&heap, partition_id), 2);

        // Test with Continue
        let mut tracer2 = heap.tracer(partition_id);

        fn continue_and_mark_handle(mut node: NonNull<GcHead>, _: &GcHeap) -> GcTraceOp {
            unsafe {
                if !node.as_ref().is_marked() {
                    node.as_mut().set_marked(true);
                    GcTraceOp::Continue
                } else {
                    GcTraceOp::Prevent
                }
            }
        }

        tracer2.trace(node1.head_ptr(), continue_and_mark_handle);
        assert_eq!(count_marked_nodes(&heap, partition_id), 2);
    }
}
