// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ptr::NonNull};

use crate::{GcHead, GcHeap, node::GcTriColor, node_link::GcNodeLink};

/// Partition ID
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GcPartitionId(pub u16);

impl GcPartitionId {
    /// Special partition ID representing no partition (null value)
    pub const NONE: Self = Self(0);

    #[inline(always)]
    pub const fn is_null(&self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Debug for GcPartitionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug)]
pub struct GcPartition {
    /// link of nodes in this partition
    pub(crate) nodes: GcNodeLink,
    /// gray nodes to be traced in this partition
    pub(crate) gray_list: Vec<NonNull<GcHead>>,
    /// Is in a marking cycle
    marking: bool,

    pub(crate) memory_used: usize,
}

impl GcPartition {
    fn new() -> Self {
        Self {
            memory_used: 0,
            nodes: GcNodeLink::default(),
            gray_list: Vec::new(),
            marking: false,
        }
    }

    #[inline(always)]
    pub fn memory_used(&self) -> usize {
        self.memory_used
    }

    #[inline(always)]
    pub const fn is_marking(&self) -> bool {
        self.marking
    }

    #[inline(always)]
    pub(crate) const fn set_marking(&mut self, marking: bool) {
        self.marking = marking;
    }

    pub(crate) fn add_gray_node(&mut self, mut node: NonNull<GcHead>) {
        debug_assert!(self.is_marking());

        match unsafe { node.as_ref().color() } {
            GcTriColor::White => unsafe {
                node.as_mut().set_color(GcTriColor::Gray);
            },
            GcTriColor::Gray => {}
            GcTriColor::Black => {
                return;
            }
        }

        if !self.gray_list.contains(&node) {
            self.gray_list.push(node);
        }
    }
}

impl GcHeap {
    /// Create a new partition.
    pub fn create_partition(&mut self) -> GcPartitionId {
        thread_local! {
            static NEXT_PARTITION_ID: Cell<u16> = const { Cell::new(1) };
        }

        let id = NEXT_PARTITION_ID.with(|next_id| {
            let mut serial = next_id.get();
            if serial == 0 {
                serial = 1;
            }
            let start = serial;

            loop {
                let candidate = GcPartitionId(serial);
                let conflict = self.partitions.contains_key(&candidate);
                if !conflict {
                    let next = if serial == u16::MAX { 1 } else { serial + 1 };
                    next_id.set(next);
                    return candidate;
                }

                serial = if serial == u16::MAX { 1 } else { serial + 1 };
                if serial == start {
                    panic!("too many active partitions");
                }
            }
        });

        let partition = GcPartition::new();
        self.partitions.insert(id, partition);

        log::trace!("[new_scope] {id:?}");

        id
    }

    /// Remove a partition, and dispose unused nodes.
    pub fn remove_partition(
        &mut self,
        partition_id: GcPartitionId,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        if partition_id.is_null() {
            return 0;
        }

        #[cfg(debug_assertions)]
        {
            self.dbg_dropping_root_partition = Some(partition_id);
        }

        log::trace!("[close_scope] {partition_id:?}");

        let mut freed_bytes = 0;

        if let Some(mut par) = self.partitions.remove(&partition_id) {
            // Record partition memory before disposal; after remove() the partition
            // is gone, so dispose() cannot find it via update_mem_use().
            let partition_mem = par.memory_used;
            let link = std::mem::take(&mut par.nodes);
            freed_bytes += self.dispose_all_nodes(link, &on_dispose);

            // Reclaim partition-level memory from the global counter.
            debug_assert!(self.total_memory_used >= partition_mem);
            self.total_memory_used -= partition_mem;
        }

        log::trace!("[close_scope_done] {partition_id:?}");

        #[cfg(debug_assertions)]
        {
            self.dbg_dropping_root_partition = None;
        }

        freed_bytes
    }

    /// Get partition information
    #[inline(always)]
    pub fn partition(&self, partition_id: GcPartitionId) -> Option<&GcPartition> {
        self.partitions.get(&partition_id)
    }

    /// Get partition information
    #[inline(always)]
    pub fn partition_mut(&mut self, partition_id: GcPartitionId) -> Option<&mut GcPartition> {
        self.partitions.get_mut(&partition_id)
    }

    /// Get all partition IDs
    pub fn partition_ids(&self) -> Vec<GcPartitionId> {
        self.partitions.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct DummyType;
    impl crate::trace::GcTrace for DummyType {
        fn trace(&self, _: &mut crate::trace::GcTraceCtx) {}
    }

    crate::gc_type_register! {
        DummyType, drop_pass = 0;
    }

    #[test]
    fn test_partition_creation() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let id = heap.create_partition();

        let partition = heap.partition(id).unwrap();
        assert_eq!(partition.memory_used(), 0);

        // Clean up partition
        heap.remove_partition(id, |_, n| {
            println!("dispose: {n:?}");
        });
        assert!(heap.partition(id).is_none());
    }

    #[test]
    fn test_gc_threshold() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        heap.set_memory_limit(1024);

        assert_eq!(heap.gc_threshold(), 0);

        heap.set_gc_threshold(512);
        assert_eq!(heap.gc_threshold(), 512);

        heap.set_gc_threshold(2048);
        assert_eq!(heap.gc_threshold(), 819);

        heap.set_gc_threshold(0);
        assert_eq!(heap.gc_threshold(), 0);
    }

    #[test]
    fn test_memory_limit() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let id = heap.create_partition();

        assert_eq!(heap.memory_limit(), 0);

        // Simulate some allocations in a single partition
        heap.update_mem_use(id, 100);
        assert_eq!(heap.memory_limit(), 0);

        // Set limit larger than used memory
        let applied = heap.set_memory_limit(512);
        assert_eq!(applied, 512);
        assert_eq!(heap.memory_limit(), 512);

        // Set limit smaller than used memory should clamp to used
        let applied_small = heap.set_memory_limit(80);
        assert_eq!(applied_small, 100);
        assert_eq!(heap.memory_limit(), 100);

        // Set unlimited
        let applied_zero = heap.set_memory_limit(0);
        assert_eq!(applied_zero, 0);
        assert_eq!(heap.memory_limit(), 0);
    }

    #[test]
    fn test_is_ancestor_of() {
        // 已删除 ancestor 相关 API，此测试不再适用，保留空壳确保编译通过
    }

    #[test]
    fn test_common_parent() {
        // 已删除 common_parent 相关 API，此测试不再适用，保留空壳确保编译通过
    }

    #[test]
    fn test_update_mem_use() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let p1 = heap.create_partition();
        let p2 = heap.create_partition();

        heap.update_mem_use(p1, 100);
        assert_eq!(heap.partition(p1).unwrap().memory_used(), 100);
        assert_eq!(heap.partition(p2).unwrap().memory_used(), 0);

        heap.update_mem_use(p2, 50);
        assert_eq!(heap.partition(p1).unwrap().memory_used(), 100);
        assert_eq!(heap.partition(p2).unwrap().memory_used(), 50);

        heap.update_mem_use(p1, -20);
        assert_eq!(heap.partition(p1).unwrap().memory_used(), 80);
        assert_eq!(heap.partition(p2).unwrap().memory_used(), 50);

        // Clean up
        heap.remove_partition(p1, GcHeap::DUMMY_DISPOSE_CALLBACK);
    }

    #[test]
    fn test_partition_id_serial_and_range() {
        let id = GcPartitionId(10);
        assert_eq!(id.0, 10);
        assert!(!id.is_null());
        assert!(GcPartitionId::NONE.is_null());
    }

    #[test]
    fn test_remove_partition_reclaims_memory_stats() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let p1 = heap.create_partition();
        let p2 = heap.create_partition();

        // Allocate some nodes in p1
        let n1 = unsafe { heap.alloc_raw(p1, DummyType) }.unwrap();
        let n2 = unsafe { heap.alloc_raw(p1, DummyType) }.unwrap();
        let n3 = unsafe { heap.alloc_raw(p2, DummyType) }.unwrap();

        let used_before = heap.memory_used();
        assert!(
            used_before > 0,
            "memory_used should be > 0 after allocations"
        );

        let p1_used = heap.partition(p1).unwrap().memory_used();
        assert!(p1_used > 0, "partition memory_used should be > 0");

        // Remove p1 — all its nodes should be disposed and memory reclaimed
        let freed = heap.remove_partition(p1, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(freed > 0, "remove_partition should free some bytes");

        // p1 is gone
        assert!(heap.partition(p1).is_none());

        // total_memory_used should have decreased by at least p1_used
        let used_after = heap.memory_used();
        assert!(
            used_after < used_before,
            "total_memory_used should decrease after partition removal: before={}, after={}",
            used_before,
            used_after
        );

        // p2's memory should be unaffected
        assert_eq!(
            heap.partition(p2).unwrap().memory_used(),
            heap.memory_used(),
            "remaining partition should account for all remaining memory"
        );

        // Clean up p2
        heap.remove_partition(p2, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(heap.memory_used(), 0, "all memory should be reclaimed");
    }
}
