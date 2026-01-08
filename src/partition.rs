// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::cell::Cell;
use std::collections::HashMap;

thread_local! {
    /// Thread-local partition ID counter (starts from 1, 0 is invalid)
    static NEXT_PARTITION_ID: Cell<u16> = Cell::new(1);
}

/// Partition ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GcPartitionId(pub u16);

/// Partition information
#[derive(Debug, Clone)]
pub struct GcPartition {
    /// Partition name
    pub(crate) name: String,
    /// Current memory usage
    pub(crate) memory_used: usize,
    /// Memory usage limit
    pub(crate) memory_limit: Option<usize>,
    /// Garbage collection threshold (triggers automatic GC when memory usage reaches this byte count)
    /// A value of 0 means automatic GC is disabled
    pub(crate) gc_threshold: usize,
}

impl GcPartition {
    pub fn new(name: String, memory_limit: Option<usize>) -> Self {
        Self {
            name,
            memory_used: 0,
            memory_limit,
            gc_threshold: 0, // Default threshold is 0 bytes (disable automatic GC)
        }
    }

    #[inline(always)]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[inline(always)]
    pub fn memory_used(&self) -> usize {
        self.memory_used
    }

    #[inline(always)]
    pub fn memory_limit(&self) -> Option<usize> {
        self.memory_limit
    }

    /// Check if garbage collection is needed
    pub fn should_gc(&self) -> bool {
        // If GC threshold > 0 and memory usage reaches threshold, trigger GC
        // gc_threshold = 0 means automatic GC is disabled
        self.gc_threshold > 0 && self.memory_used >= self.gc_threshold
    }

    /// Accumulate memory usage
    pub(crate) fn add_mem_use(&mut self, size: usize) -> bool {
        if let Some(limit) = self.memory_limit {
            if self.memory_used + size > limit {
                return false;
            }
        }
        self.memory_used += size;
        true
    }

    /// Decrement memory usage
    pub(crate) fn dec_mem_use(&mut self, size: usize) {
        debug_assert!(self.memory_used >= size);
        self.memory_used = self.memory_used.saturating_sub(size);
    }

    /// Get garbage collection threshold (bytes)
    ///
    /// A return value of 0 means automatic GC is disabled
    #[inline(always)]
    pub fn gc_threshold(&self) -> Option<usize> {
        Some(self.gc_threshold)
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
    pub fn set_gc_threshold(&mut self, threshold: usize) {
        self.gc_threshold = threshold;
    }
}

/// Partition manager (flat partitions, no hierarchical relationship)
#[derive(Debug)]
pub(crate) struct GcPartitionMgr {
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
    pub fn create_partition(&mut self, name: String, memory_limit: Option<usize>) -> GcPartitionId {
        let id = NEXT_PARTITION_ID.with(|next_id| {
            let current = next_id.get();
            next_id.set(current.wrapping_add(1));
            GcPartitionId(current)
        });

        let partition = GcPartition::new(name, memory_limit);
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

    /// Remove partition
    #[inline(always)]
    pub fn remove_partition(&mut self, id: GcPartitionId) -> Option<GcPartition> {
        self.partitions.remove(&id)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_creation() {
        let mut manager = GcPartitionMgr::new();
        let id = manager.create_partition("test".to_string(), Some(1024));

        let partition = manager.partition(id).unwrap();
        assert_eq!(partition.name, "test");
        assert_eq!(partition.memory_limit, Some(1024));
        assert_eq!(partition.gc_threshold(), Some(0)); // Default threshold is 0, automatic GC disabled

        // Clean up partition
        manager.remove_partition(id);
        assert!(manager.partition(id).is_none());
    }

    #[test]
    fn test_partition_memory_management() {
        let mut partition = GcPartition::new("test".to_string(), Some(100));

        assert!(partition.add_mem_use(50));
        assert_eq!(partition.memory_used, 50);

        assert!(!partition.add_mem_use(60)); // Exceeds limit
        assert_eq!(partition.memory_used, 50);

        partition.dec_mem_use(30);
        assert_eq!(partition.memory_used, 20);
    }

    #[test]
    fn test_gc_threshold() {
        let mut partition = GcPartition::new("test".to_string(), Some(100));

        // Default threshold is 0, no GC triggered
        partition.add_mem_use(70);
        assert!(!partition.should_gc());

        // Set threshold to 80 bytes
        partition.set_gc_threshold(80);
        partition.add_mem_use(10); // Total usage 80 bytes
        assert!(partition.should_gc()); // 80 >= 80

        partition.set_gc_threshold(0);
        assert!(!partition.should_gc());
        assert_eq!(partition.gc_threshold(), Some(0));
    }

    #[test]
    fn test_partition_manager() {
        let mut manager = GcPartitionMgr::new();

        let id1 = manager.create_partition("partition1".to_string(), Some(1024));
        let id2 = manager.create_partition("partition2".to_string(), None);

        assert!(manager.partition(id1).is_some());
        assert!(manager.partition(id2).is_some());

        manager.remove_partition(id1);
        assert!(manager.partition(id1).is_none());

        // Clean up remaining partitions
        manager.remove_partition(id2);
        assert!(manager.partition(id2).is_none());
    }
}
