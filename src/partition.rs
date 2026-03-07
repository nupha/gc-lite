// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ptr::NonNull};

use smallvec::SmallVec;

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

    /// Current memory usage
    pub(crate) memory_used: usize,
    /// Memory usage limit, 0 for unlimited
    pub(crate) memory_limit: usize,
    /// Garbage collection threshold (triggers automatic GC when memory usage reaches this byte count)
    /// A value of 0 means automatic GC is disabled
    pub(crate) gc_threshold: usize,
}

impl GcPartition {
    fn new(memory_limit: usize) -> Self {
        Self {
            memory_used: 0,
            memory_limit,
            gc_threshold: 0, // Default threshold is 0 bytes (disable automatic GC)
            nodes: GcNodeLink::default(),
            gray_list: Vec::new(),
            marking: false,
        }
    }

    #[inline(always)]
    pub fn memory_used(&self) -> usize {
        self.memory_used
    }

    /// Get memory limit, 0 for unlimited.
    #[inline(always)]
    pub fn memory_limit(&self) -> usize {
        self.memory_limit
    }

    /// Set memory limit, 0 for unlimited.
    /// if `limit` is less than currently used memory, bring up limit to used memory instead.
    /// Returns the actual memory limit applied.
    pub fn set_memory_limit(&mut self, limit: usize) -> usize {
        if limit == 0 {
            self.memory_limit = 0;
            0
        } else {
            let n = std::cmp::max(self.memory_used, limit);
            self.memory_limit = n;
            if self.gc_threshold >= n {
                self.gc_threshold = n - (n >> 2); // 0.75x of
            }
            n
        }
    }

    /// Check if garbage collection is needed
    #[inline(always)]
    pub fn should_gc(&self) -> bool {
        // If GC threshold > 0 and memory usage reaches threshold, trigger GC
        // gc_threshold = 0 means automatic GC is disabled
        self.gc_threshold > 0 && self.memory_used >= self.gc_threshold
    }

    /// Get garbage collection threshold (bytes)
    ///
    /// A return value of 0 means automatic GC is disabled
    #[inline(always)]
    pub fn gc_threshold(&self) -> usize {
        self.gc_threshold
    }

    /// Set garbage collection threshold (bytes)
    ///
    /// # Parameters
    /// - `threshold`: New garbage collection threshold (bytes)
    ///   - A value of 0 disables automatic GC
    ///   - Value must be > 0 and <= partition memory limit (if memory limit is set)
    ///
    /// # Notes
    /// This method does not perform validation, caller should ensure threshold validity
    pub fn set_gc_threshold(&mut self, threshold: usize) -> usize {
        let limit = self.memory_limit;
        if threshold > 0 && limit > 0 {
            let n = std::cmp::min(
                threshold,
                limit * 8 / 10, // 0.8x of max
            );
            self.gc_threshold = n;
            n
        } else {
            self.gc_threshold = threshold;
            threshold
        }
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
    ///
    /// # Parameters
    /// - `memory_limit`: Optional memory limit (0 for unlimited)
    ///
    /// # Returns
    /// The ID of the newly created partition
    pub fn create_partition(&mut self, memory_limit: usize) -> GcPartitionId {
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

        let partition = GcPartition::new(memory_limit);
        self.partitions.insert(id, partition);

        log::trace!("[new_scope] {id:?}");

        id
    }

    /// Remove a partition, and dispose unused nodes.
    /// For non-root partition, migrate xref nodes is optionally performed.
    pub fn remove_partition(
        &mut self,
        partition_id: GcPartitionId,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        log::trace!("[close_scope] {partition_id:?}");

        let mut freed_bytes = 0;

        if let Some(mut par) = self.partitions.remove(&partition_id) {
            let link = std::mem::take(&mut par.nodes);
            freed_bytes += self.dispose_all_nodes(link, &on_dispose);
        }

        log::trace!("[close_scope_done] {partition_id:?}");

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
        let id = heap.create_partition(1024);

        let partition = heap.partition(id).unwrap();
        assert_eq!(partition.memory_limit(), 1024);
        assert_eq!(partition.gc_threshold(), 0); // Default threshold is 0, automatic GC disabled

        // Clean up partition
        heap.remove_partition(id, |_, n| {
            println!("dispose: {n:?}");
        });
        assert!(heap.partition(id).is_none());
    }

    #[test]
    fn test_gc_threshold() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let id = heap.create_partition(1024);

        let partition = heap.partition_mut(id).unwrap();
        assert_eq!(partition.gc_threshold(), 0);

        partition.set_gc_threshold(512);
        assert_eq!(partition.gc_threshold(), 512);

        partition.set_gc_threshold(0);
        assert_eq!(partition.gc_threshold(), 0);

        // Clean up
        heap.remove_partition(id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    }

    #[test]
    fn test_memory_limit() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let id = heap.create_partition(1024);

        let partition = heap.partition_mut(id).unwrap();
        assert_eq!(partition.memory_limit(), 1024);

        partition.set_memory_limit(2048);
        assert_eq!(partition.memory_limit(), 2048);

        partition.set_memory_limit(0);
        assert_eq!(partition.memory_limit(), 0);

        // Clean up
        heap.remove_partition(id, GcHeap::DUMMY_DISPOSE_CALLBACK);
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
        let p1 = heap.create_partition(0);
        let p2 = heap.create_partition(0);

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
}
