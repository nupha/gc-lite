// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, ptr::NonNull};

use smallvec::SmallVec;

use crate::{GcHead, GcHeap, node::GcNodeFlag};

/// Partition ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GcPartitionId(pub u16);

impl GcPartitionId {
    const DEPTH_SHIFT: u16 = 10;
    const DEPTH_MASK: u16 = 0b11_1111;
    const SERIAL_MASK: u16 = 0x03FF;

    /// Special partition ID representing no parent (null value)
    pub const NONE: Self = Self(0);

    #[inline(always)]
    pub const fn is_null(&self) -> bool {
        self.0 == 0
    }

    #[inline(always)]
    pub const fn depth(self) -> u8 {
        ((self.0 >> Self::DEPTH_SHIFT) & Self::DEPTH_MASK) as u8
    }

    #[inline(always)]
    pub const fn serial(self) -> u16 {
        self.0 & Self::SERIAL_MASK
    }

    #[inline(always)]
    pub(crate) const fn from_depth_serial(depth: u8, serial: u16) -> Self {
        let d = (depth as u16) & Self::DEPTH_MASK;
        let s = serial & Self::SERIAL_MASK;
        Self((d << Self::DEPTH_SHIFT) | s)
    }
}

#[derive(Debug)]
pub struct GcPartition {
    /// Parent partition ID, GcPartitionId::NONE (0) means no parent (root partition)
    pub(crate) parent: GcPartitionId,
    /// Child partition IDs
    pub(crate) children: SmallVec<[GcPartitionId; 4]>,
    /// link of nodes in this partition
    pub(crate) nodes: Option<NonNull<GcHead>>,
    /// root nodes in this partition
    pub(crate) root_nodes: SmallVec<[NonNull<GcHead>; 8]>,
    /// Current memory usage
    pub(crate) memory_used: usize,
    /// Memory usage limit, 0 for unlimited
    pub(crate) memory_limit: usize,
    /// Garbage collection threshold (triggers automatic GC when memory usage reaches this byte count)
    /// A value of 0 means automatic GC is disabled
    pub(crate) gc_threshold: usize,
}

impl GcPartition {
    fn new(memory_limit: usize, parent: GcPartitionId) -> Self {
        Self {
            parent,
            children: SmallVec::new(),
            memory_used: 0,
            memory_limit,
            gc_threshold: 0, // Default threshold is 0 bytes (disable automatic GC)
            nodes: None,
            root_nodes: SmallVec::new(),
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

pub struct GcPartitionParentIter<'a> {
    heap: &'a GcHeap,
    current: GcPartitionId,
}

impl<'a> Iterator for GcPartitionParentIter<'a> {
    type Item = GcPartitionId;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.current.is_null() {
            let p = self.current;
            debug_assert!(self.heap.partition(p).is_some(), "{p:?}");
            self.current = self.heap.partition(p).unwrap().parent;
            Some(p)
        } else {
            None
        }
    }
}

impl GcHeap {
    /// Create a new partition with optional parent
    ///
    /// # Parameters
    /// - `memory_limit`: Optional memory limit (0 for unlimited)
    /// - `parent`: Parent partition ID, GcPartitionId::NONE for root partition
    ///
    /// # Returns
    /// The ID of the newly created partition
    fn create_partition(
        &mut self,
        memory_limit: Option<usize>,
        parent: GcPartitionId,
    ) -> GcPartitionId {
        const MAX_DEPTH: u8 = 63;
        const MAX_SERIAL: u16 = 1023;

        thread_local! {
            static NEXT_PARTITION_SERIAL: Cell<u16> = const { Cell::new(1) };
        }

        let depth = if parent.is_null() {
            0
        } else {
            let d = parent.depth().saturating_add(1);
            debug_assert!(d <= MAX_DEPTH);
            d
        };

        let id = NEXT_PARTITION_SERIAL.with(|next_serial| {
            let mut serial = next_serial.get();
            if serial == 0 || serial > MAX_SERIAL {
                serial = 1;
            }
            let start = serial;

            loop {
                let conflict = self.partitions.keys().any(|pid| pid.serial() == serial);
                if !conflict {
                    let next = if serial >= MAX_SERIAL { 1 } else { serial + 1 };
                    next_serial.set(next);
                    return GcPartitionId::from_depth_serial(depth, serial);
                }

                serial = if serial >= MAX_SERIAL { 1 } else { serial + 1 };
                if serial == start {
                    panic!("too many active partitions");
                }
            }
        });

        // If parent is specified, add this partition to parent's children
        if !parent.is_null()
            && let Some(parent_partition) = self.partitions.get_mut(&parent)
        {
            parent_partition.children.push(id);
        }

        let partition = GcPartition::new(memory_limit.unwrap_or(0), parent);
        self.partitions.insert(id, partition);

        log::trace!("[new_scope] {id:?} : {parent:?}");

        id
    }

    /// Create a new top-level partition (root partition) with the specified memory limit
    ///
    /// # Parameters
    /// - `memory_limit`: Memory limit in bytes (0 for unlimited)
    ///
    /// # Returns
    /// The ID of the newly created top-level partition
    pub fn create_root_partition(&mut self, memory_limit: usize) -> GcPartitionId {
        self.create_partition(Some(memory_limit), GcPartitionId::NONE)
    }

    /// Create a new sub-partition under the specified parent partition
    ///
    /// # Parameters
    /// - `parent`: Parent partition ID
    ///
    /// # Returns
    /// The ID of the newly created sub-partition
    pub fn create_sub_partition(&mut self, parent: GcPartitionId) -> GcPartitionId {
        self.create_partition(None, parent)
    }

    /// Note: `since` is not included in result vec
    fn load_descendants(&self, since: GcPartitionId, lst: &mut Vec<GcPartitionId>) {
        for &ch in self.partition(since).unwrap().children() {
            lst.push(ch);
            self.load_descendants(ch, lst);
        }
    }

    /// Remove a partition, and dispose unused nodes.
    /// For non-root partition, migrate xref nodes is optionally performed.
    pub fn remove_partition(
        &mut self,
        partition_id: GcPartitionId,
        on_migrate: impl Fn(&GcHeap, &GcHead, GcPartitionId),
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) {
        let parent_id = if let Some(par) = self.partition(partition_id) {
            par.parent()
        } else {
            return;
        };

        // if parent_id.is_null() {
        //     // drop root partition - fast mode
        //     return self.remove_root_partition_fast(partition_id, on_dispose);
        // }

        let mut scopes = Vec::<GcPartitionId>::with_capacity(64);
        scopes.push(partition_id);
        self.load_descendants(partition_id, &mut scopes);

        // remove resursivly from leaves to partition
        let call_on_migrate = !std::ptr::addr_eq(&on_migrate, &GcHeap::DUMMY_MIGRATE_CALLBACK);
        let mut freed_bytes = 0;

        while let Some(pid) = scopes.pop() {
            log::trace!("[close_scope] {pid:?}");

            // fix xref tree recursively
            if let Some(partition) = self.partitions.get_mut(&pid) {
                let roots = std::mem::take(&mut partition.root_nodes);
                for &xn in roots
                    .iter()
                    .filter(|n| unsafe { !n.as_ref().xref().is_null() })
                {
                    let xref = unsafe {
                        debug_assert_eq!(xn.as_ref().scope_id(), pid); // O.o
                        xn.as_ref().xref()
                    };

                    self.traverse_subtree(xn, GcPartitionId::NONE, {
                        let hp = NonNull::from_ref(self);

                        move |mut n, _| unsafe {
                            let xref0 = n.as_ref().xref();

                            let xref = if xref0.is_null() {
                                hp.as_ref().common_parent2(xref, n.as_ref().scope_id())
                            } else {
                                hp.as_ref()
                                    .common_parent3(xref, n.as_ref().scope_id(), xref0)
                            };
                            debug_assert!(!xref.is_null());

                            if n.as_mut().set_xref(xref) && n.as_ref().scope_id() != pid {
                                // xref was set. if node not in removing scope, mark the node as root node.
                                (*hp.as_ptr()).set_root_node(n, true);
                            }
                        }
                    });
                }
            }

            if let Some(mut partition) = self.partitions.remove(&pid) {
                if let Some(link0_head) = partition.nodes.take() {
                    // migrate xref nodes
                    let mut link1 = Some(link0_head);
                    let mut current = Some(link0_head);
                    let mut prev: Option<NonNull<GcHead>> = None;

                    while let Some(mut this) = current {
                        current = unsafe { this.as_ref().next };

                        let xref = unsafe { this.as_ref().xref() };
                        if !xref.is_null() {
                            log::trace!("[migrate] {:?} -> {xref:?}", unsafe { this.as_ref() });
                            debug_assert_ne!(xref, pid);

                            if let Some(p) = prev {
                                unsafe {
                                    (*p.as_ptr()).next = current;
                                }
                            } else {
                                link1 = current;
                            }

                            if call_on_migrate {
                                on_migrate(self, unsafe { this.as_ref() }, xref);
                            }

                            // clear flags and attach to xref chain
                            unsafe {
                                let mut f = this.as_ref().flags();
                                f.remove(
                                    GcNodeFlag::ROOT | GcNodeFlag::MARKED | GcNodeFlag::TRACED,
                                );
                                this.as_mut().set_flags(f);
                                this.as_mut().partition = 0; // clear partition & xref
                                this.as_mut().next.take();
                            }
                            self.attach(xref, this);

                            self.update_mem_use(
                                xref,
                                (self.gc_types[unsafe { this.as_ref().gc_type() } as usize].size
                                    as usize
                                    + std::mem::size_of::<GcHead>())
                                    as i32,
                            );
                        } else {
                            prev = Some(this);
                        }
                    }

                    // free rest nodes
                    if let Some(first) = link1 {
                        freed_bytes += self.dispose_all_nodes(first, &on_dispose);
                    }
                }
            }

            if let Some(parent) = self.partition_mut(parent_id) {
                // Remove from parent's children list
                parent.children.retain(|c| *c != partition_id);
            }

            // Decrease parent's memory usage
            self.update_mem_use(parent_id, -(freed_bytes as i32));

            log::trace!("[close_scope_done] {pid:?}");
        }
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

    pub fn partition_parent_iter(&self, partition_id: GcPartitionId) -> GcPartitionParentIter<'_> {
        GcPartitionParentIter {
            heap: self,
            current: self
                .partition(partition_id)
                .map_or(GcPartitionId::NONE, |p| p.parent),
        }
    }

    /// Get all partition IDs
    pub fn partition_ids(&self) -> Vec<GcPartitionId> {
        self.partitions.keys().copied().collect()
    }

    /// Check if the given partition is an ancestor of another partition
    ///
    /// # Parameters
    /// - `upper`: The partition supposed to be ancestor
    /// - `lower`: The partition supposed to be descendant
    ///
    /// # Returns
    /// `true` if `upper` is an ancestor of `lower`, `false` otherwise
    #[inline(always)]
    pub fn check_partition_ancestor(&self, upper: GcPartitionId, lower: GcPartitionId) -> bool {
        self.is_ancestor_of(lower, upper)
    }

    /// Find the nearest common parent partition of two partitions
    ///
    /// # Parameters
    /// - `p1`: First partition ID
    /// - `p2`: Second partition ID
    ///
    /// # Returns
    /// The nearest common parent partition ID, or `GcPartitionId::NONE` if no common ancestor
    pub fn common_parent2(&self, p1: GcPartitionId, p2: GcPartitionId) -> GcPartitionId {
        // Edge cases
        if p1 == GcPartitionId::NONE || p2 == GcPartitionId::NONE {
            return GcPartitionId::NONE;
        } else if p1 == p2 {
            return p1;
        }

        // Get depths
        let d1 = p1.depth() as usize;
        let d2 = p2.depth() as usize;

        // Align nodes to the same depth - the minimum
        let dmin = d1.min(d2);
        let mut a = p1;
        let mut b = p2;

        // Move deeper node up to dmin depth
        let mut da = d1;
        while da > dmin {
            match self.partition(a) {
                Some(partition) => {
                    a = partition.parent;
                    da -= 1;
                }
                None => return GcPartitionId::NONE,
            }
        }
        let mut db = d2;
        while db > dmin {
            match self.partition(b) {
                Some(partition) => {
                    b = partition.parent;
                    db -= 1;
                }
                None => return GcPartitionId::NONE,
            }
        }

        // Now both nodes are at the same depth, move up together until they meet
        while a != b {
            match (self.partition(a), self.partition(b)) {
                (Some(pa), Some(pb)) => {
                    a = pa.parent;
                    b = pb.parent;
                }
                _ => return GcPartitionId::NONE,
            }
        }

        a
    }

    /// Find the nearest common parent partition of three partitions
    ///
    /// # Parameters
    /// - `p1`: First partition ID
    /// - `p2`: Second partition ID
    /// - `p3`: Third partition ID
    ///
    /// # Returns
    /// The nearest common parent partition ID, or `GcPartitionId::NONE` if no common ancestor
    pub fn common_parent3(
        &self,
        p1: GcPartitionId,
        p2: GcPartitionId,
        p3: GcPartitionId,
    ) -> GcPartitionId {
        // Edge cases
        if p1 == GcPartitionId::NONE || p2 == GcPartitionId::NONE || p3 == GcPartitionId::NONE {
            return GcPartitionId::NONE;
        }

        // If any two are equal, reduce to two-node case
        if p1 == p2 {
            return self.common_parent2(p1, p3);
        } else if p1 == p3 || p2 == p3 {
            return self.common_parent2(p1, p2);
        }

        // Get depths
        let d1 = p1.depth() as usize;
        let d2 = p2.depth() as usize;
        let d3 = p3.depth() as usize;

        // Align nodes to the same depth - the minimum
        let dmin = d1.min(d2).min(d3);
        let mut a = p1;
        let mut b = p2;
        let mut c = p3;

        // Move nodes up to dmin depth
        let mut da = d1;
        while da > dmin {
            match self.partition(a) {
                Some(partition) => {
                    a = partition.parent;
                    da -= 1;
                }
                None => return GcPartitionId::NONE,
            }
        }
        let mut db = d2;
        while db > dmin {
            match self.partition(b) {
                Some(partition) => {
                    b = partition.parent;
                    db -= 1;
                }
                None => return GcPartitionId::NONE,
            }
        }
        let mut dc = d3;
        while dc > dmin {
            match self.partition(c) {
                Some(partition) => {
                    c = partition.parent;
                    dc -= 1;
                }
                None => return GcPartitionId::NONE,
            }
        }

        // Now all nodes are at the same depth, move up together until they meet
        while a != b || a != c {
            match (self.partition(a), self.partition(b), self.partition(c)) {
                (Some(pa), Some(pb), Some(pc)) => {
                    a = pa.parent;
                    b = pb.parent;
                    c = pc.parent;
                }
                _ => return GcPartitionId::NONE,
            }
        }

        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_creation() {
        let mut heap = GcHeap::new(&[]);
        let id = heap.create_root_partition(1024);

        let partition = heap.partition(id).unwrap();
        assert_eq!(partition.memory_limit(), 1024);
        assert_eq!(partition.gc_threshold(), 0); // Default threshold is 0, automatic GC disabled
        assert!(partition.is_root());
        assert_eq!(partition.parent(), GcPartitionId::NONE);

        // Clean up partition
        heap.remove_partition(
            id,
            |_, n, p| {
                println!("migrate {n:?} -> {p:?}");
            },
            |_, n| {
                println!("dispose: {n:?}");
            },
        );
        assert!(heap.partition(id).is_none());
    }

    #[test]
    fn test_hierarchical_partition_creation() {
        let mut heap = GcHeap::new(&[]);

        // Create root partition
        let root_id = heap.create_root_partition(1024);
        assert!(heap.partition(root_id).unwrap().is_root());

        // Create child partition
        let child_id = heap.create_sub_partition(root_id);
        let child = heap.partition(child_id).unwrap();
        assert!(!child.is_root());
        assert_eq!(child.parent(), root_id);

        // Verify root has the child in its children list
        let root = heap.partition(root_id).unwrap();
        assert_eq!(root.children().len(), 1);
        assert_eq!(root.children()[0], child_id);

        // Clean up
        heap.remove_partition(
            root_id,
            GcHeap::DUMMY_MIGRATE_CALLBACK,
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );
        assert!(heap.partition(root_id).is_none());
        assert!(heap.partition(child_id).is_none());
    }

    #[test]
    fn test_gc_threshold() {
        let mut heap = GcHeap::new(&[]);
        let id = heap.create_root_partition(1024);

        let partition = heap.partition_mut(id).unwrap();
        assert_eq!(partition.gc_threshold(), 0);

        partition.set_gc_threshold(512);
        assert_eq!(partition.gc_threshold(), 512);

        partition.set_gc_threshold(0);
        assert_eq!(partition.gc_threshold(), 0);

        // Clean up
        heap.remove_partition(
            id,
            GcHeap::DUMMY_MIGRATE_CALLBACK,
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );
    }

    #[test]
    fn test_memory_limit() {
        let mut heap = GcHeap::new(&[]);
        let id = heap.create_root_partition(1024);

        let partition = heap.partition_mut(id).unwrap();
        assert_eq!(partition.memory_limit(), 1024);

        partition.set_memory_limit(2048);
        assert_eq!(partition.memory_limit(), 2048);

        partition.set_memory_limit(0);
        assert_eq!(partition.memory_limit(), 0);

        // Clean up
        heap.remove_partition(
            id,
            GcHeap::DUMMY_MIGRATE_CALLBACK,
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );
    }

    #[test]
    fn test_is_ancestor_of() {
        let mut heap = GcHeap::new(&[]);
        let p1 = heap.create_root_partition(0);
        let p2 = heap.create_sub_partition(p1);
        let p3 = heap.create_sub_partition(p2);
        let p4 = heap.create_root_partition(0);

        assert!(heap.check_partition_ancestor(p1, p2));
        assert!(heap.check_partition_ancestor(p1, p3));
        assert!(heap.check_partition_ancestor(p2, p3));
        assert!(!heap.check_partition_ancestor(p2, p1));
        assert!(!heap.check_partition_ancestor(p3, p1));
        assert!(!heap.check_partition_ancestor(p3, p2));
        assert!(!heap.check_partition_ancestor(p1, p4));
        assert!(!heap.check_partition_ancestor(p4, p1));

        // Clean up
        heap.remove_partition(
            p1,
            GcHeap::DUMMY_MIGRATE_CALLBACK,
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );
        heap.remove_partition(
            p4,
            GcHeap::DUMMY_MIGRATE_CALLBACK,
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );
    }

    #[test]
    fn test_common_parent() {
        let mut heap = GcHeap::new(&[]);
        let p1 = heap.create_root_partition(0);
        let p2 = heap.create_sub_partition(p1);
        let p3 = heap.create_sub_partition(p1);
        let p4 = heap.create_sub_partition(p2);
        let p5 = heap.create_sub_partition(p2);
        let p6 = heap.create_sub_partition(p3);
        let p7 = heap.create_root_partition(0);

        assert_eq!(heap.common_parent2(p4, p5), p2);
        assert_eq!(heap.common_parent2(p4, p6), p1);
        assert_eq!(heap.common_parent2(p5, p6), p1);
        assert_eq!(heap.common_parent2(p2, p3), p1);
        assert_eq!(heap.common_parent2(p1, p7), GcPartitionId::NONE);

        // Clean up
        heap.remove_partition(
            p1,
            GcHeap::DUMMY_MIGRATE_CALLBACK,
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );
        heap.remove_partition(
            p7,
            GcHeap::DUMMY_MIGRATE_CALLBACK,
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );
    }

    #[test]
    fn test_update_mem_use() {
        let mut heap = GcHeap::new(&[]);
        let p1 = heap.create_root_partition(0);
        let p2 = heap.create_sub_partition(p1);
        let p3 = heap.create_sub_partition(p2);

        heap.update_mem_use(p3, 100);
        assert_eq!(heap.partition(p1).unwrap().memory_used(), 100);
        assert_eq!(heap.partition(p2).unwrap().memory_used(), 100);
        assert_eq!(heap.partition(p3).unwrap().memory_used(), 100);

        heap.update_mem_use(p2, 50);
        assert_eq!(heap.partition(p1).unwrap().memory_used(), 150);
        assert_eq!(heap.partition(p2).unwrap().memory_used(), 150);
        assert_eq!(heap.partition(p3).unwrap().memory_used(), 100);

        heap.update_mem_use(p3, -20);
        assert_eq!(heap.partition(p1).unwrap().memory_used(), 130);
        assert_eq!(heap.partition(p2).unwrap().memory_used(), 130);
        assert_eq!(heap.partition(p3).unwrap().memory_used(), 80);

        // Clean up
        heap.remove_partition(
            p1,
            GcHeap::DUMMY_MIGRATE_CALLBACK,
            GcHeap::DUMMY_DISPOSE_CALLBACK,
        );
    }

    #[test]
    fn test_common_parent_none_cases() {
        let mut heap = GcHeap::new(&[]);

        let root_id = heap.create_root_partition(2048);
        let child_id = heap.create_sub_partition(root_id);

        // NONE cases
        assert_eq!(
            heap.common_parent2(GcPartitionId::NONE, child_id),
            GcPartitionId::NONE
        );
        assert_eq!(
            heap.common_parent2(child_id, GcPartitionId::NONE),
            GcPartitionId::NONE
        );
        assert_eq!(
            heap.common_parent2(GcPartitionId::NONE, GcPartitionId::NONE),
            GcPartitionId::NONE
        );

        // Clean up - GcHeap doesn't have remove_partition, but we can let it drop
    }

    #[test]
    fn test_common_parent_different_trees() {
        let mut heap = GcHeap::new(&[]);

        // Create two separate root partitions (different trees)
        let root1_id = heap.create_root_partition(2048);
        let root2_id = heap.create_root_partition(2048);
        let child1_id = heap.create_sub_partition(root1_id);
        let child2_id = heap.create_sub_partition(root2_id);

        // Different trees should have no common parent
        assert_eq!(
            heap.common_parent2(child1_id, child2_id),
            GcPartitionId::NONE
        );
        assert_eq!(heap.common_parent2(root1_id, root2_id), GcPartitionId::NONE);

        // Clean up - GcHeap doesn't have remove_partition, but we can let it drop
    }

    #[test]
    fn test_common_parent3() {
        let mut heap = GcHeap::new(&[]);

        // Create hierarchy:
        // root_id
        //   ├── child1_id
        //   │   └── grandchild1_id
        //   ├── child2_id
        //   │   └── grandchild2_id
        //   └── child3_id
        let root_id = heap.create_root_partition(2048);
        let child1_id = heap.create_sub_partition(root_id);
        let child2_id = heap.create_sub_partition(root_id);
        let child3_id = heap.create_sub_partition(root_id);
        let grandchild1_id = heap.create_sub_partition(child1_id);
        let grandchild2_id = heap.create_sub_partition(child2_id);

        // Same partition (all three are the same)
        assert_eq!(heap.common_parent3(root_id, root_id, root_id), root_id);
        assert_eq!(
            heap.common_parent3(child1_id, child1_id, child1_id),
            child1_id
        );

        // Two same, one different
        assert_eq!(heap.common_parent3(child1_id, child1_id, root_id), root_id);
        assert_eq!(heap.common_parent3(root_id, child1_id, child1_id), root_id);
        assert_eq!(heap.common_parent3(child1_id, root_id, child1_id), root_id);

        // Three siblings - common parent is root
        assert_eq!(
            heap.common_parent3(child1_id, child2_id, child3_id),
            root_id
        );

        // Two siblings and their parent
        assert_eq!(heap.common_parent3(child1_id, child2_id, root_id), root_id);

        // Grandchildren from different subtrees
        assert_eq!(
            heap.common_parent3(grandchild1_id, grandchild2_id, child3_id),
            root_id
        );

        // One grandchild, its parent, and another child
        assert_eq!(
            heap.common_parent3(grandchild1_id, child1_id, child2_id),
            root_id
        );

        // NONE cases
        assert_eq!(
            heap.common_parent3(GcPartitionId::NONE, child1_id, child2_id),
            GcPartitionId::NONE
        );
        assert_eq!(
            heap.common_parent3(child1_id, GcPartitionId::NONE, child2_id),
            GcPartitionId::NONE
        );
        assert_eq!(
            heap.common_parent3(child1_id, child2_id, GcPartitionId::NONE),
            GcPartitionId::NONE
        );
        assert_eq!(
            heap.common_parent3(
                GcPartitionId::NONE,
                GcPartitionId::NONE,
                GcPartitionId::NONE
            ),
            GcPartitionId::NONE
        );

        // Different trees (no common ancestor)
        let root2_id = heap.create_root_partition(2048);
        let child4_id = heap.create_sub_partition(root2_id);
        assert_eq!(
            heap.common_parent3(child1_id, child2_id, child4_id),
            GcPartitionId::NONE
        );
    }

    #[test]
    fn test_common_parent3_complex_hierarchy() {
        let mut heap = GcHeap::new(&[]);

        // Create a more complex hierarchy:
        // root
        //   ├── A
        //   │   ├── A1
        //   │   │   └── A1a
        //   │   └── A2
        //   ├── B
        //   │   └── B1
        //   └── C
        let root = heap.create_root_partition(4096);
        let a = heap.create_sub_partition(root);
        let b = heap.create_sub_partition(root);
        let c = heap.create_sub_partition(root);
        let a1 = heap.create_sub_partition(a);
        let a2 = heap.create_sub_partition(a);
        let a1a = heap.create_sub_partition(a1);
        let b1 = heap.create_sub_partition(b);

        // Test cases
        // 1. Three nodes in same subtree
        assert_eq!(heap.common_parent3(a1a, a1, a), a);
        assert_eq!(heap.common_parent3(a1a, a1, a2), a);

        // 2. Nodes from different subtrees
        assert_eq!(heap.common_parent3(a1a, b1, c), root);
        assert_eq!(heap.common_parent3(a1, b, c), root);

        // 3. Mix of depths
        assert_eq!(heap.common_parent3(a1a, a2, root), root);
        assert_eq!(heap.common_parent3(a1a, b, root), root);

        // 4. One is ancestor of others
        assert_eq!(heap.common_parent3(a1a, a1, a1), a1); // two same
        assert_eq!(heap.common_parent3(a, a1, a1a), a);
    }

    #[test]
    fn test_partition_id_depth_and_serial_encoding() {
        let id = GcPartitionId::from_depth_serial(3, 10);
        assert_eq!(id.depth(), 3);
        assert_eq!(id.serial(), 10);
    }

    #[test]
    fn test_partition_depth_bits_on_creation() {
        let mut heap = GcHeap::new(&[]);
        let root = heap.create_root_partition(0);
        let child = heap.create_sub_partition(root);
        let grandchild = heap.create_sub_partition(child);

        assert_eq!(root.depth(), 0);
        assert_eq!(child.depth(), 1);
        assert_eq!(grandchild.depth(), 2);
        assert_ne!(root.serial(), 0);
        assert_ne!(child.serial(), 0);
        assert_ne!(grandchild.serial(), 0);
    }
}
