// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{
    GcHeap,
    node::{GcHead, GcTriColor},
    node_iterator::NodeLinkIter,
    partition::GcPartitionId,
};

impl GcHeap {
    pub fn add_gray_node(&mut self, node: NonNull<GcHead>) {
        if unsafe { node.as_ref().color() } != GcTriColor::Black {
            self.partition_mut(unsafe { node.as_ref().scope_id() })
                .unwrap()
                .add_gray_node(node);
        }
    }

    pub fn mark_reset(&mut self, partition_id: GcPartitionId) {
        if let Some(par) = self.partition_mut(partition_id) {
            par.set_marking(false);
            par.gray_list.clear();
            for mut n in par.nodes() {
                unsafe {
                    n.as_mut().set_color(GcTriColor::White);
                }
            }
        }
    }

    pub fn mark(&mut self, partition_id: GcPartitionId, max_steps: usize) -> bool {
        let heap_ptr = self as *mut Self;

        if let Some(par) = self.partitions.get_mut(&partition_id) {
            if !par.is_marking() {
                debug_assert!(par.gray_list.is_empty());

                for mut n in par.nodes() {
                    unsafe {
                        n.as_mut().set_color(GcTriColor::White);
                    }
                }
                par.set_marking(true);

                for n in par.root_nodes.iter() {
                    let mut root = *n;
                    unsafe {
                        root.as_mut().set_color(GcTriColor::Gray);
                    }
                    par.gray_list.push(root);
                }
            }

            if max_steps > 0 {
                let mut gcx = unsafe { (*heap_ptr).create_trace_ctx() };
                let mut cnt = 0;

                while let Some(mut node_ptr) = par.gray_list.pop() {
                    let node = unsafe { node_ptr.as_mut() };
                    debug_assert_eq!(node.scope_id(), partition_id);

                    if node.color() == GcTriColor::Gray {
                        if cnt >= max_steps {
                            par.gray_list.push(node_ptr);
                            return false;
                        }

                        // Trace for children.
                        (self.node_dtypes.type_info_list[node.dtype() as usize].trace_fn)(
                            node_ptr, &mut gcx,
                        );

                        while let Some(mut ch) = gcx.traced_nodes.pop_front() {
                            let child = unsafe { ch.as_mut() };

                            #[cfg(debug_assertions)]
                            child.debug_assert_node_valid_simple();

                            let scope = child.scope_id();
                            if scope == partition_id {
                                if matches!(child.color(), GcTriColor::White | GcTriColor::Gray) {
                                    child.set_color(GcTriColor::Gray);
                                    par.gray_list.push(ch);
                                }
                            } else {
                                let p2 = unsafe { (*heap_ptr).partition(scope).unwrap() };
                                if p2.is_marking() {
                                    unsafe {
                                        (*heap_ptr).add_gray_node(ch);
                                    }
                                }
                            }
                        }

                        // Mark current node as black.
                        node.set_color(GcTriColor::Black);

                        cnt += 1;
                    }
                }
            }
        }

        true
    }

    /// dispose white nodes in the partition
    pub fn sweep(
        &mut self,
        partition_id: GcPartitionId,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        if let Some(link0) = self.partition_mut(partition_id).and_then(|p| {
            if p.is_marking() && p.gray_list.is_empty() {
                p.set_marking(false);
                p.nodes.take()
            } else {
                None // mark cycle not done
            }
        }) {
            #[cfg(debug_assertions)]
            for n in NodeLinkIter::new(Some(link0)) {
                unsafe {
                    debug_assert!(
                        matches!(n.as_ref().color(), GcTriColor::Black | GcTriColor::White),
                        "sweep node must be either black or white: {:?}",
                        n.as_ref()
                    );
                }
            }

            let call_on_dispose = !std::ptr::addr_eq(&on_dispose, &Self::DUMMY_DISPOSE_CALLBACK);
            let mut link1 = Some(link0);
            let mut freed_bytes = 0;

            for &pass in self.node_dtypes.drop_passes {
                let mut current = link1;
                let mut prev: Option<NonNull<GcHead>> = None;

                while let Some(mut this) = current {
                    unsafe {
                        #[cfg(debug_assertions)]
                        this.as_ref().debug_assert_node_valid(self);

                        current = this.as_mut().next;

                        let drop_pass = self.node_dtypes.type_info_list
                            [this.as_ref().dtype() as usize]
                            .drop_pass;
                        let is_white = this.as_ref().color() == GcTriColor::White;
                        let is_protected = this.as_ref().is_protected();

                        if drop_pass == pass && is_white && !is_protected {
                            if let Some(mut p) = prev {
                                p.as_mut().next = current;
                            } else {
                                link1 = current;
                            }

                            let is_root = this.as_ref().is_root();
                            if call_on_dispose {
                                on_dispose(self, this.as_ref());
                            }

                            freed_bytes += self.dispose(this);

                            // If root node: remove from root list
                            if is_root
                                && let Some(par) = self.partition_mut(partition_id)
                                && let Some(i) = par.root_nodes.iter().position(|&x| x == this)
                            {
                                par.root_nodes.swap_remove(i);
                            }
                        } else {
                            prev = Some(this);
                        }
                    }
                }

                if link1.is_none() {
                    break;
                }
            }

            debug_assert!(
                self.partition_mut(partition_id)
                    .unwrap()
                    .gray_list
                    .is_empty()
            );

            // update remainder node link of partition
            if link1.is_some() {
                #[cfg(debug_assertions)]
                for n in NodeLinkIter::new(link1) {
                    unsafe {
                        debug_assert!(
                            n.as_ref().color() == GcTriColor::Black,
                            "live nodes should be black only"
                        );
                    }
                }

                let p = self.partition_mut(partition_id).unwrap();
                p.nodes = link1;
            }

            // Decrease partitions memory usage
            if freed_bytes != 0 {
                self.update_mem_use(partition_id, -(freed_bytes as i32));
            }

            freed_bytes
        } else {
            0
        }
    }

    /// Collect garbage on given partition, call notify with node *BEFORE* it is disposed.
    #[inline]
    pub fn garbage_collect(
        &mut self,
        partition_id: GcPartitionId,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        if self.partition(partition_id).is_none() {
            return 0;
        }

        // Mark phase: incrementally process gray list until all reachable nodes are marked
        while !self.mark(partition_id, 64) {}

        // Sweep phase: reclaim unmarked (white) nodes
        self.sweep(partition_id, on_dispose)
    }

    /// Dispose all nodes along chain
    pub(crate) fn dispose_all_nodes(
        &mut self,
        head: NonNull<GcHead>,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        let call_on_dispose = !std::ptr::addr_eq(&on_dispose, &Self::DUMMY_DISPOSE_CALLBACK);
        let mut link = Some(head);
        let mut freed_bytes = 0;

        let pass_slice = self.node_dtypes.drop_passes;
        for &pass in pass_slice {
            log::trace!(
                "[dipose_all] pass {pass}, count={}",
                NodeLinkIter::new(link).count()
            );

            let mut current = link;
            let mut prev: Option<NonNull<GcHead>> = None;

            while let Some(this) = current {
                unsafe {
                    #[cfg(debug_assertions)]
                    this.as_ref().debug_assert_node_valid(self);

                    current = this.as_ref().next;

                    let dtype = this.as_ref().dtype() as usize;
                    let info = &self.node_dtypes.type_info_list[dtype];
                    if info.drop_pass == pass {
                        if let Some(mut p) = prev {
                            p.as_mut().next = current;
                        } else {
                            link = current;
                        }

                        if call_on_dispose {
                            on_dispose(self, this.as_ref());
                        }

                        freed_bytes += self.dispose(this);
                    } else {
                        prev = Some(this);
                    }
                }
            }

            if link.is_none() {
                break;
            }
        }

        debug_assert!(link.is_none());
        log::trace!("[dipose_all] done, freed {} bytes", freed_bytes);

        freed_bytes
    }
}

#[cfg(test)]
mod sweep_test {
    use super::*;
    use crate::GcRef;

    use crate::trace::{GcTracable, GcTraceCtx};

    #[derive(Debug)]
    struct MyI32(i32);

    unsafe impl GcTracable for MyI32 {
        fn trace(&self, _: &mut GcTraceCtx) {}
    }

    crate::gc_type_register! {
        MyI32, drop_pass = 0;
    }

    /// Helper function to count nodes in a partition
    fn count_nodes_in_partition(heap: &GcHeap, partition_id: GcPartitionId) -> usize {
        let mut count = 0;
        if let Some(head) = heap.partitions.get(&partition_id).unwrap().nodes {
            count = NodeLinkIter::new(Some(head)).count();
        }
        count
    }

    /// Helper function to get all node pointers in a partition
    fn get_all_nodes_in_partition(
        heap: &GcHeap,
        partition_id: GcPartitionId,
    ) -> Vec<NonNull<GcHead>> {
        let mut nodes: Vec<NonNull<GcHead>> = Vec::new();
        if let Some(partition) = heap.partitions.get(&partition_id)
            && let Some(head) = partition.nodes
        {
            let mut current = Some(head);
            while let Some(node) = current {
                unsafe {
                    nodes.push(node);
                    current = node.as_ref().next;
                }
            }
        }
        nodes
    }

    /// Test basic sweep functionality
    #[test]
    fn test_sweep_with_basic() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| heap.alloc(partition_id, MyI32(i)).unwrap())
            .collect();

        for (i, obj) in objects.iter().enumerate() {
            if i % 2 == 1 {
                heap.set_root(*obj, true);
            }
        }

        assert_eq!(count_nodes_in_partition(&heap, partition_id), 5);

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, |_, _| {});
        assert!(removed > 0, "Should have freed some bytes");

        assert_eq!(count_nodes_in_partition(&heap, partition_id), 2);

        let remaining_nodes = get_all_nodes_in_partition(&heap, partition_id);
        for node in remaining_nodes {
            unsafe {
                let payload_ptr = (node.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
                let value = *(payload_ptr as *const i32);
                assert_eq!(value % 2, 1, "Remaining nodes should have odd values");
            }
        }
    }

    /// Test removing chain head nodes (n个节点被剔除后)
    #[test]
    fn test_sweep_with_chain_head_removal() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| heap.alloc(partition_id, MyI32(i)).unwrap())
            .collect();

        heap.set_root(objects[3], true);
        heap.set_root(objects[4], true);

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 2 nodes left (3 and 4)
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 2);

        // Verify chain head is now the node with value 4
        let head = heap.partitions.get(&partition_id).and_then(|p| p.nodes);
        assert!(head.is_some(), "Chain head should exist");

        unsafe {
            let payload_ptr =
                (head.unwrap().as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
            let value = (*(payload_ptr as *const MyI32)).0;
            assert_eq!(
                value, 4,
                "Chain head should be value 4 (last allocated, first in chain)"
            );
        }

        // Verify the chain is properly linked
        let nodes = get_all_nodes_in_partition(&heap, partition_id);
        assert_eq!(nodes.len(), 2);

        unsafe {
            let payload_ptr1 = nodes[0].as_ref().payload().cast::<MyI32>();
            let value1 = (*payload_ptr1.as_ptr()).0;
            assert_eq!(value1, 4);

            let payload_ptr2 = nodes[1].as_ref().payload().cast::<MyI32>();
            let value2 = (*payload_ptr2.as_ptr()).0;
            assert_eq!(value2, 3);
        }
    }

    /// Test removing all chain head nodes (连续剔除所有链头节点)
    #[test]
    fn test_sweep_with_all_chain_head_removal() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let _objects: Vec<GcRef<MyI32>> = (0..3)
            .map(|i| heap.alloc(partition_id, MyI32(i)).unwrap())
            .collect();

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, |_, n| {
            println!("dispose {n:?}");
        });

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 0 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 0);

        // Chain head should be None
        let head = heap.partitions.get(&partition_id).and_then(|p| p.nodes);
        assert!(
            head.is_none(),
            "Chain head should be None after removing all nodes"
        );
    }

    /// Test removing middle nodes
    #[test]
    fn test_sweep_with_middle_node_removal() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| heap.alloc(partition_id, MyI32(i)).unwrap())
            .collect();

        for (i, obj) in objects.iter().enumerate() {
            if i != 2 {
                heap.set_root(*obj, true);
            }
        }

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 4 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 4);

        // Verify chain is still properly linked
        let nodes = get_all_nodes_in_partition(&heap, partition_id);
        assert_eq!(nodes.len(), 4);

        let expected_values = [4, 3, 1, 0];
        for (i, node) in nodes.iter().enumerate() {
            unsafe {
                let payload_ptr = (node.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
                let value = (*(payload_ptr as *const MyI32)).0;
                assert_eq!(
                    value, expected_values[i],
                    "Node at position {} should have value {}",
                    i, expected_values[i]
                );
            }
        }
    }

    /// Test removing root nodes
    #[test]
    fn test_sweep_with_root_node_removal() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let root_obj = heap.alloc(partition_id, MyI32(0)).unwrap();
        let _objects: Vec<GcRef<MyI32>> = (1..3)
            .map(|i| heap.alloc(partition_id, MyI32(i)).unwrap())
            .collect();

        heap.set_root(root_obj, true);

        assert!(
            heap.partitions
                .get(&partition_id)
                .unwrap()
                .root_nodes
                .contains(&root_obj.head_ptr)
        );

        heap.set_root(root_obj, false);
        for obj in &_objects {
            heap.set_root(*obj, true);
        }

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 2 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 2);

        // Root should be removed from root list
        assert!(
            !heap
                .partitions
                .get(&partition_id)
                .unwrap()
                .root_nodes
                .contains(&root_obj.head_ptr)
        );
    }

    /// Test empty partition
    #[test]
    fn test_sweep_with_empty_partition() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(removed, 0, "Should return 0 for empty partition");
    }

    /// Test non-existent partition
    #[test]
    fn test_sweep_with_nonexistent_partition() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let non_existent_partition = GcPartitionId(9999);

        let removed = heap.sweep(non_existent_partition, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(removed, 0, "Should return 0 for non-existent partition");
    }
}
