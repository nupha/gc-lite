// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{alloc::Layout, marker::PhantomData, ptr::NonNull};

use crate::{
    GcError, GcHead, GcHeap, GcPartitionId, GcRef, GcTracable, unlikely, weak::GcWeakRawId,
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
                        !self.debug_living_nodes.contains(&n),
                        "node {ptr:?} already exists?"
                    );
                    self.debug_living_nodes.insert(n);
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
        {
            debug_assert!(
                self.debug_living_nodes.contains(&ptr.cast()),
                "[O.o] node {ptr:?} has been disposed?"
            );
        }

        let ly = Layout::from_size_align(gross_size, std::mem::align_of::<usize>());

        #[cfg(debug_assertions)]
        let layout = ly.unwrap();
        #[cfg(not(debug_assertions))]
        let layout = unsafe { ly.unwrap_unchecked() };

        debug_assert_eq!(layout.size(), gross_size);

        unsafe {
            #[cfg(debug_assertions)]
            self.debug_living_nodes.remove(&ptr.cast());

            std::alloc::dealloc(ptr.as_ptr(), layout);
        }
    }

    /// Allocate a GcRef with payload data in given partition
    pub fn alloc<T: GcTracable>(
        &mut self,
        partition_id: GcPartitionId,
        payload: T,
    ) -> Result<GcRef<T>, (GcError, T)> {
        match self.partition_mut(partition_id) {
            Some(par) => {
                let size = std::mem::size_of::<T>();
                let gross_size = std::mem::size_of::<GcHead>() + size;

                if unlikely(par.memory_limit > 0 && par.memory_used + gross_size > par.memory_limit)
                {
                    return Err((GcError::PartitionFull, payload));
                } else {
                    let gc_dtype = self.gc_data_types.register::<T>(0);

                    let ptr = match self.mem_alloc(gross_size) {
                        Some(p) => p,
                        None => {
                            return Err((GcError::AllocationFailed, payload));
                        }
                    };

                    // O.o SLOW DEBUG
                    #[cfg(debug_assertions)]
                    {
                        let mut tr = crate::GcTracer::new(self, GcPartitionId::NONE, true);
                        for &link in self.partition_nodes.values() {
                            if let Some(first) = link {
                                tr.trace(first, |n, _| {
                                    debug_assert!(
                                        n != ptr.cast(),
                                        "[O.o] node ptr {ptr:?} conflict in reference"
                                    );
                                    true
                                });
                            }
                        }

                        // // trace payload to detect descendant nodes reference
                        // let mut tr = crate::GcTracer::new(self, GcPartitionId::NONE, false);
                        // payload.trace(tr.ctx());
                        // while let Some(mut n) = tr.traced_nodes.pop_front() {
                        //     unsafe {
                        //         debug_assert!(
                        //             n.as_ref().has_check_ref(),
                        //             "alloc:{partition_id:?}, ref:{:?}, node: {:?}",
                        //             n.as_ref().get_partition_id(),
                        //             n.as_ref()
                        //         );

                        //         n.as_mut().set_check_ref(false);
                        //     }
                        // }
                    }

                    let head = ptr.cast::<GcHead>();

                    // setup node info and data
                    let node_info = GcHead {
                        attrs: {
                            #[cfg(debug_assertions)]
                            {
                                0xFF00_0000
                                    | ((gc_dtype as u32) << 8)
                                    | (crate::node::GcHeadFlag::MAGIC_NUM.bits() as u32)
                            }
                            #[cfg(not(debug_assertions))]
                            {
                                0xFF00_0000 | ((gc_dtype as u32) << 8)
                            }
                        },
                        partition: 0,
                        weak_id: GcWeakRawId::NULL,
                        next: None,

                        #[cfg(debug_assertions)]
                        alloc_in: partition_id,
                        #[cfg(debug_assertions)]
                        type_name: std::any::type_name::<T>(),
                    };

                    unsafe {
                        std::ptr::write(head.as_ptr(), node_info);
                        std::ptr::write(head.add(1).cast::<T>().as_ptr(), payload);
                    }

                    // Add to partition list
                    self.attach(partition_id, head);
                    // Update memory usage with rollup to parent partitions
                    self.mgr.update_mem_use(partition_id, gross_size as i32);

                    log::trace!("[alloc] {:?}", unsafe { head.as_ref() });

                    Ok(GcRef {
                        head_ptr: head,
                        _marker: PhantomData,
                    })
                }
            }
            None => {
                return Err((GcError::PartitionNotFound, payload));
            }
        }
    }

    /// Dispose one node
    pub(crate) fn dispose(&mut self, node: NonNull<GcHead>) -> usize {
        let hd = unsafe { node.as_ref() };
        log::trace!("[dispose] {hd:?}");

        #[cfg(debug_assertions)]
        debug_assert!(hd.test_valid());

        if !hd.weak_id.is_null() {
            // clear weak slot
            let widx = hd.weak_id.index();
            debug_assert!((widx as usize) < self.weak_slots.len());
            unsafe {
                self.weak_slots.get_unchecked_mut(widx as usize).1.take();
            }
        }

        let ty = self.get_node_gc_type(node);
        let gross_size = std::mem::size_of::<GcHead>() + ty.size as usize;

        if let Some(f) = ty.drop_fn {
            unsafe {
                f(node.as_ref().payload().as_ptr());
            }
        }

        #[cfg(debug_assertions)]
        unsafe {
            // clear head info
            (*node.as_ptr()).attrs = 0;
            (*node.as_ptr()).next.take();
        }

        self.mem_dealloc(node.cast::<u8>(), gross_size);

        gross_size
    }
}
