// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{GcHead, GcHeap, node::GcTriColor, node_link::GcNodeLink};

#[cfg(feature = "gc_arena")]
use crate::arena::GcArena;

/// Partition ID, used as index into `GcHeap::partitions`.
///
/// Every node belongs to exactly one partition (index >= 0).
/// There is no "null" partition ID.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct GcPartitionId(pub u16);

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

    /// bump + free-list arena allocator
    #[cfg(feature = "gc_arena")]
    pub(crate) arena: GcArena,
}

impl GcPartition {
    pub(super) fn new() -> Self {
        Self {
            memory_used: 0,
            nodes: GcNodeLink::default(),
            gray_list: Vec::new(),
            marking: false,

            #[cfg(feature = "gc_arena")]
            arena: GcArena::new(crate::arena::ARENA_CAPACITY).expect("arena alloc failed"),
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
        debug_assert!(
            self.is_marking(),
            "add_gray_node called when partition is not marking"
        );

        match unsafe { node.as_ref().color() } {
            GcTriColor::White => unsafe {
                node.as_mut().set_color(GcTriColor::Gray);
            },
            GcTriColor::Gray => {}
            GcTriColor::Black => {
                return;
            }
        }

        unsafe {
            if !node.as_ref().is_gray_listed() {
                node.as_mut().set_gray_listed(true);
                self.gray_list.push(node);
            }
        }
    }
}

impl GcHeap {
    /// Create a new partition and return its ID.
    pub fn create_partition(&mut self) -> GcPartitionId {
        let id = GcPartitionId(self.partitions.len() as u16);
        self.partitions.push(GcPartition::new());
        id
    }

    // ── Two-phase partition removal ──────────────────────────────────────
    //
    //  Phase 1 (finalize): takes &self, calls Drop on all node payloads without
    //    deallocating memory. Drop implementations can safely access GcHeap
    //    through a shared reference (e.g. &GcHeap), avoiding the aliasing
    //    problem that existed in the single-phase remove_partition.
    //
    //  Phase 2 (dealloc):   takes &mut self, frees the memory of all finalized
    //    nodes and resets the partition state. No Drop callbacks run
    //    at this point, so there is no risk of reentrant &mut self access.
    //
    //  remove_partition remains for backward compatibility and calls both
    //    phases in sequence.

    /// Phase 1: Finalize — call `Drop` on all node payloads without freeing memory.
    ///
    /// Only takes `&self`, so `Drop` implementations can safely access `GcHeap`
    /// via a shared reference. Returns the node link containing all finalized
    /// nodes, which must be passed to [`dealloc_partition`] to reclaim memory.
    ///
    /// Scope caches associated with this partition are cleared before any drops
    /// are called, so that `GcScopeState::clear()` (which only touches node flags)
    /// runs before payload drops.
    pub fn finalize_partition(&self, partition_id: GcPartitionId) -> Option<GcNodeLink> {
        // Check that the partition exists
        self.partitions.get(partition_id.0 as usize)?;

        // Clear scope caches associated with this partition BEFORE dropping nodes.
        // GcScopeState::clear() only touches node flags and does not access any
        // GcHeap fields, so it is safe to call during finalize_partition.
        for stack in &self.scope_stacks {
            if stack.partition == Some(partition_id) {
                for s in &stack.list {
                    s.clear();
                }
            }
        }

        log::trace!("[finalize_partition] {partition_id:?}");

        // Clone the partition's node link so we can iterate without borrowing &self.
        let link = self.partitions[partition_id.0 as usize].nodes.clone();

        // Process nodes by drop pass order.
        // We iterate all nodes for each pass to respect inter-pass dependencies.
        for &pass in self.node_dtypes.drop_passes {
            for node in link.iter() {
                let dtype = unsafe { node.as_ref().dtype() } as usize;
                let info = &self.node_dtypes.type_info_list[dtype];
                if info.drop_pass == pass {
                    self.drop_node_payload_without_dealloc(node);
                }
            }
        }

        Some(link)
    }

    /// Phase 2: Dealloc — free memory of all finalized nodes and reset the partition.
    ///
    /// Takes `&mut self` — no `Drop` callbacks run at this point, so there is
    /// no risk of reentrant `&mut self` access. The `link` must be the value
    /// returned by [`finalize_partition`] for the same partition.
    ///
    /// Returns the total number of bytes freed.
    pub fn dealloc_partition(&mut self, partition_id: GcPartitionId, link: GcNodeLink) -> usize {
        let partition_mem = self.partitions[partition_id.0 as usize].memory_used;
        let mut freed_bytes = 0;

        // Deallocate all finalized nodes in the link.
        for node in link.iter() {
            // Handle weak reference cleanup.
            unsafe {
                if !node.as_ref().weak_id.is_null() {
                    let widx = node.as_ref().weak_id.index();
                    debug_assert!(
                        (widx as usize) < self.weak_slots.len(),
                        "dealloc_partition: weak slot index {} out of bounds (len {})",
                        widx,
                        self.weak_slots.len(),
                    );
                    self.weak_slots.get_unchecked_mut(widx as usize).1.take();
                }
            }

            let dtype = unsafe { node.as_ref().dtype() } as usize;
            let info = &self.node_dtypes.type_info_list[dtype];
            let layout = info.layout();
            let gross_size = layout.size();

            #[cfg(debug_assertions)]
            unsafe {
                // Poison GcHead fields so any subsequent use-after-free is caught.
                (*node.as_ptr()).attrs = 0xDEAD_BEEF;
                (*node.as_ptr()).next = None;
            }

            #[cfg(feature = "gc_arena")]
            {
                let head = unsafe { node.as_ref() };
                if head.contains_flag(crate::node::GcNodeFlag::ARENA_ALLOC) {
                    // Arena-allocated node: memory is owned by GcArena,
                    // which will be dropped when the partition is reset below.
                    // Do NOT individually dealloc — that would be a double-free.
                    #[cfg(debug_assertions)]
                    {
                        self.dbg_living_nodes.remove(&node.cast());
                    }

                    freed_bytes += gross_size;
                    continue;
                }
            }

            self.mem_dealloc(node.cast::<u8>(), layout);
            freed_bytes += gross_size;
        }

        debug_assert!(
            self.total_memory_used >= partition_mem,
            "dealloc_partition: global memory underflow ({} < {})",
            self.total_memory_used,
            partition_mem,
        );
        self.total_memory_used -= partition_mem;

        // Reset partition to fresh state (all nodes have been freed)
        self.partitions[partition_id.0 as usize] = GcPartition::new();

        log::trace!("[dealloc_partition] freed {freed_bytes} bytes");

        freed_bytes
    }

    /// Remove a partition and reclaim all its memory.
    ///
    /// ⚠️ **DEPRECATED** — This method is inherently unsound. It holds `&mut self`
    /// while `Drop` callbacks run inside [`finalize_partition`], and those
    /// callbacks can re-enter `GcHeap` through raw pointers, creating aliasing
    /// `&mut` references (UB).
    ///
    /// **Use the two-phase API instead:**
    ///
    /// ```ignore
    /// let link = gc_heap.finalize_partition(pid);
    /// // ... (Drop callbacks that access GcHeap are safe here) ...
    /// if let Some(link) = link {
    ///     gc_heap.dealloc_partition(pid, link);
    /// }
    /// ```
    ///
    /// See [`finalize_partition`] (takes `&self`) and [`dealloc_partition`]
    /// (takes `&mut self`) for details.
    ///
    /// The `on_dispose` parameter is kept for API compatibility but is no
    /// longer called — `Drop` is handled internally by `finalize_partition`.
    #[deprecated(
        note = "unsound — use finalize_partition(&self) + dealloc_partition(&mut self) instead"
    )]
    pub fn remove_partition(
        &mut self,
        partition_id: GcPartitionId,
        _on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        let link = self.finalize_partition(partition_id);
        link.map_or(0, |link| {
            #[cfg(debug_assertions)]
            {
                self.dbg_dropping_root_partition = Some(partition_id);
            }
            let result = self.dealloc_partition(partition_id, link);
            #[cfg(debug_assertions)]
            {
                self.dbg_dropping_root_partition = None;
            }
            result
        })
    }

    /// Get partition information
    #[inline(always)]
    pub fn partition(&self, partition_id: GcPartitionId) -> Option<&GcPartition> {
        self.partitions.get(partition_id.0 as usize)
    }

    /// Get partition information
    #[inline(always)]
    pub fn partition_mut(&mut self, partition_id: GcPartitionId) -> Option<&mut GcPartition> {
        self.partitions.get_mut(partition_id.0 as usize)
    }

    /// Get all partition IDs
    pub fn partition_ids(&self) -> Vec<GcPartitionId> {
        (0..self.partitions.len())
            .map(|i| GcPartitionId(i as u16))
            .collect()
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

        // Clean up partition using two-phase API
        let link = heap.finalize_partition(id).unwrap();
        heap.dealloc_partition(id, link);
        // After dealloc, partition is reset to a fresh state
        assert_eq!(heap.partition(id).unwrap().memory_used(), 0);
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
        let id = heap.create_partition();

        heap.update_mem_use(id, 100);
        assert_eq!(heap.partition(id).unwrap().memory_used(), 100);
        assert_eq!(heap.memory_used(), 100);

        heap.update_mem_use(id, -20);
        assert_eq!(heap.partition(id).unwrap().memory_used(), 80);
        assert_eq!(heap.memory_used(), 80);

        // Clean up using two-phase API
        let link = heap.finalize_partition(id).unwrap();
        heap.dealloc_partition(id, link);
    }

    #[test]
    fn test_partition_id_serial_and_range() {
        let id = GcPartitionId(10);
        assert_eq!(id.0, 10);
    }

    #[test]
    fn test_remove_partition_reclaims_memory_stats() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let id = heap.create_partition();

        // Allocate some nodes
        let _n1 = unsafe { heap.alloc_raw(id, DummyType) }.unwrap();
        let _n2 = unsafe { heap.alloc_raw(id, DummyType) }.unwrap();

        let used_before = heap.memory_used();
        assert!(
            used_before > 0,
            "memory_used should be > 0 after allocations"
        );

        let p_used = heap.partition(id).unwrap().memory_used();
        assert!(p_used > 0, "partition memory_used should be > 0");

        // Remove partition — all its nodes should be disposed and memory reclaimed
        let link = heap.finalize_partition(id).unwrap();
        let freed = heap.dealloc_partition(id, link);
        assert!(freed > 0, "should free some bytes");

        // Partition is reset to fresh state
        assert_eq!(heap.partition(id).unwrap().memory_used(), 0);
        assert_eq!(heap.memory_used(), 0, "all memory should be reclaimed");
    }
}
