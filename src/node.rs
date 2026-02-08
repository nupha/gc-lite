// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::{
    GcHeap, GcPartitionId, GcTracable, GcTraceRestrict, GcTracer, GcWeak, weak::GcWeakRawId,
};

bitflags::bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GcHeadFlag :u8 {
        /// is marked
        const MARKED = 1 << 0;
        /// is root node
        const ROOT = 1 << 1;

        /// internal use, denotes a node has been traced.
        const TRACED = 1 << 2;

        #[cfg(debug_assertions)]
        const MAGIC_NUM = 1 << 7;
    }
}

/// GC node info
pub struct GcHead {
    /// Attributes of node:
    /// * bit 8-15:  gc datatype id
    /// * bit 0-7:   flags
    pub(super) attrs: u32,

    /// XRef partition id (16bit) + Partition id (16bit)
    pub(super) partition: u32,

    pub(super) weak_id: GcWeakRawId,

    /// Pointer to next object (for list traversal)
    pub(super) next: Option<NonNull<GcHead>>,

    #[cfg(debug_assertions)]
    pub(crate) dbg_id: usize,
    #[cfg(debug_assertions)]
    pub(crate) dbg_type_name: &'static str,
    #[cfg(debug_assertions)]
    pub(crate) dbg_heap: NonNull<GcHeap>,
}

impl std::fmt::Debug for GcHead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("GcHead");

        s.field("ptr", &(self as *const Self))
            .field("scope", &self.get_partition_id().0)
            //.field("dtype", &self.gc_dtype())
            //.field("flags", &self.flags())
            .field("xref", &self.xref_partition().0);

        if let Some(w) = self.weak() {
            s.field("weak", &format!("{}#{}", w.index(), w.version()));
        }

        #[cfg(debug_assertions)]
        {
            s.field("type_name", &self.dbg_type_name)
                .field("id", &format!("0x{:x}", self.dbg_id));
        }

        s.finish()
    }
}

impl GcHead {
    /// Get node gc data type id
    #[inline(always)]
    pub fn gc_dtype(&self) -> u8 {
        ((self.attrs & 0xFF00) >> 8) as u8
    }

    #[inline(always)]
    pub fn flags(&self) -> GcHeadFlag {
        GcHeadFlag::from_bits_retain(self.attrs as u8)
    }

    #[inline(always)]
    pub(crate) fn set_flags(&mut self, flags: GcHeadFlag) {
        #[cfg(debug_assertions)]
        debug_assert!(
            flags.contains(GcHeadFlag::MAGIC_NUM),
            "MAGIC_NUM flag is missing"
        );

        self.attrs = (self.attrs & !0xFF) | (flags.bits() as u32);
    }

    /// Check if marked
    #[inline(always)]
    pub fn is_marked(&self) -> bool {
        self.flags().contains(GcHeadFlag::MARKED)
    }

    /// Set/clear mark flag
    pub fn set_marked(&mut self, mark: bool) {
        let mut f = self.flags();
        if mark {
            f.insert(GcHeadFlag::MARKED);
        } else {
            f.remove(GcHeadFlag::MARKED);
        }
        self.set_flags(f);
    }

    /// Check if root node
    #[inline(always)]
    pub fn is_root(&self) -> bool {
        self.flags().contains(GcHeadFlag::ROOT)
    }

    /// Set/clear root object flag
    pub(super) fn set_root(&mut self, is_root: bool) {
        let mut f = self.flags();
        if is_root {
            f.insert(GcHeadFlag::ROOT);
        } else {
            f.remove(GcHeadFlag::ROOT);
        }
        self.set_flags(f);
    }

    /// Get partition ID
    #[inline(always)]
    pub fn get_partition_id(&self) -> GcPartitionId {
        GcPartitionId((self.partition & 0x0000_FFFF) as u16)
    }

    /// Set partition ID
    #[inline(always)]
    pub(crate) fn set_partition_id(&mut self, id: GcPartitionId) {
        debug_assert!(self.get_partition_id().is_null() || self.get_partition_id() == id);
        self.partition = (self.partition & 0xFFFF_0000) | id.0 as u32;
    }

    /// get node weakref info
    pub fn weak(&self) -> Option<GcWeakRawId> {
        if self.weak_id.is_null() {
            None
        } else {
            Some(self.weak_id)
        }
    }

    /// get raw pointer to payload data
    #[inline(always)]
    pub fn payload(&self) -> NonNull<u8> {
        #[cfg(debug_assertions)]
        self.debug_assert_node_valid_simple();

        unsafe { NonNull::from_ref(self).add(1).cast::<u8>() }
    }

    /// Get one-depth direct children of `self` node
    pub fn children(&self, heap: &GcHeap, restrict: GcTraceRestrict) -> Vec<NonNull<GcHead>> {
        let node = NonNull::from_ref(self);

        let mut tr = GcTracer::new(heap, restrict, false);
        (heap.get_node_gc_type(node).trace_fn)(node, tr.ctx());

        let mut lst = tr.take_traced_nodes();
        lst.retain(|&x| x != node); // remove self reference
        lst.into()
    }
}

#[cfg(debug_assertions)]
impl GcHead {
    pub fn debug_assert_node_valid_simple(&self) {
        debug_assert_eq!(self as *const Self as usize, self.dbg_id);
        debug_assert!(
            self.gc_dtype() != 0
                && self.flags().contains(GcHeadFlag::MAGIC_NUM)
                && self.next.is_none_or(|n| n.is_aligned()),
            "bad node: {self:?}"
        )
    }

    pub fn debug_assert_node_valid(&self, heap: &GcHeap) {
        debug_assert_eq!(self.dbg_heap, NonNull::from_ref(heap));
        debug_assert!(
            unsafe {
                self.dbg_heap
                    .as_ref()
                    .debug_living_nodes
                    .contains(&NonNull::from_ref(self))
            },
            "unknown node: {self:?}"
        );
        self.debug_assert_node_valid_simple();
    }

    pub fn debug_assert_node_tree_valid(&self, heap: &GcHeap) {
        debug_assert_eq!(self.dbg_heap, NonNull::from_ref(heap));
        let mut tr = heap.tracer(GcTraceRestrict::No);
        tr.trace(NonNull::from_ref(self), |n, _| unsafe {
            n.as_ref().debug_assert_node_valid(heap);
            true
        });
    }
}

/// Garbage collection reference
#[repr(transparent)]
pub struct GcRef<T: GcTracable> {
    pub(super) head_ptr: NonNull<GcHead>,
    pub(super) _marker: PhantomData<T>,
}

impl<T: GcTracable> Deref for GcRef<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { self.head_ptr.as_ref().payload().cast::<T>().as_ref() }
    }
}

impl<T: GcTracable> DerefMut for GcRef<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.head_ptr.as_ref().payload().cast::<T>().as_mut() }
    }
}

impl<T: GcTracable> Clone for GcRef<T> {
    fn clone(&self) -> Self {
        Self {
            head_ptr: self.head_ptr,
            _marker: PhantomData,
        }
    }
}

impl<T: GcTracable> Copy for GcRef<T> {}

impl<T: GcTracable> PartialEq for GcRef<T> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.head_ptr == other.head_ptr
    }
}

impl<T: GcTracable> Eq for GcRef<T> {}

impl<T: GcTracable> From<GcRef<T>> for NonNull<GcHead> {
    #[inline(always)]
    fn from(r: GcRef<T>) -> Self {
        r.head_ptr
    }
}

impl<T: GcTracable> From<&GcRef<T>> for NonNull<GcHead> {
    #[inline(always)]
    fn from(r: &GcRef<T>) -> Self {
        r.head_ptr
    }
}

impl<T: GcTracable> std::fmt::Debug for GcRef<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unsafe { write!(f, "GcRef<{:?}>", self.head_ptr.as_ref()) }
    }
}

impl<T: GcTracable> GcRef<T> {
    /// Create GcRef<T> from &T reference
    ///
    /// This method verifies that the passed reference comes from a valid GC object.
    /// It ensures safety by checking if the corresponding GcHead is in the GC context.
    ///
    /// # Parameters
    /// - `data_ref`: Reference to convert, must come from valid GcRef object
    ///
    /// # Return Value
    /// - `Some(GcRef<T>)`: If reference comes from valid GC object
    /// - `None`: If reference is not from GC object or object is invalid
    ///
    /// # Safety
    /// Caller must ensure the passed reference indeed comes from a valid GcRef object.
    pub fn try_from_ref(heap: &GcHeap, data_ref: &T) -> Option<Self> {
        let node = unsafe {
            NonNull::from_ref(data_ref)
                .cast::<u8>()
                .sub(std::mem::size_of::<GcHead>())
                .cast::<GcHead>()
        };

        if let Some(dtype_id) = heap.type_id_of::<T>()
            && dtype_id == unsafe { node.as_ref().gc_dtype() }
        {
            #[cfg(debug_assertions)]
            unsafe {
                node.as_ref().debug_assert_node_valid(heap);
            }

            Some(Self {
                head_ptr: node,
                _marker: PhantomData,
            })
        } else {
            None
        }
    }

    /// Unsafe conversion from &T to GcRef<T>, main focus on speed.
    ///
    /// # Safety
    ///
    /// Caller must ensure &T comes from GcRef<T>, otherwise consequences are unpredictable.
    #[inline]
    pub unsafe fn from_ref_unchecked(data_ref: &T) -> Self {
        let node = unsafe { NonNull::from_ref(data_ref).cast::<GcHead>().sub(1) };

        #[cfg(debug_assertions)]
        unsafe {
            node.as_ref().debug_assert_node_valid_simple();
        }

        Self {
            head_ptr: node,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn as_ptr(&self) -> NonNull<T> {
        unsafe { self.head_ptr.as_ref().payload().cast::<T>() }
    }

    #[inline(always)]
    pub fn downgrade(&self, heap: &mut GcHeap) -> GcWeak<T> {
        heap.downgrade(self)
    }

    /// check if this is root object
    #[inline(always)]
    pub fn is_root(&self) -> bool {
        unsafe { self.head_ptr.as_ref().is_root() }
    }

    /// get node raw pointer
    #[inline(always)]
    pub fn node_ptr(&self) -> NonNull<GcHead> {
        self.head_ptr
    }

    /// get node info
    #[inline(always)]
    pub fn node_info(&self) -> &GcHead {
        unsafe { self.head_ptr.as_ref() }
    }

    /// get node info mut
    #[inline(always)]
    pub fn node_info_mut(&mut self) -> &mut GcHead {
        unsafe { self.head_ptr.as_mut() }
    }
}

/// Garbage collection pointer wrapper (lifetime bound to GcContext)
pub struct Gc<'heap, T: GcTracable> {
    inner: GcRef<T>,
    _marker: std::marker::PhantomData<&'heap ()>,
}

impl<'heap, T: GcTracable> Deref for Gc<'heap, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl<'heap, T: GcTracable> DerefMut for Gc<'heap, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.deref_mut()
    }
}

impl<'heap, T: GcTracable> Clone for Gc<'heap, T> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<'heap, T: GcTracable> std::fmt::Debug for Gc<'heap, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gc({:p}:{:p})", self.inner.head_ptr, self.deref())
    }
}

impl<'heap, T: GcTracable> Gc<'heap, T> {
    /// Create new GC object (specify partition)
    pub fn new_in_partition(
        heap: &'heap mut crate::GcHeap,
        partition_id: crate::partition::GcPartitionId,
        value: T,
    ) -> crate::GcResult<Self> {
        match heap.alloc(partition_id, value) {
            Ok(inner) => Ok(Self {
                inner,
                _marker: std::marker::PhantomData,
            }),
            Err((err, _)) => Err(err),
        }
    }

    /// Get internal GC reference
    #[inline(always)]
    pub fn gc_ref(&self) -> GcRef<T> {
        self.inner
    }

    /// Set/unset root object status
    #[inline(always)]
    pub fn set_root(&self, heap: &mut crate::GcHeap, is_root: bool) {
        heap.set_root(self.inner, is_root);
    }

    #[inline(always)]
    pub fn is_root(&self) -> bool {
        self.inner.is_root()
    }
}
