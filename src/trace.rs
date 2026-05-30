// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::VecDeque, marker::PhantomData, ptr::NonNull};

use crate::{GcHeap, GcNode, GcPartitionId, GcRef, node::GcHead};

pub trait GcTrace: 'static {
    /// Collect directly referenced children gc nodes
    fn trace(&self, gcx: &mut GcTraceCtx);

    /// Get direct referencing children nodes
    fn gc_children(&self, heap: &GcHeap) -> Vec<NonNull<GcHead>> {
        let mut gcx = heap.create_trace_ctx(64);
        self.trace(&mut gcx);
        gcx.traced_nodes
    }
}

pub struct GcTraceCtx<'a> {
    pub(crate) traced_nodes: Vec<NonNull<GcHead>>,
    opaque: *mut u8,
    _mark: PhantomData<&'a ()>,
}

impl<'a> GcTraceCtx<'a> {
    #[inline(always)]
    pub const fn opaque(&self) -> *mut u8 {
        self.opaque
    }

    /// Submit a node to collected list regardless its color state.
    pub fn add_node(&mut self, node: NonNull<GcHead>) {
        #[cfg(debug_assertions)]
        unsafe {
            node.as_ref().debug_assert_node_valid_simple();
        }

        // if !self.traced_nodes.contains(&node) {
        //     self.traced_nodes.push_back(node);
        // }

        self.traced_nodes.push(node);
    }

    /// Submit a GcRef to collected list
    #[inline(always)]
    pub fn add<T: GcNode>(&mut self, gc_ref: GcRef<T>) {
        self.add_node(gc_ref.head_ptr);
    }

    #[inline(always)]
    pub fn take_nodes(&mut self) -> Vec<NonNull<GcHead>> {
        std::mem::take(&mut self.traced_nodes)
    }
}

impl GcHeap {
    pub fn create_trace_ctx(&self, cap: usize) -> GcTraceCtx<'_> {
        GcTraceCtx {
            traced_nodes: Vec::with_capacity(cap),
            opaque: self.opaque(),
            _mark: PhantomData,
        }
    }

    /// Trace direct children of a node into the given trace context
    pub fn trace_node(&self, node: NonNull<GcHead>, gcx: &mut GcTraceCtx) {
        unsafe {
            (self
                .node_dtypes
                .type_info_list
                .get_unchecked(node.as_ref().dtype() as usize)
                .trace_fn)(node, gcx);
        }
    }

    pub fn traverse_start(&mut self, partition_id: GcPartitionId) {
        for mut node in self.nodes(partition_id) {
            unsafe {
                node.as_mut().set_traverse_visited(false);
            }
        }
    }

    /// Traverses the node tree starting at `node` in depth-first order,
    /// invoking `callback` on each visited node with its optional parent.
    /// If `filter` is non-null, only nodes in the specified partition are visited.
    pub fn traverse(
        &mut self,
        node: NonNull<GcHead>,
        filter: GcPartitionId,
        mut callback: impl FnMut(NonNull<GcHead>, Option<NonNull<GcHead>>),
    ) {
        let mut stack: VecDeque<(NonNull<GcHead>, Option<NonNull<GcHead>>)> =
            vec![(node, None)].into();

        let mut gcx = self.create_trace_ctx(64);

        while let Some((mut current, parent)) = stack.pop_front() {
            unsafe {
                #[cfg(debug_assertions)]
                current.as_ref().debug_assert_node_valid(self);

                if current.as_ref().traverse_visited() {
                    continue;
                }

                current.as_mut().set_traverse_visited(true);

                if filter.is_null() || filter == current.as_ref().partition_id() {
                    callback(current, parent);
                }

                self.trace_node(current, &mut gcx);

                while let Some(child) = gcx.traced_nodes.pop() {
                    if !child.as_ref().traverse_visited() {
                        stack.push_back((child, Some(current)));
                    }
                }
            }
        }
    }
}

macro_rules! impl_dummy_trace_for_primitive {
    ($($ty:ty),*) => {
        $(
            impl GcTrace for $ty {
                #[inline(always)]
                fn trace(&self, _: &mut GcTraceCtx) { }
            }

            impl GcTrace for [$ty] {
                #[inline(always)]
                fn trace(&self, _: &mut GcTraceCtx) { }
            }

            impl GcTrace for Vec<$ty> {
                #[inline(always)]
                fn trace(&self, _: &mut GcTraceCtx) { }
            }

            impl GcTrace for Box<[$ty]> {
                #[inline(always)]
                fn trace(&self, _: &mut GcTraceCtx) { }
            }
        )*
    };
}

// Implement GcTrace for basic types
impl_dummy_trace_for_primitive!(
    u8, u16, u32, u64, u128, i8, i16, i32, i64, i128, f32, f64, usize, isize, bool, char
);

impl GcTrace for str {
    #[inline(always)]
    fn trace(&self, _: &mut GcTraceCtx) {}
}

impl GcTrace for &'static str {
    #[inline(always)]
    fn trace(&self, _: &mut GcTraceCtx) {}
}

impl GcTrace for String {
    #[inline(always)]
    fn trace(&self, _: &mut GcTraceCtx) {}
}

impl GcTrace for &'static String {
    #[inline(always)]
    fn trace(&self, _: &mut GcTraceCtx) {}
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{GcHeap, GcRef, node::GcTriColor};

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

    impl GcTrace for TestNode {
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

    crate::gc_type_register! {
        TestNode, drop_pass = 0;
        Align32Node, drop_pass = 0;
    }

    /// Helper function to count marked nodes in a partition
    fn count_non_white_nodes(heap: &GcHeap, partition_id: GcPartitionId) -> usize {
        let mut count = 0;
        for node in heap.nodes(partition_id) {
            unsafe {
                if node.as_ref().color() != GcTriColor::White {
                    count += 1;
                }
            }
        }
        count
    }

    /// Helper function to get all node IDs in a partition
    fn get_all_node_ids(heap: &GcHeap, partition_id: GcPartitionId) -> Vec<u32> {
        let mut ids = Vec::new();
        let info = &GC_TYPE_REGISTRY.type_info_list[TestNode::GC_TYPE_ID as usize];
        for node in heap.nodes(partition_id) {
            unsafe {
                let payload_ptr = info.payload_ptr(node);
                let id = payload_ptr.cast::<TestNode>().as_ref().id;
                ids.push(id);
            }
        }
        ids
    }

    /// Test 1: Simple tree structure with Propagate (depth-first)
    #[test]
    fn test_trace_propagate_simple_tree() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let child1 = unsafe { heap.alloc_raw(partition_id, TestNode::new(1)) }.unwrap();
        let child2 = unsafe { heap.alloc_raw(partition_id, TestNode::new(2)) }.unwrap();

        let mut root = TestNode::new(0);
        root.add_child(child1);
        root.add_child(child2);
        let root_ref = unsafe { heap.alloc_root_raw(partition_id, root) }.unwrap();

        // Debug: print node pointers
        println!("Root: {:?}", root_ref.node_ptr());
        println!("Child1: {:?}", child1.node_ptr());
        println!("Child2: {:?}", child2.node_ptr());

        // Mark reachable nodes using GC mark algorithm
        while !heap.mark(partition_id, 16) {}

        // check marks after tracing
        println!(
            "Marks after tracing: {}",
            count_non_white_nodes(&heap, partition_id)
        );

        // Verify all nodes are marked
        assert_eq!(count_non_white_nodes(&heap, partition_id), 3);

        // Verify all node IDs are present
        let ids = get_all_node_ids(&heap, partition_id);
        assert!(ids.contains(&0));
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
    }

    /// Test 2: Simple tree structure with Continue (breadth-first)
    #[test]
    fn test_trace_continue_simple_tree() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let child1 = unsafe { heap.alloc_raw(partition_id, TestNode::new(1)) }.unwrap();
        let child2 = unsafe { heap.alloc_raw(partition_id, TestNode::new(2)) }.unwrap();

        let mut root = TestNode::new(0);
        root.add_child(child1);
        root.add_child(child2);
        let root_ref = unsafe { heap.alloc_root_raw(partition_id, root) }.unwrap();
        while !heap.mark(partition_id, 1) {}

        // Verify all nodes are marked
        assert_eq!(count_non_white_nodes(&heap, partition_id), 3);
    }

    /// Test 3: Deep nested tree with both algorithms
    #[test]
    fn test_trace_deep_nested_tree() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let level3 = unsafe { heap.alloc_raw(partition_id, TestNode::new(3)) }.unwrap();

        let mut level2 = TestNode::new(2);
        level2.add_child(level3);
        let level2_ref = unsafe { heap.alloc_raw(partition_id, level2) }.unwrap();

        let mut level1 = TestNode::new(1);
        level1.add_child(level2_ref);
        let level1_ref = unsafe { heap.alloc_raw(partition_id, level1) }.unwrap();

        let mut level0 = TestNode::new(0);
        level0.add_child(level1_ref);
        let level0_ref = unsafe { heap.alloc_root_raw(partition_id, level0) }.unwrap();

        // Mark reachable nodes
        while !heap.mark(partition_id, 4) {}
        assert_eq!(count_non_white_nodes(&heap, partition_id), 4);
    }

    /// Test 4: Complex tree with multiple branches
    #[test]
    fn test_trace_complex_tree() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        // Create a complex tree:
        //        root
        //       /    \
        //      a      b
        //     / \    / \
        //    c   d  e   f

        let c = unsafe { heap.alloc_raw(partition_id, TestNode::new(3)) }.unwrap();
        let d = unsafe { heap.alloc_raw(partition_id, TestNode::new(4)) }.unwrap();
        let e = unsafe { heap.alloc_raw(partition_id, TestNode::new(5)) }.unwrap();
        let f = unsafe { heap.alloc_raw(partition_id, TestNode::new(6)) }.unwrap();

        let mut a = TestNode::new(1);
        a.add_child(c);
        a.add_child(d);
        let a_ref = unsafe { heap.alloc_raw(partition_id, a) }.unwrap();

        let mut b = TestNode::new(2);
        b.add_child(e);
        b.add_child(f);
        let b_ref = unsafe { heap.alloc_raw(partition_id, b) }.unwrap();

        let mut root = TestNode::new(0);
        root.add_child(a_ref);
        root.add_child(b_ref);
        unsafe { heap.alloc_root_raw(partition_id, root) }.unwrap();

        // Mark reachable nodes
        while !heap.mark(partition_id, 8) {}
        assert_eq!(count_non_white_nodes(&heap, partition_id), 7);
    }

    /// Test 5: Verify both algorithms produce same result
    #[test]
    fn test_trace_algorithms_equivalence() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        // Create a tree with 10 nodes in a balanced structure
        let mut nodes = Vec::new();
        for i in 0..10 {
            nodes.push(unsafe { heap.alloc_raw(partition_id, TestNode::new(i as u32)) }.unwrap());
        }

        // Build tree: 0 -> 1,2; 1 -> 3,4; 2 -> 5,6; 3 -> 7,8; 4 -> 9
        {
            let mut nodes = nodes.clone();
            let n = nodes[1];
            nodes[0].with_mut(&mut heap, |node| node.add_child(n));

            let n = nodes[2];
            nodes[0].with_mut(&mut heap, |node| node.add_child(n));

            let n = nodes[3];
            nodes[1].with_mut(&mut heap, |node| node.add_child(n));

            let n = nodes[4];
            nodes[1].with_mut(&mut heap, |node| node.add_child(n));

            let n = nodes[5];
            nodes[2].with_mut(&mut heap, |node| node.add_child(n));

            let n = nodes[6];
            nodes[2].with_mut(&mut heap, |node| node.add_child(n));

            let n = nodes[7];
            nodes[3].with_mut(&mut heap, |node| node.add_child(n));

            let n = nodes[8];
            nodes[3].with_mut(&mut heap, |node| node.add_child(n));

            let n = nodes[9];
            nodes[4].with_mut(&mut heap, |node| node.add_child(n));
        }

        let mut root = TestNode::new(100);
        root.add_child(nodes[0]);
        let _ = unsafe { heap.alloc_root_raw(partition_id, root) }.unwrap();
        while !heap.mark(partition_id, 16) {}
        let marks1 = count_non_white_nodes(&heap, partition_id);

        // Reset colors and mark again with smaller step limit
        heap.mark_reset(partition_id);
        while !heap.mark(partition_id, 1) {}
        let marks2 = count_non_white_nodes(&heap, partition_id);

        // Both algorithms should mark the same number of nodes
        assert_eq!(marks1, marks2);
        assert_eq!(marks1, 11);
    }

    /// Test 6: Circular reference handling
    #[test]
    fn test_trace_circular_reference() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let mut node1 = unsafe { heap.alloc_raw(partition_id, TestNode::new(1)) }.unwrap();
        let mut node2 = unsafe { heap.alloc_raw(partition_id, TestNode::new(2)) }.unwrap();

        {
            node1.with_mut(&mut heap, |n| n.add_child(node2));
            node2.with_mut(&mut heap, |n| n.add_child(node1));
        }

        // Mark reachable nodes - should handle circular reference without infinite loop
        let mut root = TestNode::new(100);
        root.add_child(node1);
        let _ = unsafe { heap.alloc_root_raw(partition_id, root) }.unwrap();
        while !heap.mark(partition_id, 4) {}

        // Both nodes should be marked
        assert_eq!(
            count_non_white_nodes(&heap, partition_id),
            3,
            "Propagate should handle circular reference"
        );

        heap.mark_reset(partition_id);
        while !heap.mark(partition_id, 1) {}
        assert_eq!(count_non_white_nodes(&heap, partition_id), 3);
    }

    // ============ Write barrier tests ============

    /// Test that the write barrier correctly re-grays a black node when a new
    /// white child is added during marking, preventing the child from being
    /// incorrectly swept.
    #[test]
    fn test_write_barrier_black_node_adds_white_child() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        // Allocate a white child node (not reachable from root yet)
        let child = unsafe { heap.alloc_raw(partition_id, TestNode::new(1)) }.unwrap();

        // Allocate root with no children initially
        let mut root = unsafe { heap.alloc_root_raw(partition_id, TestNode::new(0)) }.unwrap();

        // Run mark until root is black but child is still white.
        // Since child is not reachable from root, only root should be marked.
        while !heap.mark(partition_id, 1) {}
        assert_eq!(count_non_white_nodes(&heap, partition_id), 1);

        // Now add the white child to the black root via with_mut().
        // The write barrier should re-gray the root and enqueue it.
        root.with_mut(&mut heap, |node| node.add_child(child));

        // Continue marking. The root should be traced again, discovering the child.
        while !heap.mark(partition_id, 1) {}

        // Both root and child should now be black (marked).
        assert_eq!(
            count_non_white_nodes(&heap, partition_id),
            2,
            "Write barrier should have re-grayed root and discovered child"
        );

        // Sweep should not free any nodes since all are marked.
        let freed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(freed, 0, "No nodes should be freed after write barrier");

        // Verify both nodes are still accessible
        let ids = get_all_node_ids(&heap, partition_id);
        assert!(ids.contains(&0));
        assert!(ids.contains(&1));
    }

    /// Test that the write barrier works correctly with incremental marking:
    /// multiple black nodes add white children across several mark steps.
    #[test]
    fn test_write_barrier_incremental_black_adds_white() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        // Create a chain: root -> a -> b -> c
        let c = unsafe { heap.alloc_raw(partition_id, TestNode::new(3)) }.unwrap();
        let mut b = unsafe { heap.alloc_raw(partition_id, TestNode::new(2)) }.unwrap();
        b.with_mut(&mut heap, |node| node.add_child(c));

        let mut a = unsafe { heap.alloc_raw(partition_id, TestNode::new(1)) }.unwrap();
        a.with_mut(&mut heap, |node| node.add_child(b));

        let mut root = unsafe { heap.alloc_root_raw(partition_id, TestNode::new(0)) }.unwrap();
        root.with_mut(&mut heap, |node| node.add_child(a));

        // Mark partially: only 1 node per step, so root becomes black,
        // a becomes gray, b and c stay white.
        while !heap.mark(partition_id, 1) {}

        // At this point root is black, a is black, b is gray/black, c is white.
        // Now add a NEW white child to the black root.
        let new_child = unsafe { heap.alloc_raw(partition_id, TestNode::new(10)) }.unwrap();
        root.with_mut(&mut heap, |node| node.add_child(new_child));

        // Continue marking to completion.
        while !heap.mark(partition_id, 1) {}

        // All 5 nodes should be marked.
        assert_eq!(
            count_non_white_nodes(&heap, partition_id),
            5,
            "Write barrier should have preserved the new child added during marking"
        );

        // Sweep should not free anything.
        let freed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(freed, 0);
    }

    /// Test that bypassing the write barrier (via direct payload mutation) causes
    /// incorrect collection of a white child added to a black node during marking.
    /// This test documents the known soundness hole when write barrier is bypassed.
    #[test]
    fn test_write_barrier_bypass_leaks_white_child() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let child = unsafe { heap.alloc_raw(partition_id, TestNode::new(1)) }.unwrap();
        let mut root = unsafe { heap.alloc_root_raw(partition_id, TestNode::new(0)) }.unwrap();

        // Mark until root is black, child is white.
        while !heap.mark(partition_id, 1) {}
        assert_eq!(count_non_white_nodes(&heap, partition_id), 1);

        // Bypass the write barrier by directly mutating the payload through
        // the raw head_ptr. This is the unsafe pattern we want to prevent.
        unsafe {
            let payload = root
                .head_ptr
                .as_mut()
                .payload_for::<TestNode>()
                .cast::<TestNode>()
                .as_mut();
            payload.children.push(child);
        }

        // Continue marking. Since the write barrier was bypassed,
        // root stays black and the white child is never discovered.
        while !heap.mark(partition_id, 1) {}

        // Only root should be marked; child remains white.
        assert_eq!(
            count_non_white_nodes(&heap, partition_id),
            1,
            "Without write barrier, the white child should remain unmarked"
        );

        // Sweep will free the white child (freed > 0 indicates at least one node was collected).
        let freed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(
            freed > 0,
            "The white child should be swept without write barrier"
        );

        // Only root should remain.
        let ids = get_all_node_ids(&heap, partition_id);
        assert!(ids.contains(&0));
        assert!(!ids.contains(&1), "Child should have been collected");
    }

    // ============ High-alignment payload tests ============

    #[repr(align(32))]
    #[derive(Debug)]
    struct Align32Node {
        id: u64,
        data: [u8; 64],
    }

    impl GcTrace for Align32Node {
        fn trace(&self, _: &mut GcTraceCtx) {}
    }

    #[test]
    fn test_high_alignment_payload_alloc_and_access() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let node: GcRef<Align32Node> = unsafe {
            heap.alloc_root_raw(
                partition_id,
                Align32Node {
                    id: 42,
                    data: [0xAB; 64],
                },
            )
        }
        .unwrap();

        // Verify payload is accessible and values are correct
        assert_eq!(node.id, 42);
        assert_eq!(node.data[0], 0xAB);
        assert_eq!(node.data[63], 0xAB);

        // Verify alignment via pointer arithmetic
        let payload_ptr = node.as_ptr().as_ptr() as usize;
        assert_eq!(
            payload_ptr % 32,
            0,
            "Align32Node payload must be 32-byte aligned, got offset {}",
            payload_ptr % 32
        );

        // GC should not collect root nodes
        while !heap.mark(partition_id, 64) {}
        let freed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(freed, 0);

        // Values still accessible after GC
        assert_eq!(node.id, 42);
    }

    #[test]
    fn test_high_alignment_payload_multiple_nodes() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let nodes: Vec<GcRef<Align32Node>> = (0..10)
            .map(|i| {
                unsafe {
                    heap.alloc_root_raw(
                        partition_id,
                        Align32Node {
                            id: i as u64,
                            data: [i as u8; 64],
                        },
                    )
                }
                .unwrap()
            })
            .collect();

        // Verify all nodes are accessible and correctly aligned
        for (i, node) in nodes.iter().enumerate() {
            assert_eq!(node.id, i as u64);
            assert_eq!(node.data[0], i as u8);
            assert_eq!(node.data[63], i as u8);

            let payload_ptr = node.as_ptr().as_ptr() as usize;
            assert_eq!(
                payload_ptr % 32,
                0,
                "node[{}] payload must be 32-byte aligned",
                i
            );
        }

        // GC should not collect root nodes
        while !heap.mark(partition_id, 64) {}
        let freed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(freed, 0);

        // All nodes still accessible after GC
        for (i, node) in nodes.iter().enumerate() {
            assert_eq!(node.id, i as u64);
        }
    }
}
