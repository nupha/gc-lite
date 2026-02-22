// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{alloc::Layout, marker::PhantomData, ptr::NonNull};

use crate::{GcError, GcHead, GcHeap, GcNode, GcPartitionId, GcRef, unlikely, weak::GcWeakRawId};

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
    pub fn alloc<T: GcNode>(
        &mut self,
        scope: GcPartitionId,
        payload: T,
    ) -> Result<GcRef<T>, (GcError, T)> {
        match self.partition_mut(scope) {
            Some(par) => {
                let size = std::mem::size_of::<T>();
                let gross_size = std::mem::size_of::<GcHead>() + size;

                if unlikely(par.memory_limit > 0 && par.memory_used + gross_size > par.memory_limit)
                {
                    return Err((GcError::PartitionFull, payload));
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
                        std::ptr::write(head.add(1).cast::<T>().as_ptr(), payload);
                    }

                    // Add to nodes link
                    self.attach_node(scope, head);
                    // Update memory usage with rollup to parent partitions
                    self.update_mem_use(scope, gross_size as i32);

                    log::trace!("[alloc] {:?}", unsafe { head.as_ref() });

                    Ok(GcRef {
                        head_ptr: head,
                        _marker: PhantomData,
                    })
                }
            }
            None => Err((GcError::PartitionNotFound, payload)),
        }
    }

    /// Dispose a node
    pub(crate) fn dispose(&mut self, node: NonNull<GcHead>) -> usize {
        let hd = unsafe { node.as_ref() };
        log::trace!("[dispose] {hd:?}");

        #[cfg(debug_assertions)]
        {
            hd.debug_assert_node_valid(self);
            if self.dbg_dropping_root_partition.is_none() {
                debug_assert!(hd.xref().is_null(), "{hd:?}");
            }
        }

        if !hd.weak_id.is_null() {
            // clear weak slot
            let widx = hd.weak_id.index();
            debug_assert!((widx as usize) < self.weak_slots.len());
            unsafe {
                self.weak_slots.get_unchecked_mut(widx as usize).1.take();
            }
        }

        let dtype = hd.dtype() as usize;
        let info = &self.gc_types.type_info_list[dtype];
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
