// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{
    GcHeap, allocator::GcAllocator, node::GcHead, partition::GcPartitionId, trace::GcTracer,
};

impl GcHeap {
    pub const SWEEP_UNMARKED_FUNC: fn(&mut GcHead) -> bool = |node| {
        if node.is_marked() {
            node.set_marked(false);
            false
        } else {
            true
        }
    };

    /// Collect garbage on given partition
    pub fn collect_garbage(&mut self, partition_id: GcPartitionId) -> usize {
        if self.partitions.partition(partition_id).is_some() {
            let mut tr = GcTracer::with_capacity(self, partition_id, 64);
            tr.clear_visit_flags();
            tr.clear_marks();
            tr.trace_roots(GcTracer::MARK_FUNC);

            self.sweep_with(partition_id, Self::SWEEP_UNMARKED_FUNC)
        } else {
            0
        }
    }

    /// Mark from partition roots
    #[deprecated(note = "use GcTracer::trace_roots() instead")]
    pub fn mark_roots(&mut self, partition_id: GcPartitionId, tracer: &mut GcTracer) {
        if let Some(roots) = self.partition_roots.get(&partition_id) {
            for ptr in roots {
                unsafe {
                    let head = ptr.as_ref();
                    debug_assert_eq!(head.get_partition_id(), partition_id);

                    if !head.is_marked() {
                        (*ptr.as_ptr()).set_marked(true);

                        let trace_fn = head.get_trace_fn(&self.type_registry);
                        let payload = ptr.cast::<u8>().add(std::mem::size_of::<GcHead>());
                        trace_fn(payload.as_ptr(), tracer);
                    }
                }
            }
        }

        while let Some(p) = tracer.pendings.pop_front() {
            unsafe {
                let head = p.as_ptr();
                if (*head).get_partition_id() == partition_id && !(*head).is_marked() {
                    (*head).set_marked(true);

                    let payload = (head as *mut u8).add(std::mem::size_of::<GcHead>());
                    let trace_fn = (*head).get_trace_fn(&self.type_registry);
                    trace_fn(payload, tracer);
                }
            }
        }
    }

    pub fn sweep_with(
        &mut self,
        partition_id: GcPartitionId,
        predicate: impl Fn(&mut GcHead) -> bool,
    ) -> usize {
        if let Some(chain) = self.partition_heads.get(&partition_id).copied() {
            let mut new_chain = chain;
            let mut current = chain;
            let mut prev: Option<NonNull<GcHead>> = None;
            let mut freed_bytes = 0;

            while let Some(mut p) = current {
                unsafe {
                    current = p.as_ref().next;

                    if predicate(p.as_mut()) {
                        if let Some(last) = prev {
                            (*last.as_ptr()).next = current;
                        } else {
                            new_chain = current; //chain head changed
                        }

                        // If root node: remove from root list
                        if p.as_ref().is_root()
                            && let Some(lst) = self.partition_roots.get_mut(&partition_id)
                        {
                            if let Some(i) = lst.iter().position(|x| *x == p) {
                                lst.swap_remove(i);
                            }
                        }

                        freed_bytes += self.dispose(p);
                    } else {
                        prev = Some(p);
                    }
                }
            }

            if chain != new_chain {
                // update chain head
                *self.partition_heads.get_mut(&partition_id).unwrap() = new_chain;
            }

            // Update partition memory usage with rollup to parent partitions
            self.partitions
                .update_mem_use(partition_id, -(freed_bytes as i32));

            freed_bytes
        } else {
            0
        }
    }

    /// Sweep unmarked node in partition
    #[deprecated(note = "use ::sweep_with() instead")]
    #[inline(always)]
    pub fn sweep(&mut self, partition_id: GcPartitionId) -> usize {
        self.sweep_with(partition_id, Self::SWEEP_UNMARKED_FUNC)
    }

    #[deprecated(note = "use ::sweep_with() instead")]
    pub fn sweep_ex(
        &mut self,
        partition_id: GcPartitionId,
        force: bool,
        incl_types: Option<&[u8]>,
        excl_types: Option<&[u8]>,
        keep_mark: bool,
    ) -> usize {
        let chain = match self.partition_heads.get_mut(&partition_id) {
            Some(p) => *p,
            None => {
                return 0;
            }
        };

        let mut current = chain;
        let mut prev: Option<NonNull<GcHead>> = None;
        let mut freed_bytes = 0;

        while let Some(p) = current {
            unsafe {
                current = p.as_ref().next;
                let type_idx = p.as_ref().gc_type_id();

                let should_collect: bool = if !force && (*p.as_ptr()).is_marked() {
                    false
                } else {
                    incl_types.is_none_or(|t| t.contains(&type_idx))
                        && excl_types.is_none_or(|t| !t.contains(&type_idx))
                };

                if should_collect {
                    // Remove unmarked objects from list
                    if let Some(last) = prev {
                        (*last.as_ptr()).next = current;
                    } else {
                        *self.partition_heads.get_mut(&partition_id).unwrap() = current;
                    }

                    if p.as_ref().is_root() {
                        // Remove from root list
                        if let Some(lst) = self.partition_roots.get_mut(&partition_id) {
                            if let Some(i) = lst.iter().position(|x| *x == p) {
                                lst.swap_remove(i);
                            }
                        }
                    }

                    freed_bytes += self.dispose(p);
                } else {
                    // Reset mark bits for next GC
                    if !keep_mark {
                        (*p.as_ptr()).set_marked(false);
                    }
                    prev = Some(p);
                }
            }
        }

        // Update partition memory usage with rollup to parent partitions
        self.partitions
            .update_mem_use(partition_id, -(freed_bytes as i32));

        freed_bytes
    }

    /// Dispose a node
    pub(super) unsafe fn dispose(&mut self, node: NonNull<GcHead>) -> usize {
        let type_idx = unsafe { (*node.as_ptr()).gc_type_id() };
        debug_assert_ne!(type_idx, 0);

        if let Some(w) = unsafe { (*node.as_ptr()).weakref_index() } {
            // clear weak slot node pointer - mark the weak slot is free.
            debug_assert!((w as usize) < self.weak_slots.len());
            unsafe {
                self.weak_slots.get_unchecked_mut(w as usize).1.take();
                (*node.as_ptr()).set_weakref_index(None);
            }
        }

        #[cfg(debug_assertions)]
        unsafe {
            // clear MAGIC_NUM flag: mark this node invalid.
            let mut f = (*node.as_ptr()).flags();
            f.remove(crate::node::GcHeadFlag::MAGIC_NUM);
            (*node.as_ptr()).set_flags(f);
        }

        let (size, dispose_fn) = self
            .type_registry
            .with_type_id(type_idx, |t| (t.size as usize, t.dispose_fn))
            .unwrap();

        let gross_size = std::mem::size_of::<GcHead>() + size;

        if let Some(f) = dispose_fn {
            unsafe {
                f(node.as_ref().payload().as_ptr());
            }
        }

        GcAllocator::deallocate(
            unsafe { NonNull::new_unchecked(node.as_ptr().cast::<u8>()) },
            gross_size,
        );

        gross_size
    }

    // /// Analyze object dependencies and release in topological order
    // fn release_with_dependency_analysis(
    //     &mut self,
    //     free_list: Vec<NonNull<GcHead>>,
    //     _partition_id: GcPartitionId,
    // ) -> usize {
    //     // Initialize dependency graph and in-degree table
    //     let mut graph: HashMap<NonNull<GcHead>, HashSet<NonNull<GcHead>>> = free_list
    //         .iter()
    //         .map(|&header| (header, HashSet::new()))
    //         .collect();
    //     let mut in_degree: HashMap<NonNull<GcHead>, usize> =
    //         free_list.iter().map(|&header| (header, 0)).collect();
    //
    //     // Analyze object reference relationships
    //     let mut tracer = GcTracer::new();
    //     free_list.iter().for_each(|&source| unsafe {
    //         // Call source object's trace function to trace all objects it references
    //         let payload_ptr = (source.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
    //         let trace_fn = (*source.as_ptr()).get_trace_fn(&self.type_registry);
    //         tracer.clear();
    //         trace_fn(payload_ptr, &mut tracer);
    //
    //         // Process trace results
    //         while let Some(target) = tracer.next_header() {
    //             if free_list.contains(&target) {
    //                 graph
    //                     .entry(source)
    //                     .or_insert_with(HashSet::new)
    //                     .insert(target);
    //             }
    //         }
    //     });
    //
    //     // Calculate in-degrees
    //     graph.values().flat_map(|deps| deps.iter()).for_each(|&p| {
    //         *in_degree.entry(p).or_insert(0) += 1;
    //     });
    //
    //     // Topological sort: using Kahn's algorithm
    //     let mut queue: VecDeque<NonNull<GcHead>> = in_degree
    //         .iter()
    //         .filter_map(|(&node, &degree)| if degree == 0 { Some(node) } else { None })
    //         .collect();
    //
    //     let mut release_order: Vec<NonNull<GcHead>> = Vec::new();
    //
    //     // Execute topological sort
    //     while let Some(node) = queue.pop_front() {
    //         release_order.push(node);
    //
    //         if let Some(dependencies) = graph.get(&node) {
    //             dependencies.iter().for_each(|&neighbor| {
    //                 if let Some(degree) = in_degree.get_mut(&neighbor) {
    //                     *degree -= 1;
    //                     if *degree == 0 {
    //                         queue.push_back(neighbor);
    //                     }
    //                 }
    //             });
    //         }
    //     }
    //
    //     // Handle circular references: remaining nodes released in arbitrary order
    //     let remainings: Vec<NonNull<GcHead>> = in_degree
    //         .iter()
    //         .filter_map(|(&node, &degree)| {
    //             if degree > 0 && !release_order.contains(&node) {
    //                 Some(node)
    //             } else {
    //                 None
    //             }
    //         })
    //         .collect();
    //     release_order.extend(remainings);
    //
    //     // Release objects in topological order
    //     release_order
    //         .into_iter()
    //         .map(|p| unsafe {
    //             // Handle weak references - add boundary checks to prevent out-of-bounds crash
    //             if let Some(weak_ref_index) = (*p.as_ptr()).get_weak_ref_index() {
    //                 // Boundary check: ensure index is within valid range
    //                 if weak_ref_index < self.weakrefs_list.len() {
    //                     self.weakrefs_list[weak_ref_index].1.take();
    //                 }
    //                 // Reset weak reference index to prevent dangling references
    //                 (*p.as_ptr()).set_weak_ref_index(None);
    //             }
    //
    //             let type_id = (*p.as_ptr()).get_type_idx();
    //             let size = self.type_registry.with_idx(type_id, |t| t.size).unwrap();
    //             let gross_size = std::mem::size_of::<GcHead>() + size;
    //
    //             // Call dispose function - wrapped in unsafe block
    //             let dispose_fn = (*p.as_ptr()).get_dispose_fn(&self.type_registry);
    //             let payload_ptr = (p.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
    //             dispose_fn(payload_ptr);
    //
    //             // Deallocate memory
    //             let ptr = p.as_ptr() as *mut u8;
    //             crate::allocator::Allocator::deallocate(NonNull::new_unchecked(ptr), gross_size);
    //
    //             gross_size
    //         })
    //         .sum()
    // }
}

#[cfg(test)]
mod sweep_test {
    use super::*;
    use crate::GcRef;

    /// Helper function to count nodes in a partition
    fn count_nodes_in_partition(heap: &GcHeap, partition_id: GcPartitionId) -> usize {
        let mut count = 0;
        if let Some(head) = heap.partition_heads.get(&partition_id).copied().flatten() {
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
        if let Some(head) = heap.partition_heads.get(&partition_id).copied().flatten() {
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
        let removed = heap.sweep_with(partition_id, |node| {
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
        let removed = heap.sweep_with(partition_id, |node| {
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
        let head = heap.partition_heads.get(&partition_id).copied().flatten();
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
        let removed = heap.sweep_with(partition_id, |_| true);

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 0 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 0);

        // Chain head should be None
        let head = heap.partition_heads.get(&partition_id).copied().flatten();
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
        let removed = heap.sweep_with(partition_id, |node| unsafe {
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
        let removed = heap.sweep_with(partition_id, |node| {
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
        let removed = heap.sweep_with(partition_id, |_| true);
        assert_eq!(removed, 0, "Should return 0 for empty partition");
    }

    /// Test non-existent partition
    #[test]
    fn test_sweep_with_nonexistent_partition() {
        let mut heap = GcHeap::new();
        let non_existent_partition = GcPartitionId(9999);

        // Non-existent partition should return 0
        let removed = heap.sweep_with(non_existent_partition, |_| true);
        assert_eq!(removed, 0, "Should return 0 for non-existent partition");
    }
}
