// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{alloc::Layout, marker::PhantomData, ptr::NonNull};

use crate::gctype::{layout_align_of, layout_from_type_info, layout_size_of, payload_offset_of};
use crate::{GcError, GcHead, GcHeap, GcNode, GcPartitionId, GcRef, unlikely, weak::GcWeakRawId};

impl GcHeap {
    fn mem_alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        debug_assert_ne!(layout.size(), 0);
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
        debug_assert_ne!(layout.size(), 0);

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
        match unsafe { self.alloc_node_mem(partition_id, payload) } {
            Ok((head, _)) => {
                self.attach_node(partition_id, head);

                log::trace!("[alloc] {:?}", unsafe { head.as_ref() });

                Ok(GcRef {
                    head_ptr: head,
                    _marker: PhantomData,
                })
            }
            Err(e) => Err(e),
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
        let (mut node, _) = unsafe { self.alloc_node_mem(partition_id, payload)? };

        unsafe { node.as_mut() }.insert_flag(crate::node::GcNodeFlag::ROOT);

        self.attach_node(partition_id, node);

        let par = self.partition_mut(partition_id).unwrap();
        if par.is_marking() {
            par.add_gray_node(node);
        }

        log::trace!("[alloc_root] {:?}", unsafe { node.as_ref() });

        Ok(GcRef {
            head_ptr: node,
            _marker: PhantomData,
        })
    }

    /// Dispose a node
    pub(crate) fn dispose(&mut self, node: NonNull<GcHead>) -> usize {
        let hd = unsafe { node.as_ref() };
        log::trace!("[dispose] {hd:?}");

        #[cfg(debug_assertions)]
        hd.debug_assert_node_valid(self);

        if !hd.weak_id.is_null() {
            // clear weak slot
            let widx = hd.weak_id.index();
            debug_assert!((widx as usize) < self.weak_slots.len());
            unsafe {
                self.weak_slots.get_unchecked_mut(widx as usize).1.take();
            }
        }

        let dtype = hd.dtype() as usize;
        let info = &self.node_dtypes.type_info_list[dtype];
        let layout = layout_from_type_info(info);
        let gross_size = layout.size();

        #[cfg(debug_assertions)]
        unsafe {
            std::ptr::drop_in_place(node.cast::<GcHead>().as_ptr());
        }

        if let Some(f) = info.drop_fn {
            unsafe {
                f(info.payload_ptr(node).as_ptr());
            }
        }

        self.mem_dealloc(node.cast::<u8>(), layout);

        gross_size
    }
}
