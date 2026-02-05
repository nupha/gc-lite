// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{
    GcHeap, allocator::GcAllocator, node::GcHead, node_iterator::NodeIterator,
    partition::GcPartitionId, trace::GcTracer, weak::GcWeakId,
};

impl GcHeap {
    const NULL_NOTIFY_FN: Option<fn(&GcHead)> = None;

    pub const SWEEP_UNMARKED_FUNC: fn(&mut GcHead) -> bool = |node| !node.is_marked();

    /// call optional notify with node *BEFORE* it is disposed.
    fn sweep_internal(
        &mut self,
        partition_id: GcPartitionId,
        predicate: impl Fn(&mut GcHead) -> bool,
        notify: Option<impl Fn(&GcHead)>,
    ) -> usize {
        if let Some(first) = self.partition_nodes.get(&partition_id).copied() {
            let mut head = first;
            let mut freed_bytes = 0;

            for &pass in self.gc_type_drop_passes(&mut [0; 4]) {
                let mut current = head;
                let mut prev: Option<NonNull<GcHead>> = None;

                while let Some(mut p) = current {
                    unsafe {
                        current = p.as_ref().next;

                        if self.get_node_gc_type(p).drop_pass == pass && predicate(p.as_mut()) {
                            if let Some(last) = prev {
                                (*last.as_ptr()).next = current;
                            } else {
                                head = current;
                            }

                            // If root node: remove from root list
                            if p.as_ref().is_root()
                                && let Some(lst) = self.partition_roots.get_mut(&partition_id)
                            {
                                if let Some(i) = lst.iter().position(|x| *x == p) {
                                    lst.swap_remove(i);
                                }
                            }

                            if let Some(cb) = &notify {
                                cb(p.as_ref());
                            }
                            freed_bytes += self.dispose(p);
                        } else {
                            prev = Some(p);
                        }
                    }
                }
            }

            // let mut current = head;
            // let mut prev: Option<NonNull<GcHead>> = None;
            // while let Some(mut p) = current {
            //     unsafe {
            //         current = p.as_ref().next;

            //         if predicate(p.as_mut()) {
            //             if let Some(last) = prev {
            //                 (*last.as_ptr()).next = current;
            //             } else {
            //                 head = current; //chain head changed
            //             }

            //             // If root node: remove from root list
            //             if p.as_ref().is_root()
            //                 && let Some(lst) = self.partition_roots.get_mut(&partition_id)
            //             {
            //                 if let Some(i) = lst.iter().position(|x| *x == p) {
            //                     lst.swap_remove(i);
            //                 }
            //             }

            //             if let Some(cb) = &notify {
            //                 cb(p.as_ref());
            //             }
            //             freed_bytes += self.dispose(p);
            //         } else {
            //             prev = Some(p);
            //         }
            //     }
            // }

            if first != head {
                // update nodes head
                *self.partition_nodes.get_mut(&partition_id).unwrap() = head;
            }

            // Update partition memory usage with rollup to parent partitions
            self.mgr.update_mem_use(partition_id, -(freed_bytes as i32));

            freed_bytes
        } else {
            0
        }
    }

    /// call notify with node *BEFORE* it is disposed.
    #[inline(always)]
    pub fn sweep_notify(
        &mut self,
        partition_id: GcPartitionId,
        predicate: impl Fn(&mut GcHead) -> bool,
        notify: impl Fn(&GcHead),
    ) -> usize {
        self.sweep_internal(partition_id, predicate, Some(notify))
    }

    #[inline(always)]
    pub fn sweep(
        &mut self,
        partition_id: GcPartitionId,
        predicate: impl Fn(&mut GcHead) -> bool,
    ) -> usize {
        self.sweep_internal(partition_id, predicate, Self::NULL_NOTIFY_FN)
    }

    /// Collect garbage on given partition, optionally call notify with node *BEFORE* it is disposed.
    fn collect_internal(
        &mut self,
        partition_id: GcPartitionId,
        notify: Option<impl Fn(&GcHead)>,
    ) -> usize {
        if self.partition(partition_id).is_some() {
            self.tracer(partition_id).trace_roots(GcTracer::MARK_FUNC);
            self.sweep_internal(partition_id, Self::SWEEP_UNMARKED_FUNC, notify)
        } else {
            0
        }
    }

    /// Collect garbage on given partition, call notify with node *BEFORE* it is disposed.
    #[inline(always)]
    pub fn collect_notify(
        &mut self,
        partition_id: GcPartitionId,
        notify: impl Fn(&GcHead),
    ) -> usize {
        self.collect_internal(partition_id, Some(notify))
    }

    /// Collect garbage on given partition
    #[inline(always)]
    pub fn collect(&mut self, partition_id: GcPartitionId) -> usize {
        self.collect_internal(partition_id, Self::NULL_NOTIFY_FN)
    }

    #[deprecated(note = "use ::collect() instead")]
    pub fn collect_garbage(&mut self, partition_id: GcPartitionId) -> usize {
        self.collect(partition_id)
    }

    /// Dispose one node
    unsafe fn dispose(&mut self, node: NonNull<GcHead>) -> usize {
        let hd = unsafe { node.as_ref() };
        debug_assert!(hd.test_valid(), "[O.o] dispose valid node only");
        log::trace!("[dispose] {hd:?}");

        if !hd.weak_id.is_null() {
            // clear weak slot
            let widx = hd.weak_id.index();
            debug_assert!((widx as usize) < self.weak_slots.len());
            unsafe {
                self.weak_slots.get_unchecked_mut(widx as usize).1.take();
                (*node.as_ptr()).weak_id = GcWeakId::NULL;
            }
        }

        let ty = self.get_node_gc_type(node);
        let gross_size = std::mem::size_of::<GcHead>() + ty.size as usize;

        if let Some(f) = ty.drop_fn {
            unsafe {
                f(node.as_ref().payload().as_ptr());
            }
        }

        GcAllocator::deallocate(node.cast::<u8>(), gross_size);

        gross_size
    }

    /// Dispose all nodes along chain
    pub(crate) fn dispose_all_nodes(&mut self, start: NonNull<GcHead>) -> usize {
        let mut chain = Some(start);
        let mut freed_bytes = 0;

        for &pass in self.gc_type_drop_passes(&mut [0; 4]) {
            log::trace!(
                "[dipose_all] pass {pass}, count={}",
                NodeIterator::new(chain).count()
            );

            let mut current = chain;
            let mut prev: Option<NonNull<GcHead>> = None;

            while let Some(node) = current {
                unsafe {
                    current = node.as_ref().next;

                    if self.get_node_gc_type(node).drop_pass == pass {
                        if let Some(mut p) = prev {
                            p.as_mut().next = current;
                        } else {
                            chain = current;
                        }
                        freed_bytes += self.dispose(node);
                    } else {
                        prev = Some(node);
                    }
                }
            }
        }

        debug_assert!(chain.is_none());
        log::trace!("[dipose_all] done, freed {} bytes", freed_bytes);

        freed_bytes
    }
}

#[cfg(test)]
mod sweep_test {
    use super::*;
    use crate::GcRef;

    /// Helper function to count nodes in a partition
    fn count_nodes_in_partition(heap: &GcHeap, partition_id: GcPartitionId) -> usize {
        let mut count = 0;
        if let Some(head) = heap.partition_nodes.get(&partition_id).copied().flatten() {
            let mut current = Some(head);
            while let Some(node) = current {
                unsafe {
                    count += 1;
                    current = node.as_ref().next;
                }
            }
        }
        count
    }

    /// Helper function to get all node pointers in a partition
    fn get_all_nodes_in_partition(
        heap: &GcHeap,
        partition_id: GcPartitionId,
    ) -> Vec<NonNull<GcHead>> {
        let mut nodes = Vec::new();
        if let Some(head) = heap.partition_nodes.get(&partition_id).copied().flatten() {
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

    /// Test basic sweep_with functionality
    #[test]
    fn test_sweep_with_basic() {
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(4096);

        // Allocate 5 objects
        let objects: Vec<GcRef<i32>> = (0..5)
            .map(|i| heap.alloc(partition_id, i).unwrap())
            .collect();

        // Verify we have 5 nodes
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 5);

        // Create a predicate that removes objects with even values
        let removed = heap.sweep(partition_id, |node| {
            unsafe {
                let payload_ptr =
                    (node as *mut GcHead as *mut u8).add(std::mem::size_of::<GcHead>());
                let value = *(payload_ptr as *const i32);
                value % 2 == 0 // Remove even numbers
            }
        });

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
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(4096);

        // Allocate 5 objects with values 0-4
        let objects: Vec<GcRef<i32>> = (0..5)
            .map(|i| heap.alloc(partition_id, i).unwrap())
            .collect();

        // Mark first 3 objects (0, 1, 2) for removal
        let removed = heap.sweep(partition_id, |node| {
            unsafe {
                let payload_ptr =
                    (node as *mut GcHead as *mut u8).add(std::mem::size_of::<GcHead>());
                let value = *(payload_ptr as *const i32);
                value < 3 // Remove values 0, 1, 2
            }
        });

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 2 nodes left (3 and 4)
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 2);

        // Verify chain head is now the node with value 4
        let head = heap.partition_nodes.get(&partition_id).copied().flatten();
        assert!(head.is_some(), "Chain head should exist");

        unsafe {
            let payload_ptr =
                (head.unwrap().as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
            let value = *(payload_ptr as *const i32);
            assert_eq!(
                value, 4,
                "Chain head should be value 4 (last allocated, first in chain)"
            );
        }

        // Verify the chain is properly linked
        let nodes = get_all_nodes_in_partition(&heap, partition_id);
        assert_eq!(nodes.len(), 2);

        // Check values are 4 and 3 (in reverse allocation order)
        unsafe {
            let payload_ptr1 = (nodes[0].as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
            let value1 = *(payload_ptr1 as *const i32);
            assert_eq!(value1, 4);

            let payload_ptr2 = (nodes[1].as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
            let value2 = *(payload_ptr2 as *const i32);
            assert_eq!(value2, 3);
        }
    }

    /// Test removing all chain head nodes (连续剔除所有链头节点)
    #[test]
    fn test_sweep_with_all_chain_head_removal() {
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(4096);

        // Allocate 3 objects
        let objects: Vec<GcRef<i32>> = (0..3)
            .map(|i| heap.alloc(partition_id, i).unwrap())
            .collect();

        // Remove all nodes
        let removed = heap.sweep(partition_id, |_| true);

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 0 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 0);

        // Chain head should be None
        let head = heap.partition_nodes.get(&partition_id).copied().flatten();
        assert!(
            head.is_none(),
            "Chain head should be None after removing all nodes"
        );
    }

    /// Test removing middle nodes
    #[test]
    fn test_sweep_with_middle_node_removal() {
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(4096);

        // Allocate 5 objects
        let objects: Vec<GcRef<i32>> = (0..5)
            .map(|i| heap.alloc(partition_id, i).unwrap())
            .collect();

        // Remove only middle node (value 2)
        let removed = heap.sweep(partition_id, |node| unsafe {
            let payload_ptr = (node as *mut GcHead as *mut u8).add(std::mem::size_of::<GcHead>());
            let value = *(payload_ptr as *const i32);
            value == 2
        });

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 4 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 4);

        // Verify chain is still properly linked
        let nodes = get_all_nodes_in_partition(&heap, partition_id);
        assert_eq!(nodes.len(), 4);

        // Check values are 4, 3, 1, 0 (in reverse allocation order, skipping value 2)
        let expected_values = vec![4, 3, 1, 0];
        for (i, node) in nodes.iter().enumerate() {
            unsafe {
                let payload_ptr = (node.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
                let value = *(payload_ptr as *const i32);
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
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(4096);

        // Allocate 3 objects
        let objects: Vec<GcRef<i32>> = (0..3)
            .map(|i| heap.alloc(partition_id, i).unwrap())
            .collect();

        // Mark first object as root
        heap.set_root(objects[0], true);

        // Verify root list contains the object
        assert!(
            heap.partition_roots
                .get(&partition_id)
                .unwrap()
                .contains(&objects[0].head_ptr)
        );

        // Remove the root object
        let removed = heap.sweep(partition_id, |node| {
            unsafe {
                let payload_ptr =
                    (node as *mut GcHead as *mut u8).add(std::mem::size_of::<GcHead>());
                let value = *(payload_ptr as *const i32);
                value == 0 // Remove value 0 (the root)
            }
        });

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 2 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 2);

        // Root should be removed from root list
        assert!(
            !heap
                .partition_roots
                .get(&partition_id)
                .unwrap()
                .contains(&objects[0].head_ptr)
        );
    }

    /// Test empty partition
    #[test]
    fn test_sweep_with_empty_partition() {
        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(4096);

        // No objects allocated, sweep should return 0
        let removed = heap.sweep(partition_id, |_| true);
        assert_eq!(removed, 0, "Should return 0 for empty partition");
    }

    /// Test non-existent partition
    #[test]
    fn test_sweep_with_nonexistent_partition() {
        let mut heap = GcHeap::new();
        let non_existent_partition = GcPartitionId(9999);

        // Non-existent partition should return 0
        let removed = heap.sweep(non_existent_partition, |_| true);
        assert_eq!(removed, 0, "Should return 0 for non-existent partition");
    }
}
