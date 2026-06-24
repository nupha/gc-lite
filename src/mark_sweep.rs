// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{
    GcHeap,
    node::{GcHead, GcNode, GcRef, GcTriColor},
    node_link::{GcNodeLink, NodeLinkIter},
    partition::GcPartitionId,
    trace::GcTraceCtx,
};

#[cfg(feature = "gc_arena")]
use crate::node::GcNodeFlag;

impl GcHeap {
    pub fn add_gray_node(&mut self, node: NonNull<GcHead>) {
        if unsafe { node.as_ref().color() } != GcTriColor::Black {
            let pid = unsafe { node.as_ref().partition_id() };
            self.partitions[pid.0 as usize].add_gray_node(node);
        }
    }

    /// Check whether the partition that `node` belongs to is currently marking.
    #[inline]
    pub(crate) unsafe fn is_node_partition_marking<T: GcNode>(&self, node: GcRef<T>) -> bool {
        let pid = unsafe { node.head_ptr.as_ref().partition_id() };
        self.partitions[pid.0 as usize].is_marking()
    }

    pub fn mark_reset(&mut self, partition_id: GcPartitionId) {
        let par = &mut self.partitions[partition_id.0 as usize];
        par.set_marking(false);

        // Drain gray list and clear flags so stale GRAY_LISTED bits
        // don't affect the next marking cycle.
        for mut n in par.gray_list.drain(..) {
            unsafe {
                n.as_mut().set_gray_listed(false);
            }
        }

        for n in par.nodes_mut() {
            n.set_color(GcTriColor::White);
        }
    }

    pub fn mark_prepare(&mut self, partition_id: GcPartitionId) {
        if (partition_id.0 as usize) >= self.partitions.len() {
            return;
        }

        // If the target partition is already marking, no-op (its gray_list
        // is being processed by mark_grays).
        if self.partitions[partition_id.0 as usize].is_marking() {
            return;
        }

        // Set ALL partitions to marking mode so that cross-partition
        // references can be pushed into any partition's gray_list.
        for par in &mut self.partitions {
            par.set_marking(true);
        }

        // Seed roots from ALL partitions.
        for i in 0..self.partitions.len() {
            let has_pending_grays = !self.partitions[i].gray_list.is_empty();
            let par = &mut self.partitions[i];

            for mut n in par.nodes.iter() {
                let node = unsafe { n.as_mut() };
                if node.is_root_or_local() {
                    node.set_color(GcTriColor::Gray);
                    if !has_pending_grays {
                        par.gray_list.push(n);
                    }
                } else if !has_pending_grays {
                    node.set_color(GcTriColor::White);
                }
            }

            if has_pending_grays {
                // Pending gray nodes already exist; only add root/LOCAL
                // nodes that are not already in the gray list.
                for mut n in par.nodes.iter() {
                    let node = unsafe { n.as_mut() };
                    if node.is_root_or_local() {
                        node.set_color(GcTriColor::Gray);
                        if !node.is_gray_listed() {
                            node.set_gray_listed(true);
                            par.gray_list.push(n);
                        }
                    }
                }
            }
        }
    }

    pub fn mark_grays(&mut self, partition_id: GcPartitionId, max_steps: usize) -> bool {
        if max_steps == 0 {
            return false;
        }

        // Pre-acquire the opaque pointer and type registry so we don't need &self
        // while holding a mutable borrow on the partition.
        let opaque = self.opaque();
        let node_dtypes: *const crate::gctype::GcTypeRegistry = self.node_dtypes;

        if !self.partitions[partition_id.0 as usize]
            .gray_list
            .is_empty()
        {
            let mut gcx = GcTraceCtx {
                traced_nodes: Vec::with_capacity(64),
                opaque,
                _mark: std::marker::PhantomData,
            };

            // Buffer for children that need to be pushed to a different
            // partition's gray_list (avoids borrow-checker conflicts when
            // indexing self.partitions at different indices).
            let mut cross_buffer: Vec<(u16, NonNull<GcHead>)> = Vec::with_capacity(8);
            let mut cnt = 0;

            while let Some(mut node_ptr) = self.partitions[partition_id.0 as usize].gray_list.pop()
            {
                let node = unsafe { node_ptr.as_mut() };
                // Clear gray_listed flag when popping (O(1) instead of O(n) contains)
                node.set_gray_listed(false);

                if node.color() == GcTriColor::Gray {
                    if cnt >= max_steps {
                        self.partitions[partition_id.0 as usize]
                            .gray_list
                            .push(node_ptr);
                        return false;
                    }

                    // SAFETY: node_dtypes is &'static, and we hold &mut self so the
                    // registry is guaranteed to be alive.
                    debug_assert!(
                        gcx.traced_nodes.is_empty(),
                        "trace context should be empty before tracing a new node"
                    );
                    gcx.traced_nodes.clear();

                    unsafe {
                        let dtype = node_ptr.as_ref().dtype() as usize;
                        let info = &(*node_dtypes).type_info_list[dtype];
                        (info.trace_fn)(node_ptr, &mut gcx);
                    }

                    while let Some(mut ch) = gcx.traced_nodes.pop() {
                        let child = unsafe { ch.as_mut() };

                        #[cfg(debug_assertions)]
                        child.debug_assert_node_valid_simple();

                        if matches!(child.color(), GcTriColor::White | GcTriColor::Gray) {
                            child.set_color(GcTriColor::Gray);
                            child.set_gray_listed(true);

                            let child_pid = child.partition_id();
                            if child_pid == partition_id {
                                // Same partition: push directly
                                self.partitions[partition_id.0 as usize].gray_list.push(ch);
                            } else {
                                // Cross-partition child: buffer and push after
                                // the inner loop so we don't contend with the
                                // while-let borrow on self.partitions[pid].
                                cross_buffer.push((child_pid.0, ch));
                            }
                        }
                    }

                    // Flush cross-partition children to their own partitions
                    for (cpid, node) in cross_buffer.drain(..) {
                        self.partitions[cpid as usize].gray_list.push(node);
                    }

                    // Mark current node as black.
                    node.set_color(GcTriColor::Black);

                    cnt += 1;
                }
            }
        }

        true
    }

    pub fn mark(&mut self, partition_id: GcPartitionId, max_steps: usize) -> bool {
        self.mark_prepare(partition_id);
        if max_steps > 0 {
            self.mark_grays(partition_id, max_steps)
        } else {
            false
        }
    }

    /// dispose white nodes in the partition.
    /// `on_dispose` is called BEFORE a node will be disposed.
    pub fn sweep(
        &mut self,
        partition_id: GcPartitionId,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        if (partition_id.0 as usize) >= self.partitions.len() {
            return 0;
        }
        if let Some(link0) = {
            let par = &mut self.partitions[partition_id.0 as usize];
            if par.is_marking() && par.gray_list.is_empty() {
                par.set_marking(false);
                std::mem::take(&mut par.nodes).into_inner()
            } else {
                None // mark cycle not done
            }
        } {
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

            #[cfg(feature = "gc_arena")]
            let mut holes: Vec<crate::arena::Hole> = Vec::new();

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

                        if drop_pass == pass
                            && this.as_ref().color() == GcTriColor::White
                            && !this.as_ref().is_root_or_local()
                        {
                            if let Some(mut p) = prev {
                                p.as_mut().next = current;
                            } else {
                                link1 = current;
                            }

                            if call_on_dispose {
                                on_dispose(self, this.as_ref());
                            }

                            // Cache arena info before dispose poisons GcHead
                            #[cfg(feature = "gc_arena")]
                            let arena_info = {
                                let hd = this.as_ref();
                                let is_arena = hd.contains_flag(GcNodeFlag::ARENA_ALLOC);
                                let dtype = hd.dtype() as usize;
                                let info = &self.node_dtypes.type_info_list[dtype];
                                (is_arena, info.layout().size())
                            };

                            freed_bytes += self.dispose(this);

                            // Arena hole collection (after dispose, which skips mem_dealloc)
                            #[cfg(feature = "gc_arena")]
                            if arena_info.0 {
                                self.partitions[partition_id.0 as usize].arena.collect_hole(
                                    &mut holes,
                                    this.cast::<u8>(),
                                    arena_info.1,
                                );
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

            #[cfg(feature = "gc_arena")]
            self.partitions[partition_id.0 as usize]
                .arena
                .finish_sweep(&mut holes);

            debug_assert!(
                self.partitions[partition_id.0 as usize]
                    .gray_list
                    .is_empty()
            );

            // update remainder node link of partition
            if link1.is_some() {
                #[cfg(debug_assertions)]
                for n in NodeLinkIter::new(link1) {
                    unsafe {
                        debug_assert!(
                            n.as_ref().color() == GcTriColor::Black
                                || n.as_ref().is_root_or_local(),
                            "live nodes should be black, root or protected"
                        );
                    }
                }

                self.partitions[partition_id.0 as usize].nodes =
                    crate::node_link::GcNodeLink::new(link1);
            }

            // Memory accounting is already handled by dispose() which calls
            // update_mem_use() for each individual node. No need to subtract
            // freed_bytes again here.
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
        mut link: GcNodeLink,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        let call_on_dispose = !std::ptr::addr_eq(&on_dispose, &Self::DUMMY_DISPOSE_CALLBACK);
        let mut freed_bytes = 0;

        let pass_slice = self.node_dtypes.drop_passes;
        for &pass in pass_slice {
            log::trace!("[dipose_all] pass {pass}, count={}", link.len());

            let self_ptr: *mut GcHeap = self;

            link.filter_remove_with(
                |node| {
                    #[cfg(debug_assertions)]
                    unsafe {
                        node.debug_assert_node_valid(&*self_ptr);
                    }

                    let dtype = node.dtype() as usize;
                    let info = unsafe { &(*self_ptr).node_dtypes.type_info_list[dtype] };
                    info.drop_pass == pass
                },
                |node_ptr| unsafe {
                    if call_on_dispose {
                        on_dispose(&*self_ptr, node_ptr.as_ref());
                    }

                    freed_bytes += (&mut *self_ptr).dispose(node_ptr);
                },
            );

            if link.head().is_none() {
                break;
            }
        }

        debug_assert!(
            link.head().is_none(),
            "dispose_all_nodes: link still has nodes after disposal",
        );
        log::trace!("[dipose_all] done, freed {} bytes", freed_bytes);

        freed_bytes
    }
}

#[cfg(test)]
mod sweep_test {
    use super::*;
    use crate::{GcNode, GcRef};

    use crate::trace::{GcTrace, GcTraceCtx};

    #[derive(Debug)]
    struct MyI32(i32);

    impl GcTrace for MyI32 {
        fn trace(&self, _: &mut GcTraceCtx) {}
    }

    crate::gc_type_register! {
        MyI32, drop_pass = 0;
    }

    /// Helper function to count nodes in a partition
    fn count_nodes_in_partition(heap: &GcHeap, partition_id: GcPartitionId) -> usize {
        heap.nodes(partition_id).count()
    }

    /// Helper function to get all node pointers in a partition
    fn get_all_nodes_in_partition(
        heap: &GcHeap,
        partition_id: GcPartitionId,
    ) -> Vec<NonNull<GcHead>> {
        heap.nodes(partition_id).collect()
    }

    /// Test basic sweep functionality
    #[test]
    fn test_sweep_with_basic() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| unsafe { heap.alloc_raw(partition_id, MyI32(i)) }.unwrap())
            .collect();

        for (i, obj) in objects.iter().enumerate() {
            if i % 2 == 1 {
                unsafe {
                    let head = obj.head_ptr.as_ptr();
                    let attrs = (*head).attrs | crate::node::GcNodeFlag::ROOT.bits() as u32;
                    std::ptr::write(&mut (*head).attrs, attrs);
                }
            }
        }

        assert_eq!(count_nodes_in_partition(&heap, partition_id), 5);

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, |_, _| {});
        assert!(removed > 0, "Should have freed some bytes");

        assert_eq!(
            count_nodes_in_partition(&heap, partition_id),
            2,
            "Only root nodes should remain"
        );

        let remaining_nodes = get_all_nodes_in_partition(&heap, partition_id);
        let info = &GC_TYPE_REGISTRY.type_info_list[MyI32::GC_TYPE_ID as usize];
        for node in remaining_nodes {
            unsafe {
                let payload_ptr = info.payload_ptr(node);
                let value = *(payload_ptr.as_ptr() as *const i32);
                assert_eq!(value % 2, 1, "Remaining nodes should have odd values");
            }
        }
    }

    /// Test removing chain head nodes (n个节点被剔除后)
    #[test]
    fn test_sweep_with_chain_head_removal() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| unsafe { heap.alloc_raw(partition_id, MyI32(i)) }.unwrap())
            .collect();

        let _ = unsafe { heap.alloc_root_raw(partition_id, MyI32(3)) }.unwrap();
        let _ = unsafe { heap.alloc_root_raw(partition_id, MyI32(4)) }.unwrap();

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 2 nodes left (3 and 4)
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 2);

        // Verify chain head is now the node with value 4
        let head = heap.partitions[partition_id.0 as usize].nodes.head();
        assert!(head.is_some(), "Chain head should exist");

        unsafe {
            let info = &GC_TYPE_REGISTRY.type_info_list[MyI32::GC_TYPE_ID as usize];
            let payload_ptr = info.payload_ptr(head.unwrap());
            let value = (*(payload_ptr.as_ptr() as *const MyI32)).0;
            assert_eq!(
                value, 4,
                "Chain head should be value 4 (last allocated, first in chain)"
            );
        }

        // Verify the chain is properly linked
        let nodes = get_all_nodes_in_partition(&heap, partition_id);
        assert_eq!(nodes.len(), 2);

        unsafe {
            let info = &GC_TYPE_REGISTRY.type_info_list[MyI32::GC_TYPE_ID as usize];
            let payload_ptr1 = info.payload_ptr(nodes[0]).cast::<MyI32>();
            let value1 = (*payload_ptr1.as_ptr()).0;
            assert_eq!(value1, 4);

            let payload_ptr2 = info.payload_ptr(nodes[1]).cast::<MyI32>();
            let value2 = (*payload_ptr2.as_ptr()).0;
            assert_eq!(value2, 3);
        }
    }

    /// Test removing all chain head nodes (连续剔除所有链头节点)
    #[test]
    fn test_sweep_with_all_chain_head_removal() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let _objects: Vec<GcRef<MyI32>> = (0..3)
            .map(|i| unsafe { heap.alloc_raw(partition_id, MyI32(i)) }.unwrap())
            .collect();

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, |_, n| {
            println!("dispose {n:?}");
        });

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 0 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 0);

        // Chain head should be None
        let head = heap.partitions[partition_id.0 as usize].nodes.head();
        assert!(
            head.is_none(),
            "Chain head should be None after removing all nodes"
        );
    }

    /// Test removing middle nodes
    #[test]
    fn test_sweep_with_middle_node_removal() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let _objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| {
                if i != 2 {
                    unsafe { heap.alloc_root_raw(partition_id, MyI32(i)) }.unwrap()
                } else {
                    unsafe { heap.alloc_raw(partition_id, MyI32(i)) }.unwrap()
                }
            })
            .collect();

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 4 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 4);

        // Verify chain is still properly linked
        let nodes = get_all_nodes_in_partition(&heap, partition_id);
        assert_eq!(nodes.len(), 4);

        let expected_values = [4, 3, 1, 0];
        let info = &GC_TYPE_REGISTRY.type_info_list[MyI32::GC_TYPE_ID as usize];
        for (i, node) in nodes.iter().enumerate() {
            unsafe {
                let payload_ptr = info.payload_ptr(*node);
                let value = (*(payload_ptr.as_ptr() as *const MyI32)).0;
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
        let partition_id = heap.create_partition();

        let root_obj = unsafe { heap.alloc_raw(partition_id, MyI32(0)) }.unwrap();
        let _objects: Vec<GcRef<MyI32>> = (1..3)
            .map(|i| unsafe { heap.alloc_raw(partition_id, MyI32(i)) }.unwrap())
            .collect();

        let _ = unsafe { heap.alloc_root_raw(partition_id, MyI32(1)) }.unwrap();

        let _ = unsafe { heap.alloc_root_raw(partition_id, MyI32(1)) }.unwrap();

        while !heap.mark(partition_id, 64) {}

        let removed = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);

        assert!(removed > 0, "Should have freed some bytes");

        // Should have 2 nodes left
        assert_eq!(count_nodes_in_partition(&heap, partition_id), 2);

        let remaining = get_all_nodes_in_partition(&heap, partition_id);
        assert!(!remaining.contains(&root_obj.head_ptr));
    }

    /// Test empty partition
    #[test]
    fn test_sweep_with_empty_partition() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

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
