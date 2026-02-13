// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{cell::Cell, collections::HashMap, ptr::NonNull};

use crate::{GcHead, GcHeap, node::GcNodeFlag};

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

impl GcPartitionId {
    #[inline(always)]
    pub const fn is_null(&self) -> bool {
        self.0 == 0
    }
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
    pub(crate) partitions: HashMap<GcPartitionId, GcPartition>,
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

        log::trace!("[open_scope] {id:?} : {parent:?}");

        id
    }

    /// Remove partition and all its descendants
    ///
    /// # Returns
    /// The removed partition if it existed
    #[deprecated]
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

        let removed = self.partitions.remove(&id);
        removed
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
                    par.memory_used += delta as usize;
                } else {
                    debug_assert!(par.memory_used >= (-delta) as usize);
                    par.memory_used -= (-delta) as usize;
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

    /// Check if the given partition ID is an ancestor of the specified partition
    ///
    /// # Parameters
    /// - `this`: The target partition to check
    /// - `ancestor`: The potential ancestor partition ID
    ///
    /// # Returns
    /// `true` if `ancestor` is an ancestor of `this`, `false` otherwise
    pub fn is_ancestor_of(&self, this: GcPartitionId, ancestor: GcPartitionId) -> bool {
        debug_assert_ne!(this, GcPartitionId::NONE);
        debug_assert_ne!(ancestor, GcPartitionId::NONE);

        let mut current_id = this;
        while current_id != GcPartitionId::NONE {
            if current_id == ancestor {
                return true;
            } else if let Some(p) = self.partitions.get(&current_id) {
                current_id = p.parent;
            } else {
                #[cfg(debug_assertions)]
                unreachable!();
                #[cfg(not(debug_assertions))]
                break;
            }
        }

        false
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
            self.current = self.heap.partition(p).unwrap().parent;
            Some(p)
        } else {
            None
        }
    }
}

impl GcHeap {
    /// Create a new top-level partition (root partition) with the specified memory limit
    ///
    /// # Parameters
    /// - `memory_limit`: Memory limit in bytes (0 for unlimited)
    ///
    /// # Returns
    /// The ID of the newly created top-level partition
    pub fn create_root_partition(&mut self, memory_limit: usize) -> GcPartitionId {
        let id = self
            .mgr
            .create_partition(Some(memory_limit), GcPartitionId::NONE);
        self.partition_nodes.insert(id, None);
        self.partition_root_nodes.insert(id, Vec::new());

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
        let id = self.mgr.create_partition(None, parent);
        self.partition_nodes.insert(id, None);
        self.partition_root_nodes.insert(id, Vec::new());
        id
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
            if let Some(roots) = self.partition_root_nodes.remove(&pid) {
                for xn in roots
                    .iter()
                    .filter(|n| unsafe { !n.as_ref().xref_partition().is_null() })
                {
                    let xref = unsafe {
                        debug_assert_eq!(xn.as_ref().scope_id(), pid); // O.o
                        xn.as_ref().xref_partition()
                    };

                    self.apply_recursive(*xn, GcPartitionId::NONE, {
                        let hp = NonNull::from_ref(self);

                        move |mut n, _| unsafe {
                            let xref0 = n.as_ref().xref_partition();

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

            if let Some(link0) = self.partition_nodes.remove(&pid) {
                // migrate xref nodes
                let mut link1 = link0;
                let mut current = link0;
                let mut prev: Option<NonNull<GcHead>> = None;

                while let Some(mut this) = current {
                    current = unsafe { this.as_ref().next };

                    let xref = unsafe { this.as_ref().xref_partition() };
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
                            f.remove(GcNodeFlag::ROOT | GcNodeFlag::MARKED | GcNodeFlag::TRACED);
                            this.as_mut().set_flags(f);
                            this.as_mut().partition = 0; // clear partition & xref
                            this.as_mut().next.take();
                        }
                        self.attach(xref, this);

                        // Increase memory usage with rollup to xref partitions
                        self.mgr.update_mem_use(
                            xref,
                            (Self::with_node_gc_type(this, |ty| ty.size) as usize
                                + std::mem::size_of::<GcHead>()) as i32,
                        );
                    } else {
                        prev = Some(this);
                    }
                }

                // free rest nodes
                if let Some(first) = link1 {
                    freed_bytes += self.dispose_all_nodes(first, &on_dispose);
                }

                self.mgr.partitions.remove(&pid);
            }

            if let Some(parent) = self.partition_mut(parent_id) {
                // Remove from parent's children list
                parent.children.retain(|&c| c != partition_id);
            }

            // Decrease parent's memory usage
            self.mgr.update_mem_use(parent_id, -(freed_bytes as i32));

            log::trace!("[close_scope_done] {pid:?}");
        }
    }

    /// drop root partition and all its descendants without check and fixes - the fast path.
    pub(crate) fn remove_root_partition_fast(
        &mut self,
        partition_id: GcPartitionId,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) {
        log::trace!("[remove_root_partition] {partition_id:?}");
        debug_assert!(self.partition(partition_id).unwrap().is_root());

        #[cfg(debug_assertions)]
        {
            debug_assert!(self.dbg_dropping_root_partition.is_none());
            self.dbg_dropping_root_partition = Some(partition_id);
        }

        let mut scopes = Vec::with_capacity(64);
        scopes.push(partition_id);
        self.load_descendants(partition_id, &mut scopes);
        log::debug!("[descendant_partitions] {:?}", &scopes[1..]);

        // from descendants to ancestor - using ::pop() from tail.
        while let Some(pid) = scopes.pop() {
            if let Some(link) = self.partition_nodes.remove(&pid).unwrap() {
                self.dispose_all_nodes(link, &on_dispose);
            }
            self.partition_root_nodes.remove(&pid);
            self.mgr.partitions.remove(&pid);
        }

        #[cfg(debug_assertions)]
        {
            self.dbg_dropping_root_partition = None;
        }
    }

    /// Get partition information
    pub fn partition(&self, partition_id: GcPartitionId) -> Option<&GcPartition> {
        self.mgr.partitions.get(&partition_id)
    }

    /// Get partition information
    pub fn partition_mut(&mut self, partition_id: GcPartitionId) -> Option<&mut GcPartition> {
        self.mgr.partitions.get_mut(&partition_id)
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
        self.mgr.partitions.keys().copied().collect()
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
        self.mgr.is_ancestor_of(lower, upper)
    }

    /// Get the depth of a partition (distance to root)
    fn depth(&self, id: GcPartitionId) -> usize {
        debug_assert!(self.partition(id).is_some());
        self.partition_parent_iter(id).count()
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
        let d1 = self.depth(p1);
        let d2 = self.depth(p2);

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
        } else if p1 == p3 {
            return self.common_parent2(p1, p2);
        } else if p2 == p3 {
            return self.common_parent2(p1, p2);
        }

        // Get depths
        let d1 = self.depth(p1);
        let d2 = self.depth(p2);
        let d3 = self.depth(p3);

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
        let mut heap = GcHeap::new();
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
        let mut heap = GcHeap::new();

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
        let mut manager = GcPartitionMgr::new();
        let id = manager.create_partition(Some(100), GcPartitionId::NONE);

        // Default threshold is 0, no GC triggered
        manager.update_mem_use(id, 70);
        assert!(!manager.partitions.get(&id).unwrap().should_gc());

        // Set threshold to 80 bytes
        manager
            .partitions
            .get_mut(&id)
            .unwrap()
            .set_gc_threshold(80);
        manager.update_mem_use(id, 10); // Total usage 80 bytes
        assert!(manager.partitions.get(&id).unwrap().should_gc()); // 80 >= 80

        manager.partitions.get_mut(&id).unwrap().set_gc_threshold(0);
        assert!(!manager.partitions.get(&id).unwrap().should_gc());
        assert_eq!(manager.partitions.get(&id).unwrap().gc_threshold(), 0);
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
        assert_eq!(
            manager.partitions.get(&child_id).unwrap().memory_used(),
            100
        );
        assert_eq!(manager.partitions.get(&root_id).unwrap().memory_used(), 100);

        // Add more memory to child
        manager.update_mem_use(child_id, 50);

        // Both should be updated
        assert_eq!(
            manager.partitions.get(&child_id).unwrap().memory_used(),
            150
        );
        assert_eq!(manager.partitions.get(&root_id).unwrap().memory_used(), 150);

        // Decrement memory from child
        manager.update_mem_use(child_id, -30);

        // Both should be updated
        assert_eq!(
            manager.partitions.get(&child_id).unwrap().memory_used(),
            120
        );
        assert_eq!(manager.partitions.get(&root_id).unwrap().memory_used(), 120);
    }

    #[test]
    fn test_gc_rollup() {
        let mut manager = GcPartitionMgr::new();

        let root_id = manager.create_partition(Some(2048), GcPartitionId::NONE);
        let child_id = manager.create_partition(Some(1024), root_id);

        // Set threshold on both partitions
        manager
            .partitions
            .get_mut(&root_id)
            .unwrap()
            .set_gc_threshold(100);
        manager
            .partitions
            .get_mut(&child_id)
            .unwrap()
            .set_gc_threshold(50);

        // Add memory to child until it triggers GC
        let _child_mem = manager.update_mem_use(child_id, 50);

        // Child should trigger GC (50 >= 50)
        assert!(manager.partitions.get(&child_id).unwrap().should_gc());
        // Root should not trigger GC yet (50 < 100)
        assert!(!manager.partitions.get(&root_id).unwrap().should_gc());

        // Add more to trigger root GC too
        let child_mem = manager.update_mem_use(child_id, 60); // Total: 110
        // Both should trigger GC now
        assert!(manager.partitions.get(&child_id).unwrap().should_gc()); // 110 >= 50
        assert!(manager.partitions.get(&root_id).unwrap().should_gc()); // 110 >= 100
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
        assert!(manager.partitions.get(&root_id).is_some());
        assert!(manager.partitions.get(&child1_id).is_some());
        assert!(manager.partitions.get(&child2_id).is_some());
        assert!(manager.partitions.get(&grandchild_id).is_some());

        // Remove root - should remove all descendants
        manager.remove_partition(root_id);

        // All partitions should be removed
        assert!(manager.partitions.get(&root_id).is_none());
        assert!(manager.partitions.get(&child1_id).is_none());
        assert!(manager.partitions.get(&child2_id).is_none());
        assert!(manager.partitions.get(&grandchild_id).is_none());
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
        assert_eq!(manager.partitions.get(&root_id).unwrap().memory_used(), 200);
        assert_eq!(
            manager.partitions.get(&child1_id).unwrap().memory_used(),
            70
        );
        assert_eq!(
            manager.partitions.get(&child2_id).unwrap().memory_used(),
            30
        );
        assert_eq!(
            manager
                .partitions
                .get(&grandchild_id)
                .unwrap()
                .memory_used(),
            20
        );

        // Verify memory decreases with rollup when objects are freed
        manager.update_mem_use(grandchild_id, -10);
        assert_eq!(
            manager
                .partitions
                .get(&grandchild_id)
                .unwrap()
                .memory_used(),
            10
        );
        assert_eq!(
            manager.partitions.get(&child1_id).unwrap().memory_used(),
            60
        ); // 70 - 10
        assert_eq!(manager.partitions.get(&root_id).unwrap().memory_used(), 190); // 200 - 10
    }

    #[test]
    fn test_is_ancestor_of() {
        let mut manager = GcPartitionMgr::new();

        let root_id = manager.create_partition(Some(2048), GcPartitionId::NONE);
        let child_id = manager.create_partition(Some(1024), root_id);
        let grandchild_id = manager.create_partition(Some(512), child_id);
        let sibling_id = manager.create_partition(Some(256), root_id);

        // A partition is always an ancestor of itself
        assert!(manager.is_ancestor_of(root_id, root_id));
        assert!(manager.is_ancestor_of(child_id, child_id));
        assert!(manager.is_ancestor_of(grandchild_id, grandchild_id));
        assert!(manager.is_ancestor_of(sibling_id, sibling_id));

        // Root is ancestor of everyone
        assert!(manager.is_ancestor_of(child_id, root_id));
        assert!(manager.is_ancestor_of(grandchild_id, root_id));
        assert!(manager.is_ancestor_of(sibling_id, root_id));

        // Child is ancestor of grandchild
        assert!(manager.is_ancestor_of(grandchild_id, child_id));

        // Not ancestor checks
        assert!(!manager.is_ancestor_of(root_id, child_id)); // root has no ancestor
        assert!(!manager.is_ancestor_of(child_id, grandchild_id)); // child is not ancestor of grandchild
        assert!(!manager.is_ancestor_of(child_id, sibling_id));
        assert!(!manager.is_ancestor_of(sibling_id, grandchild_id));
        assert!(!manager.is_ancestor_of(grandchild_id, sibling_id));
    }

    #[test]
    fn test_common_parent() {
        let mut heap = GcHeap::new();

        // Create hierarchy:
        // root_id
        //   ├── child1_id
        //   │   └── grandchild1_id
        //   └── child2_id
        //       └── grandchild2_id
        let root_id = heap.create_root_partition(2048);
        let child1_id = heap.create_sub_partition(root_id);
        let child2_id = heap.create_sub_partition(root_id);
        let grandchild1_id = heap.create_sub_partition(child1_id);
        let grandchild2_id = heap.create_sub_partition(child2_id);

        // Same partition
        assert_eq!(heap.common_parent2(root_id, root_id), root_id);
        assert_eq!(heap.common_parent2(child1_id, child1_id), child1_id);
        assert_eq!(
            heap.common_parent2(grandchild1_id, grandchild1_id),
            grandchild1_id
        );

        // Direct parent-child
        assert_eq!(heap.common_parent2(child1_id, root_id), root_id);
        assert_eq!(heap.common_parent2(root_id, child1_id), root_id);
        assert_eq!(heap.common_parent2(grandchild1_id, child1_id), child1_id);
        assert_eq!(heap.common_parent2(child1_id, grandchild1_id), child1_id);

        // Sibling partitions - common parent is the root
        assert_eq!(heap.common_parent2(child1_id, child2_id), root_id);
        assert_eq!(heap.common_parent2(child2_id, child1_id), root_id);

        // Grandchild from different subtrees - common parent is root
        assert_eq!(heap.common_parent2(grandchild1_id, grandchild2_id), root_id);
        assert_eq!(heap.common_parent2(grandchild2_id, grandchild1_id), root_id);

        // Grandchild and child from different subtrees
        assert_eq!(heap.common_parent2(grandchild1_id, child2_id), root_id);
        assert_eq!(heap.common_parent2(child1_id, grandchild2_id), root_id);
    }

    #[test]
    fn test_common_parent_none_cases() {
        let mut heap = GcHeap::new();

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
        let mut heap = GcHeap::new();

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
        let mut heap = GcHeap::new();

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
        let mut heap = GcHeap::new();

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
}
