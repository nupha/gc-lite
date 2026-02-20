// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::{
    GcHeap, GcPartitionId, GcTracable, GcTraceCtx, GcTraceRestrict, GcWeak, weak::GcWeakRawId,
};

bitflags::bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GcNodeFlag :u8 {
        /// is marked
        const MARKED = 1 << 0;
        /// is root node
        const ROOT = 1 << 1;
        /// node has been traced? internal use
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
    pub(crate) dbg_string: std::borrow::Cow<'static, str>,
}

impl std::fmt::Debug for GcHead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("GcNode");
        s.field("ptr", &(self as *const Self))
            .field("scope", &self.scope_id().0);

        if !self.xref().is_null() {
            s.field("xref", &self.xref().0);
        }
        if let Some(w) = self.weak() {
            s.field("weak", &format!("{}#{}", w.index(), w.version()));
        }

        #[cfg(debug_assertions)]
        {
            s.field("dbg_string", &self.dbg_string);
        }

        s.finish()
    }
}

impl GcHead {
    /// Get node gc data type id
    #[inline(always)]
    pub fn gc_type(&self) -> u8 {
        ((self.attrs & 0xFF00) >> 8) as u8
    }

    #[inline(always)]
    pub fn flags(&self) -> GcNodeFlag {
        GcNodeFlag::from_bits_retain(self.attrs as u8)
    }

    #[inline(always)]
    pub(crate) fn set_flags(&mut self, flags: GcNodeFlag) {
        #[cfg(debug_assertions)]
        debug_assert!(
            flags.contains(GcNodeFlag::MAGIC_NUM),
            "MAGIC_NUM flag is missing"
        );

        self.attrs = (self.attrs & !0xFF) | (flags.bits() as u32);
    }

    /// Check if marked
    #[inline(always)]
    pub fn is_marked(&self) -> bool {
        self.flags().contains(GcNodeFlag::MARKED)
    }

    /// Set/clear mark flag
    pub fn set_marked(&mut self, mark: bool) {
        let mut f = self.flags();
        if mark {
            f.insert(GcNodeFlag::MARKED);
        } else {
            f.remove(GcNodeFlag::MARKED);
        }
        self.set_flags(f);
    }

    /// Check if root node
    #[inline(always)]
    pub fn is_root(&self) -> bool {
        self.flags().contains(GcNodeFlag::ROOT)
    }

    /// Set/clear root object flag
    pub(super) fn set_root(&mut self, is_root: bool) {
        let mut f = self.flags();
        if is_root {
            f.insert(GcNodeFlag::ROOT);
        } else {
            f.remove(GcNodeFlag::ROOT);
        }
        self.set_flags(f);
    }

    #[inline(always)]
    pub fn is_traced(&self) -> bool {
        self.flags().contains(GcNodeFlag::TRACED)
    }

    /// Get scope of node
    #[inline(always)]
    pub fn scope_id(&self) -> GcPartitionId {
        GcPartitionId((self.partition & 0x0000_FFFF) as u16)
    }

    /// Set scope ID
    #[inline(always)]
    pub(crate) fn set_scope_id(&mut self, id: GcPartitionId) {
        debug_assert!(self.scope_id().is_null() || self.scope_id() == id);
        self.partition = (self.partition & 0xFFFF_0000) | id.0 as u32;
    }

    #[inline(always)]
    pub(crate) fn unset_scope_id(&mut self) {
        self.partition = self.partition & 0xFFFF_0000;
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

    /// Get direct referencing children nodes
    pub fn gc_children(&self, heap: &mut GcHeap) -> Vec<NonNull<GcHead>> {
        let mut ctx = GcTraceCtx::new(heap, GcTraceRestrict::No, false);
        let dtype = self.gc_type() as usize;
        let info = &ctx.heap().gc_types.type_info_list[dtype];
        (info.trace_fn)(NonNull::from_ref(self), &mut ctx);
        ctx.take_traced_nodes()
    }

    /// Get GcRef<T> from node. if node is not of type T, returns None
    pub fn gc_ref<T: GcNode>(&self) -> Option<GcRef<T>> {
        if T::GC_TYPE_ID == self.gc_type() {
            Some(GcRef::<T> {
                head_ptr: NonNull::from_ref(self),
                _marker: PhantomData,
            })
        } else {
            None
        }
    }
}

pub trait GcNode: GcTracable {
    const GC_TYPE_ID: u8;
}

/// Garbage collection reference
#[repr(transparent)]
pub struct GcRef<T: GcNode> {
    pub(super) head_ptr: NonNull<GcHead>,
    pub(super) _marker: PhantomData<T>,
}

impl<T: GcNode> Deref for GcRef<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { self.head_ptr.as_ref().payload().cast::<T>().as_ref() }
    }
}

impl<T: GcNode> DerefMut for GcRef<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.head_ptr.as_ref().payload().cast::<T>().as_mut() }
    }
}

impl<T: GcNode> Clone for GcRef<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: GcNode> Copy for GcRef<T> {}

impl<T: GcNode> PartialEq for GcRef<T> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.head_ptr == other.head_ptr
    }
}

impl<T: GcNode> Eq for GcRef<T> {}

impl<T: GcNode> From<GcRef<T>> for NonNull<GcHead> {
    #[inline(always)]
    fn from(r: GcRef<T>) -> Self {
        r.head_ptr
    }
}

impl<T: GcNode> From<&GcRef<T>> for NonNull<GcHead> {
    #[inline(always)]
    fn from(r: &GcRef<T>) -> Self {
        r.head_ptr
    }
}

impl<T: GcNode> std::fmt::Debug for GcRef<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unsafe { write!(f, "GcRef<{:?}>", self.head_ptr.as_ref()) }
    }
}

impl<T: GcNode> GcRef<T> {
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

        if T::GC_TYPE_ID == unsafe { node.as_ref().gc_type() } {
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

#[cfg(debug_assertions)]
impl GcHead {
    pub fn debug_set_dbg_string(&mut self, str: std::borrow::Cow<'static, str>) {
        self.dbg_string = str;
    }

    pub fn debug_assert_node_valid_simple(&self) {
        if !std::thread::panicking() {
            debug_assert!(
                self.flags().contains(GcNodeFlag::MAGIC_NUM)
                    && self.next.is_none_or(|n| n.is_aligned()),
                "bad node: {self:p}"
            );
        }
    }

    pub fn debug_assert_node_valid(&self, heap: &GcHeap) {
        if !std::thread::panicking() {
            debug_assert!(
                heap.dbg_living_nodes.contains(&NonNull::from_ref(self)),
                "[O.o] bad node: {self:p}"
            );
            self.debug_assert_node_valid_simple();
        }
    }

    pub fn debug_assert_node_tree_valid(&self, heap: &mut GcHeap) {
        if !std::thread::panicking() {
            let mut gcx = GcTraceCtx::new(heap, GcTraceRestrict::No, false);
            gcx.trace(NonNull::from_ref(self), |n, _| unsafe {
                n.as_ref().debug_assert_node_valid(heap);
            });
        }
    }
}
