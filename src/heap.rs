// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::HashMap, marker::PhantomData, ptr::NonNull};

use crate::{
    GcError, GcResult, GcTracer,
    allocator::Allocator,
    node::{GcHead, GcRef},
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
    /// Weak reference list, each slot stores (version, CompactGcHeader)
    pub(super) weak_list: Vec<(u32, Option<NonNull<GcHead>>)>,
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
            weak_list: Vec::new(),
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

    /// Remove partition
    pub fn remove_partition(&mut self, partition_id: GcPartitionId) {
        if self.partitions.partition(partition_id).is_some() {
            // Perform garbage collection first to clean up unreachable objects
            let _ = self.collect_garbage(partition_id);

            self.partition_roots.remove(&partition_id);
            let _ = self.sweep(partition_id);

            debug_assert!(
                self.partition_heads
                    .get(&partition_id)
                    .copied()
                    .flatten()
                    .is_none(),
                "shouldn't have live nodes"
            );

            self.partitions.remove_partition(partition_id);
            self.partition_heads.remove(&partition_id);
        }
    }

    //
    // Object Allocation
    //

    /// Allocate a new object in partition
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
                    let ptr = match Allocator::allocate(gross_size) {
                        Some(p) => p,
                        None => {
                            return Err((GcError::AllocationFailed, data));
                        }
                    };

                    unsafe {
                        // Initialize header
                        let header_ptr = ptr.as_ptr().cast::<GcHead>();

                        (*header_ptr) = GcHead {
                            flags: 0, // marked=false, root=false
                            type_partition: ((partition_id.0 as u32) << 16) | (type_idx as u32),
                            weak_ref_index: 0xFFFF, // no weak ref
                            next: None,
                        };

                        #[cfg(debug_assertions)]
                        {
                            (*header_ptr).flags = super::node::GC_HEAD_MAGIC;
                        }

                        // Initialize data
                        let data_ptr = ptr.as_ptr().add(std::mem::size_of::<GcHead>()).cast::<T>();
                        std::ptr::write(data_ptr, data);

                        debug_assert!((*header_ptr).type_id() != 0);

                        // Add to partition list
                        let header = NonNull::new_unchecked(header_ptr);
                        self.add_to_partition_list(partition_id, header);

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

    /// Add object to partition list
    #[inline(always)]
    fn add_to_partition_list(&mut self, partition_id: GcPartitionId, node: NonNull<GcHead>) {
        let head = self.partition_heads.entry(partition_id).or_insert(None);
        unsafe {
            (*node.as_ptr()).next = *head;
            *head = Some(node);
        }
    }

    /// Set/unset partition root object status
    pub fn set_root<T>(&mut self, gc_ref: GcRef<T>, is_root: bool) {
        unsafe {
            let mut header = gc_ref.head_ptr;
            let partition_id = (*header.as_ptr()).get_partition_id();

            // 设置/清除根对象标记
            header.as_mut().set_root(is_root);

            if is_root {
                // Add to partition's root object list, create if doesn't exist
                let roots = self
                    .partition_roots
                    .entry(partition_id)
                    .or_insert_with(|| Vec::with_capacity(8));
                if !roots.contains(&header) {
                    roots.push(header);
                }
            } else {
                // Remove from partition's root object list
                if let Some(roots) = self.partition_roots.get_mut(&partition_id) {
                    if let Some(pos) = roots.iter().position(|&r| r == header) {
                        roots.swap_remove(pos);
                    }
                }
            }
        }
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
        let header = gc_ref.head_ptr;
        let partition_id = unsafe { header.as_ref().get_partition_id() };

        // 验证分区是否存在
        if self.partitions.partition(partition_id).is_none() {
            return Err(GcError::PartitionNotFound);
        }

        // 验证对象是否来自这个上下文的分配
        if !self.contains_internal(header) {
            return Err(GcError::InvalidReference);
        }

        // If object is a root object, remove root object mark first
        if let Some(roots) = self.partition_roots.get_mut(&partition_id) {
            if let Some(i) = roots.iter().position(|&r| r == header) {
                roots.swap_remove(i);
            }
        }

        // Remove object from partition list
        self.remove_from_partition_list(partition_id, header)?;

        unsafe { Ok(self.release_node(header)) }
    }

    //
    // Other
    //

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
            if !self.contains_internal(target) {
                return Err(GcError::InvalidReference);
            }

            let mut referenced = false;
            let mut tracer = GcTracer::new();

            // Check if specified object references target object source -> target
            let check_obj_reference = |source: NonNull<GcHead>,
                                       target: NonNull<GcHead>,
                                       tracer: &mut crate::trace::GcTracer|
             -> bool {
                // Call source object's trace function to trace all objects it references
                let payload_ptr = (source.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
                let trace_fn = source.as_ref().get_trace_fn(&self.type_registry);
                tracer.clear();
                trace_fn(payload_ptr, tracer);

                // Check if target object is included in trace results
                tracer.mark_cache.iter().any(|h| *h == target)
            };

            // Get partition list head, manually traverse to avoid borrow conflicts
            let head = match self.partition_heads.get(&partition_id) {
                Some(p) => *p,
                None => return Ok(false),
            };

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

    /// Check if GcRef is allocated from this context
    ///
    /// # Parameters
    /// - `gc_ref`: Garbage collection reference to check
    ///
    /// # Return Value
    /// - `true`: If the reference is allocated by this GcHeap
    /// - `false`: If the reference is not allocated by this GcHeap
    ///
    /// # Notes
    /// This method only checks if the reference is from the current context, it does not check if the reference is still valid
    #[inline(always)]
    pub fn contains<T>(&self, gc_ref: &GcRef<T>) -> bool {
        self.contains_internal(gc_ref.head_ptr)
    }

    /// Verify object is allocated from this context
    fn contains_internal(&self, header: NonNull<GcHead>) -> bool {
        unsafe {
            let partition_id = header.as_ref().get_partition_id();

            // Check if partition exists
            if self.partitions.partition(partition_id).is_none() {
                return false;
            }

            // Check if object is in partition's list
            if let Some(head) = self.partition_heads.get(&partition_id) {
                let mut current = *head;
                while let Some(node) = current {
                    if node == header {
                        return true;
                    }
                    current = (*node.as_ptr()).next;
                }
            }

            false
        }
    }

    /// Remove specified object from partition list
    fn remove_from_partition_list(
        &mut self,
        partition_id: GcPartitionId,
        target: NonNull<GcHead>,
    ) -> GcResult<()> {
        let head = self
            .partition_heads
            .get_mut(&partition_id)
            .ok_or(GcError::PartitionNotFound)?;

        let mut current = *head;
        let mut prev: Option<NonNull<GcHead>> = None;

        unsafe {
            while let Some(header) = current {
                if header == target {
                    // Found target object, remove from list
                    if let Some(prev_header) = prev {
                        (*prev_header.as_ptr()).next = (*header.as_ptr()).next;
                    } else {
                        *head = (*header.as_ptr()).next;
                    }
                    return Ok(());
                }

                prev = Some(header);
                current = (*header.as_ptr()).next;
            }
        }

        // If object not found, return error
        Err(GcError::InvalidReference)
    }
}

/// Generic trace function, used to call trace method of specific type
pub(super) unsafe fn trace_fn<T: GcTracable>(data_ptr: *mut u8, tracer: &mut crate::GcTracer) {
    let typed_ptr = data_ptr as *const T;
    let typed_ref: &T = unsafe { &*typed_ptr };
    typed_ref.trace(tracer);
}

/// Generic dispose function, used to call drop_in_place of specific type
pub(super) unsafe fn dispose_fn<T>(data_ptr: *mut u8) {
    let typed_ptr = data_ptr as *mut T;
    unsafe { std::ptr::drop_in_place(typed_ptr) };
}

/// Empty dispose function, for types that don't need Drop
pub(super) unsafe fn noop_dispose_fn(_data_ptr: *mut u8) {
    // Do nothing
}
