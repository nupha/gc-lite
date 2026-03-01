// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{
    GcHeap,
    node::{GcHead, GcTriColor},
    node_iterator::{GcNodeLink, NodeLinkIter},
    partition::GcPartitionId,
};

impl GcHeap {
    pub fn add_gray_node(&mut self, node: NonNull<GcHead>) {
        if unsafe { node.as_ref().color() } != GcTriColor::Black {
            self.partition_mut(unsafe { node.as_ref().partition_id() })
                .unwrap()
                .add_gray_node(node);
        }
    }

    pub fn mark_reset(&mut self, partition_id: GcPartitionId) {
        if let Some(par) = self.partition_mut(partition_id) {
            par.set_marking(false);
            par.gray_list.clear();
            for n in par.nodes_mut() {
                n.set_color(GcTriColor::White);
            }
        }
    }

    /// ensure marking cycle is started:
    /// if marking is in progress, exit do nothing;
    /// if marking is done, start new cycle, add initialize gray list with root nodes.
    pub fn ensure_mark_cycle(&mut self, partition_id: GcPartitionId) {
        if let Some(par) = self.partitions.get_mut(&partition_id) {
            if !par.is_marking() {
                debug_assert!(par.gray_list.is_empty());

                // reset nodes color to white
                for mut n in par.nodes.iter() {
                    unsafe {
                        n.as_mut().set_color(GcTriColor::White);
                    }
                }

                par.set_marking(true);

                // add root nodes
                for n in par.root_nodes.iter() {
                    let mut root = *n;
                    unsafe {
                        root.as_mut().set_color(GcTriColor::Gray);
                    }
                    par.gray_list.push(root);
                }
            }
        }
    }

    pub fn mark_grays(&mut self, partition_id: GcPartitionId, max_steps: usize) -> bool {
        if max_steps == 0 {
            return false;
        }

        let heap_ptr = self as *mut Self;

        if let Some(par) = self.partitions.get_mut(&partition_id)
            && !par.gray_list.is_empty()
        {
            let mut gcx = unsafe { (*heap_ptr).create_trace_ctx() };
            let mut cnt = 0;

            while let Some(mut node_ptr) = par.gray_list.pop() {
                let node = unsafe { node_ptr.as_mut() };
                debug_assert_eq!(node.partition_id(), partition_id);

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

                        let pid = child.partition_id();
                        if pid == partition_id {
                            if matches!(child.color(), GcTriColor::White | GcTriColor::Gray) {
                                child.set_color(GcTriColor::Gray);
                                par.gray_list.push(ch);
                            }
                        } else {
                            let p2 = unsafe { (*heap_ptr).partition(pid).unwrap() };
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

        true
    }

    pub fn mark(&mut self, partition_id: GcPartitionId, max_steps: usize) -> bool {
        self.ensure_mark_cycle(partition_id);
        if max_steps > 0 {
            self.mark_grays(partition_id, max_steps)
        } else {
            false
        }
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
                std::mem::take(&mut p.nodes).into_inner()
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

                        if drop_pass == pass
                            && this.as_ref().color() == GcTriColor::White
                            && !this.as_ref().is_protected()
                        {
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
                            n.as_ref().color() == GcTriColor::Black
                                || n.as_ref().is_root()
                                || n.as_ref().is_protected(),
                            "live nodes should be black, root or protected"
                        );
                    }
                }

                let p = self.partition_mut(partition_id).unwrap();
                p.nodes = crate::node_iterator::GcNodeLink::new(link1);
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
        link: GcNodeLink,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        let call_on_dispose = !std::ptr::addr_eq(&on_dispose, &Self::DUMMY_DISPOSE_CALLBACK);
        let mut link = link.into_inner();
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

    fn count_black_nodes(heap: &GcHeap, partition_id: GcPartitionId) -> usize {
        let mut count = 0;
        for node in heap.nodes(partition_id) {
            unsafe {
                if node.as_ref().color() == GcTriColor::Black {
                    count += 1;
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
        heap.nodes(partition_id).collect()
    }

    /// Test basic sweep functionality
    #[test]
    fn test_sweep_with_basic() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| unsafe { heap.alloc_raw(partition_id, MyI32(i)) }.unwrap())
            .collect();

        for (i, obj) in objects.iter().enumerate() {
            if i % 2 == 1 {
                // Make existing nodes roots instead of creating new ones.
                unsafe {
                    let head = obj.head_ptr.as_ptr();
                    let attrs = (*head).attrs | crate::node::GcNodeFlag::ROOT.bits() as u32;
                    std::ptr::write(&mut (*head).attrs, attrs);
                    heap.partition_mut(partition_id)
                        .unwrap()
                        .root_nodes
                        .push(obj.head_ptr);
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
        let head = heap
            .partitions
            .get(&partition_id)
            .and_then(|p| p.nodes.head());
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
        let head = heap
            .partitions
            .get(&partition_id)
            .and_then(|p| p.nodes.head());
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
