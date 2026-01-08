// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{
    GcHeap, allocator::Allocator, node::GcHead, partition::GcPartitionId, trace::GcTracer,
};

impl GcHeap {
    /// Collect garbage on given partition
    pub fn collect_garbage(&mut self, partition_id: GcPartitionId) -> usize {
        if self.partitions.partition(partition_id).is_some() {
            let mut tracer = GcTracer::new();
            self.mark_roots(partition_id, &mut tracer);
            self.sweep(partition_id)
        } else {
            0
        }
    }

    /// Automatic garbage collection (check all partitions)
    pub fn collect_garbage_auto(&mut self) -> usize {
        let mut total_freed = 0;

        // Get all partitions needing GC
        for partition_id in self.partitions.partitions_needing_gc() {
            let freed = self.collect_garbage(partition_id);
            total_freed += freed;
        }

        total_freed
    }

    /// Mark from partition roots
    pub fn mark_roots(&mut self, partition_id: GcPartitionId, tracer: &mut GcTracer) {
        if let Some(roots) = self.partition_roots.get(&partition_id) {
            for p in roots {
                unsafe {
                    debug_assert_eq!(p.as_ref().get_partition_id(), partition_id);

                    if !(*p.as_ptr()).is_marked() {
                        (*p.as_ptr()).set_marked(true);

                        let payload = (p.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
                        let trace_fn = (*p.as_ptr()).get_trace_fn(&self.type_registry);
                        trace_fn(payload, tracer);
                    }
                }
            }
        }

        while let Some(p) = tracer.next_header() {
            unsafe {
                if p.as_ref().get_partition_id() == partition_id {
                    let head = p.as_ptr();
                    if !(*head).is_marked() {
                        (*head).set_marked(true);

                        let payload = (head as *mut u8).add(std::mem::size_of::<GcHead>());
                        let trace_fn = (*head).get_trace_fn(&self.type_registry);
                        trace_fn(payload, tracer);
                    }
                } else {
                    #[cfg(debug_assertions)]
                    unreachable!(
                        "marking a gc object in difference paritition, expect {partition_id:?}, found {:?}",
                        p.as_ref().get_partition_id()
                    );
                }
            }
        }
    }

    /// Sweep unmarked objects in partition
    #[inline(always)]
    pub fn sweep(&mut self, partition_id: GcPartitionId) -> usize {
        self.sweep_ex(partition_id, false, None, None, false)
    }

    /// Sweep all objects in partition, regardless of marking
    #[inline(always)]
    pub fn sweep_purge(&mut self, partition_id: GcPartitionId) -> usize {
        self.sweep_ex(partition_id, true, None, None, false)
    }

    pub fn sweep_ex(
        &mut self,
        partition_id: GcPartitionId,
        force: bool,
        incl_types: Option<&[u16]>,
        excl_types: Option<&[u16]>,
        keep_mark: bool,
    ) -> usize {
        let head = match self.partition_heads.get_mut(&partition_id) {
            Some(head) => *head,
            None => {
                return 0;
            }
        };

        let mut current = head;
        let mut prev: Option<NonNull<GcHead>> = None;
        let mut freed_bytes = 0;

        while let Some(p) = current {
            unsafe {
                current = p.as_ref().next;
                let type_idx = p.as_ref().get_type_idx();

                let should_collect: bool = if !force && (*p.as_ptr()).is_marked() {
                    false
                } else {
                    #[cfg(debug_assertions)]
                    if !force {
                        let partition_id = (*p.as_ptr()).get_partition_id();
                        if let Some(roots) = self.partition_roots.get(&partition_id) {
                            debug_assert!(
                                !roots.contains(&p),
                                "Cannot free root object: partition_id={:?}, header={:?}, type={:?}",
                                partition_id,
                                p,
                                self.type_registry.with_idx(type_idx, |x| x.type_name)
                            );
                        }
                    }
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

                    // Release
                    freed_bytes += self.release_node(p);
                } else {
                    // Reset mark bits for next GC
                    if !keep_mark {
                        (*p.as_ptr()).set_marked(false);
                    }
                    prev = Some(p);
                }
            }
        }

        // Update partition memory usage
        if let Some(partition) = self.partitions.partition_mut(partition_id) {
            partition.dec_mem_use(freed_bytes);
        }

        freed_bytes
    }

    /// Release a node
    pub(super) unsafe fn release_node(&mut self, p: NonNull<GcHead>) -> usize {
        if let Some(weak_ref_index) = unsafe { (*p.as_ptr()).get_weak_ref_index() } {
            debug_assert!(weak_ref_index < self.weak_list.len());
            self.weak_list[weak_ref_index].1.take();
            unsafe {
                (*p.as_ptr()).set_weak_ref_index(None);
            }
        }

        let type_idx = unsafe { (*p.as_ptr()).get_type_idx() };
        debug_assert!(type_idx != 0);

        let (size, dispose_fn) = self
            .type_registry
            .with_idx(type_idx, |t| {
                (
                    t.size,
                    if t.needs_drop {
                        Some(t.dispose_fn)
                    } else {
                        None
                    },
                )
            })
            .unwrap();

        let gross_size = std::mem::size_of::<GcHead>() + size;

        // 调用dispose函数
        if let Some(f) = dispose_fn {
            let payload_ptr = unsafe { (p.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>()) };
            unsafe { f(payload_ptr) };
        }

        // 释放内存
        Allocator::deallocate(
            unsafe { NonNull::new_unchecked(p.as_ptr().cast::<u8>()) },
            gross_size,
        );

        gross_size
    }

    // /// Release object list
    // fn release(&mut self, free_list: impl Iterator<Item = NonNull<GcHead>>) -> usize {
    //     let mut size = 0;
    //     for node in free_list.into_iter() {
    //         size += unsafe { self.release_node(node) };
    //     }
    //     size
    // }

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

    //     // Analyze object reference relationships
    //     let mut tracer = GcTracer::new();
    //     free_list.iter().for_each(|&source| unsafe {
    //         // Call source object's trace function to trace all objects it references
    //         let payload_ptr = (source.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
    //         let trace_fn = (*source.as_ptr()).get_trace_fn(&self.type_registry);
    //         tracer.clear();
    //         trace_fn(payload_ptr, &mut tracer);

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

    //     // Calculate in-degrees
    //     graph.values().flat_map(|deps| deps.iter()).for_each(|&p| {
    //         *in_degree.entry(p).or_insert(0) += 1;
    //     });

    //     // Topological sort: using Kahn's algorithm
    //     let mut queue: VecDeque<NonNull<GcHead>> = in_degree
    //         .iter()
    //         .filter_map(|(&node, &degree)| if degree == 0 { Some(node) } else { None })
    //         .collect();

    //     let mut release_order: Vec<NonNull<GcHead>> = Vec::new();

    //     // Execute topological sort
    //     while let Some(node) = queue.pop_front() {
    //         release_order.push(node);

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

    //             let type_id = (*p.as_ptr()).get_type_idx();
    //             let size = self.type_registry.with_idx(type_id, |t| t.size).unwrap();
    //             let gross_size = std::mem::size_of::<GcHead>() + size;

    //             // Call dispose function - wrapped in unsafe block
    //             let dispose_fn = (*p.as_ptr()).get_dispose_fn(&self.type_registry);
    //             let payload_ptr = (p.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
    //             dispose_fn(payload_ptr);

    //             // Deallocate memory
    //             let ptr = p.as_ptr() as *mut u8;
    //             crate::allocator::Allocator::deallocate(NonNull::new_unchecked(ptr), gross_size);

    //             gross_size
    //         })
    //         .sum()
    // }
}
