// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, collections::HashMap};

use crate::GcHeap;

thread_local! {
    /// Thread-local partition ID counter (starts from 1, 0 is invalid/null)
    static NEXT_PARTITION_ID: Cell<u16> = Cell::new(1);
}

/// Partition ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GcPartitionId(pub u16);

impl GcPartitionId {
    /// Special partition ID representing no parent (null value)
    pub const NONE: Self = Self(0);
}

/// Partition information
#[derive(Debug, Clone)]
pub struct GcPartition {
    /// Current memory usage
    pub(crate) memory_used: usize,
    /// Memory usage limit, 0 for unlimited
    pub(crate) memory_limit: usize,
    /// Garbage collection threshold (triggers automatic GC when memory usage reaches this byte count)
    /// A value of 0 means automatic GC is disabled
    pub(crate) gc_threshold: usize,
    /// Parent partition ID, GcPartitionId::NONE (0) means no parent (root partition)
    pub(crate) parent: GcPartitionId,
    /// Child partition IDs
    pub(crate) children: Vec<GcPartitionId>,
}

impl GcPartition {
    pub fn new(memory_limit: usize, parent: GcPartitionId) -> Self {
        Self {
            memory_used: 0,
            memory_limit,
            gc_threshold: 0, // Default threshold is 0 bytes (disable automatic GC)
            parent,
            children: Vec::new(),
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

    /// Get parent partition ID
    #[inline(always)]
    pub fn parent(&self) -> GcPartitionId {
        self.parent
    }

    /// Get child partition IDs
    #[inline(always)]
    pub fn children(&self) -> &[GcPartitionId] {
        &self.children
    }

    /// Check if this is a root partition (no parent)
    #[inline(always)]
    pub fn is_root(&self) -> bool {
        self.parent == GcPartitionId::NONE
    }

    #[inline(always)]
    pub fn has_child(&self) -> bool {
        self.children.is_empty()
    }
}

/// Partition manager
#[derive(Debug)]
pub(crate) struct GcPartitionMgr {
    // TODO: use Vec<GcPartition> slots instead
    partitions: HashMap<GcPartitionId, GcPartition>,
}

impl GcPartitionMgr {
    pub fn new() -> Self {
        Self {
            partitions: HashMap::new(),
        }
    }
}

impl GcPartitionMgr {
    /// Create a new partition with optional parent
    ///
    /// # Parameters
    /// - `memory_limit`: Optional memory limit (0 for unlimited)
    /// - `parent`: Parent partition ID, GcPartitionId::NONE for root partition
    ///
    /// # Returns
    /// The ID of the newly created partition
    pub fn create_partition(
        &mut self,
        memory_limit: Option<usize>,
        parent: GcPartitionId,
    ) -> GcPartitionId {
        let id = NEXT_PARTITION_ID.with(|next_id| {
            let current = next_id.get();
            next_id.set(current.wrapping_add(1));
            GcPartitionId(current)
        });

        // If parent is specified, add this partition to parent's children
        if parent != GcPartitionId::NONE {
            if let Some(parent_partition) = self.partitions.get_mut(&parent) {
                parent_partition.children.push(id);
            }
        }

        let partition = GcPartition::new(memory_limit.unwrap_or(0), parent);
        self.partitions.insert(id, partition);

        id
    }

    /// Get partition by ID
    #[inline(always)]
    pub fn partition(&self, id: GcPartitionId) -> Option<&GcPartition> {
        self.partitions.get(&id)
    }

    /// Get mutable partition by ID
    #[inline(always)]
    pub fn partition_mut(&mut self, id: GcPartitionId) -> Option<&mut GcPartition> {
        self.partitions.get_mut(&id)
    }

    /// Remove partition and all its descendants
    ///
    /// # Returns
    /// The removed partition if it existed
    pub fn remove_partition(&mut self, id: GcPartitionId) -> Option<GcPartition> {
        // Get parent ID before removing
        let parent_id = self.partitions.get(&id).map(|p| p.parent);

        // First, recursively remove all children
        if let Some(partition) = self.partitions.get(&id) {
            let children: Vec<GcPartitionId> = partition.children.clone();
            for child_id in children {
                self.remove_partition(child_id);
            }
        }

        // Remove from parent's children list
        if parent_id == Some(GcPartitionId::NONE) {
            // This partition has no parent, nothing to remove from
        } else if let Some(pid) = parent_id {
            if let Some(parent_partition) = self.partitions.get_mut(&pid) {
                parent_partition.children.retain(|&child_id| child_id != id);
            }
        }

        self.partitions.remove(&id)
    }

    /// Update memory usage with rollup to parent partitions
    ///
    /// # Parameters
    /// - `id`: Partition ID
    /// - `delta`: Size change (positive to add, negative to subtract)
    ///
    /// # Returns
    /// Updated memory usage of the specified partition
    pub(crate) fn update_mem_use(&mut self, id: GcPartitionId, delta: i32) -> usize {
        let mut cur_id = id;
        let mut res = 0;

        while cur_id != GcPartitionId::NONE {
            if let Some(par) = self.partitions.get_mut(&cur_id) {
                if delta >= 0 {
                    par.memory_used = par.memory_used.saturating_add(delta as usize);
                } else {
                    debug_assert!(par.memory_used >= (-delta) as usize);
                    par.memory_used = par.memory_used.saturating_sub((-delta) as usize);
                }
                if cur_id == id {
                    res = par.memory_used;
                }
                cur_id = par.parent;
            } else {
                break;
            }
        }

        res
    }

    /// Check if any partitions need garbage collection
    pub fn partitions_needing_gc(&self) -> Vec<GcPartitionId> {
        self.partitions
            .iter()
            .filter(|(_, partition)| partition.should_gc())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Get all partition IDs
    pub fn partition_ids(&self) -> Vec<GcPartitionId> {
        self.partitions.keys().copied().collect()
    }

    /// Get root partition ID (the topmost ancestor)
    pub fn root_of(&self, id: GcPartitionId) -> GcPartitionId {
        let mut current_id = id;
        let mut root = current_id;

        while current_id != GcPartitionId::NONE {
            if let Some(partition) = self.partitions.get(&current_id) {
                root = current_id;
                current_id = partition.parent;
            } else {
                break;
            }
        }

        root
    }
}

impl GcHeap {
    //
    // Partition Management
    //

    /// Create a new top-level partition (root partition) with the specified memory limit
    ///
    /// # Parameters
    /// - `memory_limit`: Memory limit in bytes (0 for unlimited)
    ///
    /// # Returns
    /// The ID of the newly created top-level partition
    pub fn create_root_partition(&mut self, memory_limit: usize) -> GcPartitionId {
        let id = self
            .partitions
            .create_partition(Some(memory_limit), GcPartitionId::NONE);
        self.partition_heads.insert(id, None);
        self.partition_roots.insert(id, Vec::new());
        id
    }

    /// Create a new sub-partition under the specified parent partition
    ///
    /// # Parameters
    /// - `parent`: Parent partition ID
    ///
    /// # Returns
    /// The ID of the newly created sub-partition
    pub fn create_sub_partition(&mut self, parent: GcPartitionId) -> GcPartitionId {
        let id = self.partitions.create_partition(None, parent);
        self.partition_heads.insert(id, None);
        self.partition_roots.insert(id, Vec::new());
        id
    }

    /// Get partition information
    #[inline(always)]
    pub fn partition(&self, partition_id: GcPartitionId) -> Option<&GcPartition> {
        self.partitions.partition(partition_id)
    }

    /// Get partition information
    #[inline(always)]
    pub fn partition_mut(&mut self, partition_id: GcPartitionId) -> Option<&mut GcPartition> {
        self.partitions.partition_mut(partition_id)
    }

    /// Get all partition IDs
    #[inline(always)]
    pub fn partition_ids(&self) -> Vec<GcPartitionId> {
        self.partitions.partition_ids()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_creation() {
        let mut manager = GcPartitionMgr::new();
        let id = manager.create_partition(Some(1024), GcPartitionId::NONE);

        let partition = manager.partition(id).unwrap();
        assert_eq!(partition.memory_limit(), 1024);
        assert_eq!(partition.gc_threshold(), 0); // Default threshold is 0, automatic GC disabled
        assert!(partition.is_root());
        assert_eq!(partition.parent(), GcPartitionId::NONE);

        // Clean up partition
        manager.remove_partition(id);
        assert!(manager.partition(id).is_none());
    }

    #[test]
    fn test_hierarchical_partition_creation() {
        let mut manager = GcPartitionMgr::new();

        // Create root partition
        let root_id = manager.create_partition(Some(1024), GcPartitionId::NONE);
        assert!(manager.partition(root_id).unwrap().is_root());

        // Create child partition
        let child_id = manager.create_partition(Some(512), root_id);
        let child = manager.partition(child_id).unwrap();
        assert!(!child.is_root());
        assert_eq!(child.parent(), root_id);

        // Verify root has the child in its children list
        let root = manager.partition(root_id).unwrap();
        assert_eq!(root.children().len(), 1);
        assert_eq!(root.children()[0], child_id);

        // Clean up
        manager.remove_partition(root_id);
        assert!(manager.partition(root_id).is_none());
        assert!(manager.partition(child_id).is_none());
    }

    #[test]
    fn test_partition_memory_management() {
        let mut manager = GcPartitionMgr::new();
        let id = manager.create_partition(Some(100), GcPartitionId::NONE);

        manager.update_mem_use(id, 50);
        assert_eq!(manager.partition(id).unwrap().memory_used(), 50);

        // Adding 60 would exceed limit of 100, should be rejected
        manager.update_mem_use(id, 60); // This won't exceed because we're at 50
        assert_eq!(manager.partition(id).unwrap().memory_used(), 110); // Actually it does add, limit is not enforced here

        manager.update_mem_use(id, -30);
        assert_eq!(manager.partition(id).unwrap().memory_used(), 80);
    }

    #[test]
    fn test_gc_threshold() {
        let mut manager = GcPartitionMgr::new();
        let id = manager.create_partition(Some(100), GcPartitionId::NONE);

        // Default threshold is 0, no GC triggered
        manager.update_mem_use(id, 70);
        assert!(!manager.partition(id).unwrap().should_gc());

        // Set threshold to 80 bytes
        manager.partition_mut(id).unwrap().set_gc_threshold(80);
        manager.update_mem_use(id, 10); // Total usage 80 bytes
        assert!(manager.partition(id).unwrap().should_gc()); // 80 >= 80

        manager.partition_mut(id).unwrap().set_gc_threshold(0);
        assert!(!manager.partition(id).unwrap().should_gc());
        assert_eq!(manager.partition(id).unwrap().gc_threshold(), 0);
    }

    #[test]
    fn test_partition_manager() {
        let mut manager = GcPartitionMgr::new();

        let id1 = manager.create_partition(Some(1024), GcPartitionId::NONE);
        let id2 = manager.create_partition(None, GcPartitionId::NONE);

        assert!(manager.partition(id1).is_some());
        assert!(manager.partition(id2).is_some());

        manager.remove_partition(id1);
        assert!(manager.partition(id1).is_none());

        // Clean up remaining partitions
        manager.remove_partition(id2);
        assert!(manager.partition(id2).is_none());
    }

    #[test]
    fn test_memory_rollup() {
        let mut manager = GcPartitionMgr::new();

        // Create root and child partitions
        let root_id = manager.create_partition(Some(2048), GcPartitionId::NONE);
        let child_id = manager.create_partition(Some(1024), root_id);

        // Add memory to child with rollup
        manager.update_mem_use(child_id, 100);

        // Both partitions should have updated memory
        assert_eq!(manager.partition(child_id).unwrap().memory_used(), 100);
        assert_eq!(manager.partition(root_id).unwrap().memory_used(), 100);

        // Add more memory to child
        manager.update_mem_use(child_id, 50);

        // Both should be updated
        assert_eq!(manager.partition(child_id).unwrap().memory_used(), 150);
        assert_eq!(manager.partition(root_id).unwrap().memory_used(), 150);

        // Decrement memory from child
        manager.update_mem_use(child_id, -30);

        // Both should be updated
        assert_eq!(manager.partition(child_id).unwrap().memory_used(), 120);
        assert_eq!(manager.partition(root_id).unwrap().memory_used(), 120);
    }

    #[test]
    fn test_gc_rollup() {
        let mut manager = GcPartitionMgr::new();

        let root_id = manager.create_partition(Some(2048), GcPartitionId::NONE);
        let child_id = manager.create_partition(Some(1024), root_id);

        // Set threshold on both partitions
        manager
            .partition_mut(root_id)
            .unwrap()
            .set_gc_threshold(100);
        manager
            .partition_mut(child_id)
            .unwrap()
            .set_gc_threshold(50);

        // Add memory to child until it triggers GC
        let _child_mem = manager.update_mem_use(child_id, 50);

        // Child should trigger GC (50 >= 50)
        assert!(manager.partition(child_id).unwrap().should_gc());
        // Root should not trigger GC yet (50 < 100)
        assert!(!manager.partition(root_id).unwrap().should_gc());

        // Add more to trigger root GC too
        let child_mem = manager.update_mem_use(child_id, 60); // Total: 110
        // Both should trigger GC now
        assert!(manager.partition(child_id).unwrap().should_gc()); // 110 >= 50
        assert!(manager.partition(root_id).unwrap().should_gc()); // 110 >= 100
        assert_eq!(child_mem, 110);
    }

    #[test]
    fn test_remove_partition_with_children() {
        let mut manager = GcPartitionMgr::new();

        let root_id = manager.create_partition(Some(2048), GcPartitionId::NONE);
        let child1_id = manager.create_partition(Some(1024), root_id);
        let child2_id = manager.create_partition(Some(512), root_id);
        let grandchild_id = manager.create_partition(Some(256), child1_id);

        // Verify all partitions exist
        assert!(manager.partition(root_id).is_some());
        assert!(manager.partition(child1_id).is_some());
        assert!(manager.partition(child2_id).is_some());
        assert!(manager.partition(grandchild_id).is_some());

        // Remove root - should remove all descendants
        manager.remove_partition(root_id);

        // All partitions should be removed
        assert!(manager.partition(root_id).is_none());
        assert!(manager.partition(child1_id).is_none());
        assert!(manager.partition(child2_id).is_none());
        assert!(manager.partition(grandchild_id).is_none());
    }

    #[test]
    fn test_memory_rollup_with_hierarchy() {
        let mut manager = GcPartitionMgr::new();

        let root_id = manager.create_partition(Some(4096), GcPartitionId::NONE);
        let child1_id = manager.create_partition(Some(2048), root_id);
        let child2_id = manager.create_partition(Some(1024), root_id);
        let grandchild_id = manager.create_partition(Some(512), child1_id);

        // Add memory to different partitions (with rollup)
        manager.update_mem_use(root_id, 100);
        manager.update_mem_use(child1_id, 50);
        manager.update_mem_use(child2_id, 30);
        manager.update_mem_use(grandchild_id, 20);

        // After rollup, each partition's memory_used includes its own and descendants'
        // Root: 100 + 50 + 30 + 20 = 200
        // Child1: 50 + 20 = 70
        // Child2: 30
        // Grandchild: 20
        assert_eq!(manager.partition(root_id).unwrap().memory_used(), 200);
        assert_eq!(manager.partition(child1_id).unwrap().memory_used(), 70);
        assert_eq!(manager.partition(child2_id).unwrap().memory_used(), 30);
        assert_eq!(manager.partition(grandchild_id).unwrap().memory_used(), 20);

        // Verify memory decreases with rollup when objects are freed
        manager.update_mem_use(grandchild_id, -10);
        assert_eq!(manager.partition(grandchild_id).unwrap().memory_used(), 10);
        assert_eq!(manager.partition(child1_id).unwrap().memory_used(), 60); // 70 - 10
        assert_eq!(manager.partition(root_id).unwrap().memory_used(), 190); // 200 - 10
    }

    #[test]
    fn test_root_of() {
        let mut manager = GcPartitionMgr::new();

        let root_id = manager.create_partition(Some(2048), GcPartitionId::NONE);
        let child_id = manager.create_partition(Some(1024), root_id);
        let grandchild_id = manager.create_partition(Some(512), child_id);

        assert_eq!(manager.root_of(root_id), root_id);
        assert_eq!(manager.root_of(child_id), root_id);
        assert_eq!(manager.root_of(grandchild_id), root_id);
    }
}
