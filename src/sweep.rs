// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{
    GcHeap, GcTraceRestrict,
    node::{GcHead, GcTriColor},
    node_iterator::NodeLinkIter,
    partition::GcPartitionId,
    trace::GcTraceCtx,
};

impl GcHeap {
    pub const SWEEP_UNMARKED_FUNC: fn(&GcHead) -> bool = |node| node.color() == GcTriColor::White;

    /// optionally call `on_dispose` before a node is disposed
    pub fn sweep(
        &mut self,
        partition_id: GcPartitionId,
        predicate: impl Fn(&GcHead) -> bool,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        if let Some(link0) = self
            .partitions
            .get_mut(&partition_id)
            .and_then(|p| p.nodes.take())
        {
            let call_on_dispose = !std::ptr::addr_eq(&on_dispose, &Self::DUMMY_DISPOSE_CALLBACK);

            let mut link1 = Some(link0);
            let mut freed_bytes = 0;

            let pass_slice = self.gc_types.drop_passes;
            for &pass in pass_slice {
                let mut current = link1;
                let mut prev: Option<NonNull<GcHead>> = None;

                while let Some(mut this) = current {
                    unsafe {
                        #[cfg(debug_assertions)]
                        this.as_ref().debug_assert_node_valid(self);

                        current = this.as_mut().next;

                        let dtype = this.as_ref().gc_type() as usize;
                        let info = &self.gc_types.type_info_list[dtype];
                        if predicate(this.as_mut()) && info.drop_pass == pass {
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
                                && let Some(p) = self.partitions.get_mut(&partition_id)
                                && let Some(i) = p.root_nodes.iter().position(|&x| x == this)
                            {
                                p.root_nodes.swap_remove(i);
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

            // update node link for partition
            if let Some(p) = self.partitions.get_mut(&partition_id) {
                p.nodes = link1;
            }
            // Decrease partitions memory usage
            self.update_mem_use(partition_id, -(freed_bytes as i32));

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
        if self.partition(partition_id).is_some() {
            let mut ctx = GcTraceCtx::new(self, GcTraceRestrict::No, true);
            ctx.trace_roots(partition_id, GcTraceCtx::MARK_FUNC);
            self.sweep(partition_id, Self::SWEEP_UNMARKED_FUNC, on_dispose)
        } else {
            0
        }
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

        let pass_slice = self.gc_types.drop_passes;
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

                    let dtype = this.as_ref().gc_type() as usize;
                    let info = &self.gc_types.type_info_list[dtype];
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

    use crate::{GcNode, trace::GcTracable};

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
        if let Some(partition) = heap.partitions.get(&partition_id) {
            if let Some(head) = partition.nodes {
                let mut current = Some(head);
                while let Some(node) = current {
                    unsafe {
                        nodes.push(node);
                        current = node.as_ref().next;
                    }
                }
            }
        }
        nodes
    }

    /// Test basic sweep_with functionality
    #[test]
    fn test_sweep_with_basic() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_root_partition(4096);

        let _objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| heap.alloc(partition_id, MyI32(i)).unwrap())
            .collect();

        // Verify we have 5 nodes
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 5);

        // Create a predicate that removes objects with even values
        let removed = heap.sweep(
            partition_id,
            |node| {
                unsafe {
                    let payload_ptr =
                        (node as *const GcHead as *const u8).add(std::mem::size_of::<GcHead>());
                    let value = (*(payload_ptr as *const MyI32)).0;
                    value % 2 == 0 // Remove even numbers
                }
            },
            |_, n| {
                println!("dispose node: {n:?}");
            },
        );

        // Should have removed 3 objects (0, 2, 4 are even)
        assert!(removed > 0, "Should have freed some bytes");

        // Verify we have 2 nodes left (1 and 3)
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 2);

        // Verify the remaining nodes have odd values
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
        let partition_id = heap.create_root_partition(4096);

        let _objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| heap.alloc(partition_id, MyI32(i)).unwrap())
            .collect();

        // Mark first 3 objects (0, 1, 2) for removal
        let removed = heap.sweep(
            partition_id,
            |node| {
                let payload_ptr = node.payload().cast::<MyI32>();
                let value = unsafe { (*payload_ptr.as_ptr()).0 };
                value < 3 // Remove values 0, 1, 2
            },
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );

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
        let partition_id = heap.create_root_partition(4096);

        let _objects: Vec<GcRef<MyI32>> = (0..3)
            .map(|i| heap.alloc(partition_id, MyI32(i)).unwrap())
            .collect();

        // Remove all nodes
        let removed = heap.sweep(
            partition_id,
            |_| true,
            |_, n| {
                println!("dispose {n:?}");
            },
        );

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
        let partition_id = heap.create_root_partition(4096);

        let _objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| heap.alloc(partition_id, MyI32(i)).unwrap())
            .collect();

        // Remove only middle node (value 2)
        let removed = heap.sweep(
            partition_id,
            |node| unsafe {
                let payload_ptr = node.payload().cast::<MyI32>();
                let value = (*payload_ptr.as_ptr()).0;
                value == 2
            },
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 4 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 4);

        // Verify chain is still properly linked
        let nodes = get_all_nodes_in_partition(&heap, partition_id);
        assert_eq!(nodes.len(), 4);

        let expected_values = vec![4, 3, 1, 0];
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
        let partition_id = heap.create_root_partition(4096);

        let root_obj = heap.alloc(partition_id, MyI32(0)).unwrap();
        let _objects: Vec<GcRef<MyI32>> = (1..3)
            .map(|i| heap.alloc(partition_id, MyI32(i)).unwrap())
            .collect();

        // Mark first object as root
        heap.set_root(root_obj, true);

        // Verify root list contains the object
        assert!(
            heap.partitions
                .get(&partition_id)
                .unwrap()
                .root_nodes
                .contains(&root_obj.head_ptr)
        );

        // Remove the root object
        let removed = heap.sweep(
            partition_id,
            |node| unsafe {
                let payload_ptr = node.payload().cast::<MyI32>();
                let value = (*payload_ptr.as_ptr()).0;
                value == 0
            },
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );

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
        let partition_id = heap.create_root_partition(4096);

        // No objects allocated, sweep should return 0
        let removed = heap.sweep(partition_id, |_| true, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(removed, 0, "Should return 0 for empty partition");
    }

    /// Test non-existent partition
    #[test]
    fn test_sweep_with_nonexistent_partition() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let non_existent_partition = GcPartitionId(9999);

        // Non-existent partition should return 0
        let removed = heap.sweep(
            non_existent_partition,
            |_| true,
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );
        assert_eq!(removed, 0, "Should return 0 for non-existent partition");
    }
}
