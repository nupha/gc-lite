// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{alloc::Layout, marker::PhantomData, ptr::NonNull};

use crate::{
    GcError, GcHead, GcHeap, GcLocal, GcNode, GcPartitionId, GcRef, unlikely, weak::GcWeakRawId,
};

impl GcHeap {
    fn mem_alloc(&mut self, size: usize) -> Option<NonNull<u8>> {
        debug_assert_ne!(size, 0);

        let layout = Layout::from_size_align(size, std::mem::align_of::<usize>()).ok()?;
        debug_assert_eq!(layout.size(), size);

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

    fn mem_dealloc(&mut self, ptr: NonNull<u8>, gross_size: usize) {
        debug_assert_ne!(gross_size, 0);

        #[cfg(debug_assertions)]
        debug_assert!(
            self.dbg_living_nodes.contains(&ptr.cast()),
            "[O.o][dealloc] bad pointer {ptr:?}"
        );

        let ly = Layout::from_size_align(gross_size, std::mem::align_of::<usize>());

        #[cfg(debug_assertions)]
        let layout = ly.unwrap();
        #[cfg(not(debug_assertions))]
        let layout = unsafe { ly.unwrap_unchecked() };

        debug_assert_eq!(layout.size(), gross_size);

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
            Some(par) => {
                let size = std::mem::size_of::<T>();
                let gross_size = std::mem::size_of::<GcHead>() + size;

                if unlikely(par.memory_limit > 0 && par.memory_used + gross_size > par.memory_limit)
                {
                    Err((GcError::PartitionFull, payload))
                } else {
                    let gc_type = T::GC_TYPE_ID;
                    let ptr = match self.mem_alloc(gross_size) {
                        Some(p) => p,
                        None => {
                            return Err((GcError::AllocationFailed, payload));
                        }
                    };

                    let head = ptr.cast::<GcHead>();

                    // setup node info and data
                    unsafe {
                        std::ptr::write(head.add(1).cast::<T>().as_ptr(), payload);
                    }

                    let node_info = GcHead {
                        attrs: {
                            #[cfg(debug_assertions)]
                            {
                                0xFF00_0000
                                    | ((gc_type as u32) << 8)
                                    | (crate::node::GcNodeFlag::MAGIC_NUM.bits() as u32)
                            }
                            #[cfg(not(debug_assertions))]
                            {
                                0xFF00_0000 | ((gc_type as u32) << 8)
                            }
                        },
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
            }
            None => Err((GcError::PartitionNotFound, payload)),
        }
    }

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
                log::trace!("[alloc] {:?}", unsafe { head.as_ref() });

                self.attach_node(partition_id, head);
                if let Some(guard) = self.current_alloc_guard() {
                    guard.add(head);
                }

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
        match unsafe { self.alloc_node_mem(partition_id, payload) } {
            Ok((mut head, gross_size)) => {
                // Add to nodes link
                self.attach_node(partition_id, head);

                log::trace!("[alloc] {:?}", unsafe { head.as_ref() });

                if let Some(guard) = self.current_alloc_guard() {
                    guard.add(head);
                }

                // Mark as root
                unsafe { head.as_mut() }.insert_flag(crate::node::GcNodeFlag::ROOT);
                self.partition_mut(partition_id)
                    .unwrap()
                    .root_nodes
                    .push(head);

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
    /// The caller must ensure that the `partition_id` is valid and that the returned `GcLocal` is
    /// properly managed to avoid memory leaks or use-after-free errors.
    pub unsafe fn alloc_local_raw<T: GcNode>(
        &mut self,
        partition_id: GcPartitionId,
        payload: T,
    ) -> Result<GcLocal<T>, (GcError, T)> {
        match unsafe { self.alloc_raw(partition_id, payload) } {
            Ok(gc_ref) => Ok(GcLocal::new(self, gc_ref)),
            Err(e) => Err(e),
        }
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
        let gross_size = std::mem::size_of::<GcHead>() + info.size as usize;

        #[cfg(debug_assertions)]
        unsafe {
            std::ptr::drop_in_place(node.cast::<GcHead>().as_ptr());
        }

        if let Some(f) = info.drop_fn {
            unsafe {
                f(node.as_ref().payload().as_ptr());
            }
        }

        self.mem_dealloc(node.cast::<u8>(), gross_size);

        gross_size
    }
}
