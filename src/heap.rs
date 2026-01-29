// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::HashMap, marker::PhantomData, ptr::NonNull};

use crate::{
    GcError, GcResult, GcTracer,
    allocator::GcAllocator,
    node::{GcHead, GcHeadFlag, GcRef},
    partition::{GcPartitionId, GcPartitionMgr},
    trace::GcTracable,
    type_registry::TypeRegistry,
    unlikely,
};

pub struct GcHeap {
    /// Partition management
    pub(super) partitions: GcPartitionMgr,
    /// LUT: Object list heads for each partition
    pub(super) partition_heads: HashMap<GcPartitionId, Option<NonNull<GcHead>>>,
    /// LUT: Root object lists for each partition
    pub(super) partition_roots: HashMap<GcPartitionId, Vec<NonNull<GcHead>>>,
    /// Weak reference list, each slot stores (version, GcHeader)
    pub(super) weak_slots: Vec<(u16, Option<NonNull<GcHead>>)>,
    /// Type registry
    pub(super) type_registry: crate::type_registry::TypeRegistry,
}

impl Drop for GcHeap {
    fn drop(&mut self) {
        for pid in self.partitions.partition_ids() {
            if self.partitions.partition(pid).is_some() {
                self.remove_partition(pid);
            }
        }
    }
}

impl Default for GcHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl GcHeap {
    /// Create a new garbage collection heap
    pub fn new() -> Self {
        let partitions = GcPartitionMgr::new();

        Self {
            partitions,
            partition_heads: HashMap::with_capacity(8),
            partition_roots: HashMap::with_capacity(8),
            weak_slots: Vec::new(),
            type_registry: TypeRegistry::new(),
        }
    }

    /// Get garbage collection threshold for partition (bytes)
    ///
    /// A return value of 0 means automatic GC is disabled
    pub fn gc_threshold(&self, partition_id: GcPartitionId) -> Option<usize> {
        self.partitions
            .partition(partition_id)
            .map(|partition| partition.gc_threshold())
    }

    /// Set garbage collection threshold for partition (in bytes)
    ///
    /// # Parameters
    /// - `partition_id`: Partition ID
    /// - `threshold`: New garbage collection threshold (bytes)
    ///   - A value of 0 disables automatic GC
    ///   - If threshold exceeds partition memory limit, it's automatically set to the memory limit
    ///
    /// # Notes
    /// - If the partition doesn't exist, this method does nothing
    pub fn set_gc_threshold(&mut self, partition_id: GcPartitionId, threshold: usize) {
        if let Some(partition) = self.partitions.partition_mut(partition_id) {
            partition.set_gc_threshold(if threshold > 0 {
                let lim = partition.memory_limit();
                if lim > 0 && threshold > lim {
                    lim
                } else {
                    threshold
                }
            } else {
                threshold
            });
        }
    }

    /// Allocate in partition
    pub fn alloc<T: GcTracable>(
        &mut self,
        partition_id: GcPartitionId,
        data: T,
    ) -> Result<GcRef<T>, (GcError, T)> {
        match self.partitions.partition_mut(partition_id) {
            Some(par) => {
                let size = std::mem::size_of::<T>();
                let gross_size = std::mem::size_of::<GcHead>() + size;

                if unlikely(par.memory_limit > 0 && par.memory_used + gross_size > par.memory_limit)
                {
                    return Err((GcError::PartitionFull, data));
                } else {
                    // Type information
                    let type_idx = self.type_registry.register::<T>();
                    debug_assert!(type_idx != 0);

                    // Allocate memory
                    let ptr = match GcAllocator::allocate(gross_size) {
                        Some(p) => p,
                        None => {
                            return Err((GcError::AllocationFailed, data));
                        }
                    };

                    unsafe {
                        // Initialize header
                        let header_ptr = ptr.as_ptr().cast::<GcHead>();

                        (*header_ptr) = GcHead {
                            attrs: {
                                #[cfg(debug_assertions)]
                                {
                                    0xFF00_0000
                                        | ((type_idx as u32) << 8)
                                        | (GcHeadFlag::empty().union(GcHeadFlag::MAGIC_NUM).bits()
                                            as u32)
                                }
                                #[cfg(not(debug_assertions))]
                                {
                                    0xFF00_0000 | ((type_idx as u32) << 8)
                                }
                            },
                            partition: 0, // NONE
                            next: None,
                        };

                        // Initialize data
                        let data_ptr = ptr.as_ptr().add(std::mem::size_of::<GcHead>()).cast::<T>();
                        std::ptr::write(data_ptr, data);

                        debug_assert!((*header_ptr).gc_type_id() != 0);

                        // Add to partition list
                        let header = NonNull::new_unchecked(header_ptr);
                        self.attach(partition_id, header);

                        // Update memory usage with rollup to parent partitions
                        let _ = self
                            .partitions
                            .update_mem_use(partition_id, gross_size as i32);

                        Ok(GcRef {
                            head_ptr: header,
                            _marker: PhantomData,
                        })
                    }
                }
            }
            None => {
                return Err((GcError::PartitionNotFound, data));
            }
        }
    }

    /// Attach a node to partition
    #[inline]
    pub(crate) fn attach(&mut self, partition_id: GcPartitionId, node: NonNull<GcHead>) {
        debug_assert_ne!(partition_id, GcPartitionId::NONE);

        unsafe {
            debug_assert_eq!(node.as_ref().get_partition_id(), GcPartitionId::NONE);
            (*node.as_ptr()).set_partition_id(partition_id);

            let chain = self.partition_heads.entry(partition_id).or_insert(None);
            (*node.as_ptr()).next = *chain;
            *chain = Some(node);
        }
    }

    /// Remove a node from partition
    pub(crate) fn detach(&mut self, node: NonNull<GcHead>) {
        let partition_id = unsafe { node.as_ref().get_partition_id() };

        if partition_id != GcPartitionId::NONE {
            let chain = self.partition_heads.get_mut(&partition_id).unwrap();

            let mut current = *chain;
            let mut prev: Option<NonNull<GcHead>> = None;

            while let Some(header) = current {
                unsafe {
                    if header == node {
                        // take out from chain
                        if let Some(mut p) = prev {
                            p.as_mut().next = header.as_ref().next;
                        } else {
                            *chain = header.as_ref().next;
                        }

                        if node.as_ref().is_root() {
                            if let Some(roots) = self.partition_roots.get_mut(&partition_id) {
                                roots.retain(|p| node != *p);
                            }
                            (*node.as_ptr()).set_root(false);
                        }

                        // clear partition id
                        (*node.as_ptr()).partition = GcPartitionId::NONE.0 as _;

                        return;
                    }

                    prev = Some(header);
                    current = header.as_ref().next;
                }
            }

            #[cfg(debug_assertions)]
            unreachable!("node not exist");
        }
    }

    /// Set/unset partition root object status
    pub(crate) fn set_root_internal(&mut self, node: NonNull<GcHead>, is_root: bool) {
        unsafe {
            let pid = (*node.as_ptr()).get_partition_id();

            (*node.as_ptr()).set_root(is_root);

            if is_root {
                // Add to partition's root object list, create if doesn't exist
                let roots = self
                    .partition_roots
                    .entry(pid)
                    .or_insert_with(|| Vec::with_capacity(8));
                if !roots.contains(&node) {
                    roots.push(node);
                }
            } else {
                // Remove from partition's root object list
                if let Some(roots) = self.partition_roots.get_mut(&pid) {
                    if let Some(pos) = roots.iter().position(|&r| r == node) {
                        roots.swap_remove(pos);
                    }
                }
            }
        }
    }

    /// Set/unset partition root object status
    #[inline(always)]
    pub fn set_root<T>(&mut self, gc_ref: GcRef<T>, is_root: bool) {
        self.set_root_internal(gc_ref.head_ptr, is_root);
    }

    //
    // Manual Release
    //

    /// Safely manually release an object
    ///
    /// This method performs GC mark verification before release to ensure the object is not referenced by other objects.
    /// If the object is referenced, it returns an error to prevent dangling pointer issues.
    ///
    /// # 参数
    /// - `gc_ref`: 要释放的垃圾回收引用
    ///
    /// # Return Value
    /// - `Ok(())`: Release successful
    /// - `Err(GcError::InvalidReference)`: Object is not allocated from this context or is referenced by other objects
    /// - `Err(GcError::PartitionNotFound)`: The partition where the object is located does not exist
    ///
    /// # 注意
    /// - 如果对象是根对象，会先将其从根对象列表中移除
    /// - 释放后，该引用将变为无效，不应再使用
    pub fn free<T>(&mut self, gc_ref: GcRef<T>) -> GcResult<usize> {
        // Perform GC mark verification to check if object is referenced
        if self.is_node_referenced(gc_ref)? {
            Err(GcError::InvalidReference)
        } else {
            // Object is not referenced, safe to release
            unsafe { self.free_unchecked(gc_ref) }
        }
    }

    /// Unsafe quick release of an object
    ///
    /// This method does not check if the object is referenced by other objects, it releases directly.
    /// If the object is being referenced, it will cause dangling pointer and memory safety issues.
    ///
    /// # Safety
    /// The caller must ensure that no other objects reference this object, otherwise it will cause undefined behavior.
    ///
    /// # 参数
    /// - `gc_ref`: 要释放的垃圾回收引用
    ///
    /// # Return Value
    /// - `Ok(())`: Release successful
    /// - `Err(GcError::InvalidReference)`: Object is not allocated from this context
    /// - `Err(GcError::PartitionNotFound)`: The partition where the object is located does not exist
    ///
    /// # 注意
    /// - 如果对象是根对象，会先将其从根对象列表中移除
    /// - 释放后，该引用将变为无效，不应再使用
    pub unsafe fn free_unchecked<T>(&mut self, gc_ref: GcRef<T>) -> GcResult<usize> {
        let header = gc_ref.head_ptr();
        let partition_id = unsafe { header.as_ref().get_partition_id() };
        debug_assert_ne!(partition_id, GcPartitionId::NONE);

        if !self.contains(header) {
            // not allocated in this heap
            return Err(GcError::InvalidReference);
        }

        // If object is a root object, unset root
        if let Some(roots) = self.partition_roots.get_mut(&partition_id) {
            if let Some(i) = roots.iter().position(|&r| r == header) {
                roots.swap_remove(i);
            }
        }

        self.detach(header);
        unsafe { Ok(self.dispose(header)) }
    }

    /// Check if object is referenced by other objects
    fn is_node_referenced<T>(&mut self, gc_ref: GcRef<T>) -> GcResult<bool> {
        unsafe {
            let target = gc_ref.head_ptr;
            let partition_id = target.as_ref().get_partition_id();

            // Verify partition exists
            if self.partitions.partition(partition_id).is_none() {
                return Err(GcError::PartitionNotFound);
            }

            // Verify object is allocated from this context
            if !self.contains(target) {
                return Err(GcError::InvalidReference);
            }

            // Check if specified object references target object master -> slave
            let check_obj_reference =
                |master: NonNull<GcHead>, slave: NonNull<GcHead>, tracer: &mut GcTracer| -> bool {
                    // Call master's trace function to trace all objects it references
                    let payload = (master.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
                    let trace_fn = master.as_ref().get_trace_fn(&self.type_registry);
                    tracer.clear();
                    trace_fn(payload, tracer);

                    // Check if target object is included in trace results
                    tracer.pendings.iter().any(|h| *h == slave)
                };

            // Get partition list head, manually traverse to avoid borrow conflicts
            let head = match self.partition_heads.get(&partition_id) {
                Some(p) => *p,
                None => return Ok(false),
            };

            let mut referenced = false;
            let mut tracer = GcTracer::new(self, partition_id);
            let mut current = head;

            while let Some(node) = current {
                if node != target {
                    // Check if this object references target object
                    if check_obj_reference(node, target, &mut tracer) {
                        referenced = true;
                        break;
                    }
                }
                current = (*node.as_ptr()).next;
            }

            // Clear all possible marks to ensure they don't affect subsequent GC operations
            // This seems unnecessary as we directly call dispose_fn and don't set marked flags, so clearing is not needed.
            // For conservative protection, we still keep the code to clear mark flags.
            let mut current = head;
            while let Some(mut node) = current {
                node.as_mut().set_marked(false);
                current = node.as_ref().next;
            }

            Ok(referenced)
        }
    }

    /// Check if `node` was allocated in this heap
    pub fn contains(&self, node: NonNull<GcHead>) -> bool {
        self.nodes_iter(unsafe { node.as_ref().get_partition_id() })
            .any(|p| p == node)
    }

    /// Promote all objects from a partition to its parent partition
    ///
    /// This method moves all GC objects from the specified partition to its parent partition.
    /// If the partition is already a root partition (no parent), this method does nothing.
    ///
    /// # Parameters
    /// - `partition_id`: The ID of the partition whose objects should be promoted
    #[deprecated(note = "this will be removed")]
    pub fn promote_all(&mut self, partition_id: GcPartitionId) {
        match self.partition(partition_id) {
            Some(p) if p.is_root() => {}
            None => {}
            Some(p) => {
                let parent = p.parent;

                // Take source partition's chain head
                let src_head = match self
                    .partition_heads
                    .get_mut(&partition_id)
                    .and_then(Option::take)
                {
                    Some(h) => h,
                    None => return,
                };

                // 1. migrate nodes
                let mut current = Some(src_head);
                let mut last: NonNull<GcHead> = NonNull::dangling();

                while let Some(node) = current {
                    unsafe {
                        // Update the node's partition ID
                        debug_assert_eq!(node.as_ref().get_partition_id(), partition_id);
                        (*node.as_ptr()).partition = parent.0 as _;
                        last = node;
                        current = node.as_ref().next;
                    }
                }

                // Get dest partition head, create if doesn't exist
                let dest_head = self.partition_heads.entry(parent).or_insert(None);
                unsafe {
                    (*last.as_ptr()).next = *dest_head;
                }
                // Relink all nodes to parent's list
                dest_head.replace(src_head);

                // 2. migrate roots info
                if let Some(roots) = self.partition_roots.remove(&partition_id)
                    && !roots.is_empty()
                {
                    if let Some(lst) = self.partition_roots.get_mut(&parent) {
                        lst.extend_from_slice(&roots);
                    } else {
                        self.partition_roots.insert(parent, roots);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod heap_tests {
    use super::*;

    #[test]
    fn test_promote_all() {
        let mut heap = GcHeap::new();

        // Create root and child partitions
        let root_id = heap.create_root_partition(4096);
        let child_id = heap.create_sub_partition(root_id);

        // Allocate some objects in child partition
        let obj1: GcRef<i32> = heap.alloc(child_id, 42).unwrap();
        let obj2: GcRef<String> = heap.alloc(child_id, "hello".to_string()).unwrap();

        // Verify objects are in child partition
        let child_head = heap.partition_heads.get(&child_id).copied().flatten();
        assert!(child_head.is_some(), "Objects should be in child partition");

        // Verify partition ID before promote
        unsafe {
            let pid1 = obj1.head_ptr.as_ref().get_partition_id();
            let pid2 = obj2.head_ptr.as_ref().get_partition_id();
            assert_eq!(
                pid1, child_id,
                "Object1 should be in child partition before promote"
            );
            assert_eq!(
                pid2, child_id,
                "Object2 should be in child partition before promote"
            );
        }

        // Promote all objects to parent
        heap.promote_all(child_id);

        // Verify child partition is now empty
        let child_head_after = heap.partition_heads.get(&child_id).copied().flatten();
        assert!(
            child_head_after.is_none(),
            "Child partition should be empty after promote"
        );

        // Verify objects are now in root partition
        let root_head = heap.partition_heads.get(&root_id).copied().flatten();
        assert!(root_head.is_some(), "Objects should be in root partition");

        // Verify partition ID after promote
        unsafe {
            let pid1 = obj1.head_ptr.as_ref().get_partition_id();
            let pid2 = obj2.head_ptr.as_ref().get_partition_id();
            assert_eq!(
                pid1, root_id,
                "Object1 should be in root partition after promote"
            );
            assert_eq!(
                pid2, root_id,
                "Object2 should be in root partition after promote"
            );
        }
    }

    #[test]
    fn test_promote_all_root_partition() {
        let mut heap = GcHeap::new();

        // Create root partition (no parent)
        let root_id = heap.create_root_partition(4096);

        // Allocate an object in root partition
        let obj: GcRef<i32> = heap.alloc(root_id, 100).unwrap();

        // Get the head before promote
        let root_head_before = heap.partition_heads.get(&root_id).copied().flatten();
        assert!(root_head_before.is_some());

        // Verify partition ID before promote
        unsafe {
            let pid = obj.head_ptr.as_ref().get_partition_id();
            assert_eq!(pid, root_id, "Object should be in root partition");
        }

        // Promote all on root partition should do nothing
        heap.promote_all(root_id);

        // Objects should still be in root partition
        let root_head_after = heap.partition_heads.get(&root_id).copied().flatten();
        assert!(
            root_head_after.is_some(),
            "Root partition should unchanged after promote"
        );

        // Verify partition ID is unchanged after promote
        unsafe {
            let pid = obj.head_ptr.as_ref().get_partition_id();
            assert_eq!(
                pid, root_id,
                "Object should still be in root partition after promote"
            );
        }
    }

    #[test]
    fn test_promote_all_updates_partition_roots() {
        let mut heap = GcHeap::new();

        // Create root and child partitions
        let root_id = heap.create_root_partition(4096);
        let child_id = heap.create_sub_partition(root_id);

        // Allocate and mark objects as root in child partition
        let obj1: GcRef<i32> = heap.alloc(child_id, 42).unwrap();
        let obj2: GcRef<String> = heap.alloc(child_id, "hello".to_string()).unwrap();

        // Mark both as root objects
        heap.set_root(obj1, true);
        heap.set_root(obj2, true);

        // Verify root objects are in child's partition_roots
        let child_roots = heap.partition_roots.get(&child_id);
        assert!(child_roots.is_some(), "Child partition should have roots");
        assert_eq!(
            child_roots.unwrap().len(),
            2,
            "Child partition should have 2 root objects"
        );

        // Verify parent partition has no roots yet
        let parent_roots_before = heap.partition_roots.get(&root_id);
        assert!(
            parent_roots_before.is_none() || parent_roots_before.unwrap().is_empty(),
            "Parent partition should have no roots before promote"
        );

        // Promote all objects to parent
        heap.promote_all(child_id);

        // Verify child partition's roots are cleared
        let child_roots_after = heap.partition_roots.get(&child_id);
        assert!(
            child_roots_after.is_none() || child_roots_after.unwrap().is_empty(),
            "Child partition should have no roots after promote"
        );

        // Verify root objects are now in parent's partition_roots
        let parent_roots = heap.partition_roots.get(&root_id);
        assert!(
            parent_roots.is_some(),
            "Parent partition should have roots after promote"
        );
        assert_eq!(
            parent_roots.unwrap().len(),
            2,
            "Parent partition should have 2 root objects"
        );
    }
}
