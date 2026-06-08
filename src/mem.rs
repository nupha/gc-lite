// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{alloc::Layout, marker::PhantomData, ptr::NonNull};

use crate::{
    GcError, GcHead, GcHeap, GcNode, GcPartitionId, GcRef,
    gctype::{layout_align_of, layout_size_of, payload_offset_of},
    unlikely,
    weak::GcWeakRawId,
};

impl GcHeap {
    fn mem_alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        debug_assert_ne!(layout.size(), 0, "mem_alloc: zero-sized layout");
        unsafe {
            let ptr = std::alloc::alloc(layout);

            if !ptr.is_null() {
                #[cfg(debug_assertions)]
                {
                    let n = NonNull::new_unchecked(ptr).cast::<GcHead>();
                    debug_assert!(
                        !self.dbg_living_nodes.contains(&n),
                        "node {ptr:?} already exists"
                    );
                    self.dbg_living_nodes.insert(n);
                }

                Some(NonNull::new_unchecked(ptr))
            } else {
                None
            }
        }
    }

    fn mem_dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        debug_assert_ne!(layout.size(), 0, "mem_dealloc: zero-sized layout");

        #[cfg(debug_assertions)]
        debug_assert!(
            self.dbg_living_nodes.contains(&ptr.cast()),
            "[O.o][dealloc] bad pointer {ptr:?}"
        );

        unsafe {
            #[cfg(debug_assertions)]
            self.dbg_living_nodes.remove(&ptr.cast());

            std::alloc::dealloc(ptr.as_ptr(), layout);
        }
    }

    /// Allocate a typed gc node with payload data in given scope
    unsafe fn alloc_node_mem<T: GcNode>(
        &mut self,
        partition_id: GcPartitionId,
        payload: T,
    ) -> Result<(NonNull<GcHead>, usize), (GcError, T)> {
        match self.partition_mut(partition_id) {
            Some(_) => {
                let layout =
                    match Layout::from_size_align(layout_size_of::<T>(), layout_align_of::<T>()) {
                        Ok(layout) => layout,
                        Err(_) => return Err((GcError::AllocationFailed, payload)),
                    };

                // gross_size equals `GcTypeInfo::layout_size` for this type,
                // which is set by the `gc_type_table_internal` macro via the same
                // `layout_size_of::<T>()` call. This ensures alloc/dealloc symmetry.
                let gross_size = layout.size();

                if unlikely(
                    self.memory_limit > 0
                        && self.total_memory_used + gross_size > self.memory_limit,
                ) {
                    return Err((GcError::PartitionFull, payload));
                }

                let gc_type = T::GC_TYPE_ID;
                let ptr = match self.mem_alloc(layout) {
                    Some(p) => p,
                    None => {
                        return Err((GcError::AllocationFailed, payload));
                    }
                };

                let head = ptr.cast::<GcHead>();

                // setup node info and data
                unsafe {
                    std::ptr::write(
                        ptr.add(payload_offset_of::<T>()).cast::<T>().as_ptr(),
                        payload,
                    );
                }

                let node_info = GcHead {
                    attrs: { 0xFF00_0000 | ((gc_type as u32) << 8) },
                    partition: 0,
                    weak_id: GcWeakRawId::NULL,
                    next: None,

                    #[cfg(debug_assertions)]
                    dbg_string: std::any::type_name::<T>().into(),
                };

                unsafe {
                    std::ptr::write(head.as_ptr(), node_info);
                }

                self.update_mem_use(partition_id, gross_size as i32);

                Ok((head, gross_size))
            }
            None => Err((GcError::PartitionNotFound, payload)),
        }
    }

    /// Allocate a typed gc node with payload data, do not put to any scope, even if the current scope is present.
    ///
    /// # SAFETY
    ///
    /// This function is unsafe because it directly manipulates raw pointers and memory allocation.
    /// The caller must ensure that the `partition_id` is valid and that the returned `GcRef` is
    /// properly managed to avoid memory leaks or use-after-free errors.
    pub unsafe fn alloc_raw<T: GcNode>(
        &mut self,
        partition_id: GcPartitionId,
        payload: T,
    ) -> Result<GcRef<T>, (GcError, T)> {
        unsafe {
            self.alloc_node_mem(partition_id, payload)
                .map(|(head_ptr, _)| {
                    log::trace!("[alloc] {:?}", head_ptr.as_ref());

                    self.attach_node(partition_id, head_ptr);
                    GcRef {
                        head_ptr,
                        _marker: PhantomData,
                    }
                })
        }
    }

    /// # SAFETY
    ///
    /// This function is unsafe because it directly manipulates raw pointers and memory allocation.
    /// The caller must ensure that the `partition_id` is valid and that the returned `GcRef` is
    /// properly managed to avoid memory leaks or use-after-free errors.
    pub unsafe fn alloc_root_raw<T: GcNode>(
        &mut self,
        partition_id: GcPartitionId,
        payload: T,
    ) -> Result<GcRef<T>, (GcError, T)> {
        let head_ptr = unsafe {
            let (mut h, _) = self.alloc_node_mem(partition_id, payload)?;
            h.as_mut().insert_flag(crate::node::GcNodeFlag::ROOT);
            log::trace!("[alloc_root] {:?}", h.as_ref());
            h
        };

        self.attach_node(partition_id, head_ptr);

        let par = self.partition_mut(partition_id).unwrap();
        if par.is_marking() {
            par.add_gray_node(head_ptr);
        }

        Ok(GcRef {
            head_ptr,
            _marker: PhantomData,
        })
    }

    /// Dispose a node
    pub(crate) fn dispose(&mut self, node: NonNull<GcHead>) -> usize {
        let hd = unsafe { node.as_ref() };
        log::trace!("[dispose] {hd:?}");

        #[cfg(debug_assertions)]
        hd.debug_assert_node_valid(self);

        let partition_id = hd.partition_id();

        if !hd.weak_id.is_null() {
            // clear weak slot
            let widx = hd.weak_id.index();
            debug_assert!(
                (widx as usize) < self.weak_slots.len(),
                "dispose: weak slot index {} out of bounds (len {})",
                widx,
                self.weak_slots.len(),
            );
            unsafe {
                self.weak_slots.get_unchecked_mut(widx as usize).1.take();
            }
        }

        let dtype = hd.dtype() as usize;
        let info = &self.node_dtypes.type_info_list[dtype];
        // Layout comes from `GcTypeInfo::layout_size` / `layout_align`, which are
        // set by the `gc_type_table_internal` macro via `layout_size_of::<T>()` /
        // `layout_align_of::<T>()`. This matches the Layout used at allocation time
        // in `alloc_node_mem`, ensuring alloc/dealloc symmetry.
        let layout = info.layout();
        let gross_size = layout.size();

        if let Some(f) = info.drop_fn {
            unsafe {
                f(info.payload_ptr(node).as_ptr());
            }
        }

        #[cfg(debug_assertions)]
        unsafe {
            // Poison GcHead fields so any subsequent use-after-free is caught.
            // 0xDEAD_BEEF destroys the 0xFF sentinel byte (debug_assert_node_valid_simple
            // will fail) and is non-zero so even writes that only modify low bits
            // (e.g. remove_flag) change the value, triggering malloc checksum detection.
            // Must run AFTER drop_fn so the payload Drop can still access GcHead.
            (*node.as_ptr()).attrs = 0xDEAD_BEEF;
            (*node.as_ptr()).next = None;
        }

        self.mem_dealloc(node.cast::<u8>(), layout);

        // Reclaim memory accounting for both partition and global counters.
        // Use i32::MAX as a safe upper bound; gross_size is always well below that.
        self.update_mem_use(partition_id, -(gross_size as i32));

        gross_size
    }
}
