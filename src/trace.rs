// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::VecDeque, marker::PhantomData, ptr::NonNull};

use crate::{GcHeap, GcNode, GcPartitionId, GcRef, node::GcHead};

pub trait GcTrace: 'static {
    /// Collect directly referenced children gc nodes
    fn trace(&self, gcx: &mut GcTraceCtx);

    /// Get direct referencing children nodes, regardless their color state.
    fn gc_children(&self, heap: &GcHeap) -> Vec<NonNull<GcHead>> {
        let mut gcx = heap.create_trace_ctx();
        self.trace(&mut gcx);
        gcx.traced_nodes.into()
    }
}

pub struct GcTraceCtx<'a> {
    pub(crate) traced_nodes: VecDeque<NonNull<GcHead>>,
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

        if !self.traced_nodes.contains(&node) {
            self.traced_nodes.push_back(node);
        }
    }

    /// Submit a GcRef to collected list
    #[inline(always)]
    pub fn add<T: GcNode>(&mut self, gc_ref: GcRef<T>) {
        self.add_node(gc_ref.head_ptr);
    }

    #[inline(always)]
    pub fn next_node(&mut self) -> Option<NonNull<GcHead>> {
        self.traced_nodes.pop_front()
    }
}

impl GcHeap {
    pub fn create_trace_ctx(&self) -> GcTraceCtx<'_> {
        GcTraceCtx {
            traced_nodes: VecDeque::new(),
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

        let mut gcx = self.create_trace_ctx();

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

                while let Some(child) = gcx.traced_nodes.pop_front() {
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
    }

    /// Helper function to count marked nodes in a partition
    fn count_non_white_nodes(heap: &GcHeap, partition_id: GcPartitionId) -> usize {
        let mut count = 0;
        if let Some(partition) = heap.partitions.get(&partition_id) {
            let mut current = partition.nodes;
            while let Some(node) = current {
                unsafe {
                    if node.as_ref().color() != GcTriColor::White {
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
        if let Some(partition) = heap.partitions.get(&partition_id) {
            let mut current = partition.nodes;
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
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

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

        // Mark reachable nodes using GC mark algorithm
        heap.set_root(root_ref, true);
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
        let partition_id = heap.create_partition(4096);

        // Create a simple tree: root -> child1, child2
        let child1 = heap.alloc(partition_id, TestNode::new(1)).unwrap();
        let child2 = heap.alloc(partition_id, TestNode::new(2)).unwrap();

        let mut root = TestNode::new(0);
        root.add_child(child1);
        root.add_child(child2);
        let root_ref = heap.alloc(partition_id, root).unwrap();

        // Mark reachable nodes incrementally with small step limit
        heap.set_root(root_ref, true);
        while !heap.mark(partition_id, 1) {}

        // Verify all nodes are marked
        assert_eq!(count_non_white_nodes(&heap, partition_id), 3);
    }

    /// Test 3: Deep nested tree with both algorithms
    #[test]
    fn test_trace_deep_nested_tree() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(8192);

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

        // Mark reachable nodes
        heap.set_root(level0_ref, true);
        while !heap.mark(partition_id, 4) {}
        assert_eq!(count_non_white_nodes(&heap, partition_id), 4);
    }

    /// Test 4: Complex tree with multiple branches
    #[test]
    fn test_trace_complex_tree() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(16384);

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

        // Mark reachable nodes
        heap.set_root(root_ref, true);
        while !heap.mark(partition_id, 8) {}
        assert_eq!(count_non_white_nodes(&heap, partition_id), 7);
    }

    /// Test 5: Verify both algorithms produce same result
    #[test]
    fn test_trace_algorithms_equivalence() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(8192);

        // Create a tree with 10 nodes in a balanced structure
        let mut nodes = Vec::new();
        for i in 0..10 {
            nodes.push(heap.alloc(partition_id, TestNode::new(i as u32)).unwrap());
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

        // Mark with larger step limit
        heap.set_root(nodes[0], true);
        while !heap.mark(partition_id, 16) {}
        let marks1 = count_non_white_nodes(&heap, partition_id);

        // Reset colors and mark again with smaller step limit
        heap.mark_reset(partition_id);
        while !heap.mark(partition_id, 1) {}
        let marks2 = count_non_white_nodes(&heap, partition_id);

        // Both algorithms should mark the same number of nodes
        assert_eq!(marks1, marks2);
        assert_eq!(marks1, 10);
    }

    /// Test 6: Circular reference handling
    #[test]
    fn test_trace_circular_reference() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        // Create two nodes that reference each other
        let mut node1 = heap.alloc(partition_id, TestNode::new(1)).unwrap();
        let mut node2 = heap.alloc(partition_id, TestNode::new(2)).unwrap();

        {
            node1.with_mut(&mut heap, |n| n.add_child(node2));
            node2.with_mut(&mut heap, |n| n.add_child(node1));
        }

        // Mark reachable nodes - should handle circular reference without infinite loop
        heap.set_root(node1, true);
        while !heap.mark(partition_id, 4) {}

        // Both nodes should be marked
        assert_eq!(
            count_non_white_nodes(&heap, partition_id),
            2,
            "Propagate should handle circular reference"
        );

        // Reset and mark again to ensure stability
        heap.mark_reset(partition_id);
        while !heap.mark(partition_id, 1) {}
        assert_eq!(count_non_white_nodes(&heap, partition_id), 2);
    }
}
