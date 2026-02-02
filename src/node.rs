// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::{
    GcHeap, GcPartitionId, GcTracable,
    trace::GcTraceOp,
    type_registry::{TypeRegistry, dispose_fn, trace_fn},
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
        const TRACE_DONE = 1 << 2;

        #[cfg(debug_assertions)]
        const MAGIC_NUM = 1 << 7;
    }
}

/// GC node head info
#[repr(C)]
pub struct GcHead {
    /// Attributes of node:
    /// * bit 24-31: weak reference index, u8::MAX means none
    /// * bit 8-16:  type id
    /// * bit 0-7:   flags
    pub(super) attrs: u32,

    /// XRef partition id (16bit) + Partition id (16bit)
    pub(super) partition: u32,

    /// Pointer to next object (for list traversal)
    pub(super) next: Option<NonNull<GcHead>>,
}

impl GcHead {
    /// Get node gc type id
    #[inline(always)]
    pub fn gc_type_id(&self) -> u8 {
        ((self.attrs & 0xFF00) >> 8) as u8
    }

    #[cfg(debug_assertions)]
    pub fn test_valid(&self) -> bool {
        self.flags().contains(GcHeadFlag::MAGIC_NUM)
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
        GcPartitionId(self.partition as u16)
    }

    /// Set partition ID
    #[inline(always)]
    pub(crate) fn set_partition_id(&mut self, id: GcPartitionId) {
        debug_assert!(
            self.get_partition_id() == GcPartitionId::NONE || self.get_partition_id() == id
        );
        self.partition = self.partition & 0xFFFF_0000 | id.0 as u32;
    }

    #[inline(always)]
    pub(super) fn get_trace_fn(
        &self,
        type_registry: &TypeRegistry,
    ) -> fn(NonNull<GcHead>, GcTraceOp) {
        let f = type_registry.with_type_id(self.gc_type_id(), |t| t.trace_fn);

        #[cfg(debug_assertions)]
        {
            f.unwrap()
        }
        #[cfg(not(debug_assertions))]
        unsafe {
            f.unwrap_unchecked()
        }
    }

    /// get raw pointer to payload data
    #[inline(always)]
    pub unsafe fn payload(&self) -> NonNull<u8> {
        #[cfg(debug_assertions)]
        debug_assert!(
            self.test_valid(),
            "invalid gc node: head={self:p}, attrs={:#x}, flags={:?}, xref={:?}",
            self.attrs,
            self.flags(),
            self.xref_partition(),
        );

        unsafe {
            NonNull::from_ref(self)
                .cast::<u8>()
                .add(std::mem::size_of::<GcHead>())
        }
    }
}

/// Garbage collection reference
#[repr(transparent)]
pub struct GcRef<T> {
    pub(super) head_ptr: NonNull<GcHead>,
    pub(super) _marker: PhantomData<T>,
}

impl<T> Clone for GcRef<T> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            head_ptr: self.head_ptr,
            _marker: PhantomData,
        }
    }
}

impl<T> Deref for GcRef<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { self.head_ptr.as_ref().payload().cast::<T>().as_ref() }
    }
}

impl<T> DerefMut for GcRef<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.head_ptr.as_ref().payload().cast::<T>().as_mut() }
    }
}

impl<T> PartialEq for GcRef<T> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.head_ptr == other.head_ptr
    }
}

impl<T> Eq for GcRef<T> {}

impl<T> Copy for GcRef<T> {}

impl<T> From<GcRef<T>> for NonNull<GcHead> {
    #[inline(always)]
    fn from(r: GcRef<T>) -> Self {
        r.head_ptr
    }
}
impl<T> From<&GcRef<T>> for NonNull<GcHead> {
    #[inline(always)]
    fn from(r: &GcRef<T>) -> Self {
        r.head_ptr
    }
}

impl<T> std::fmt::Debug for GcRef<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GcRef({:p}:{:p})",
            self.head_ptr,
            std::ops::Deref::deref(self)
        )
    }
}

impl<T> GcRef<T> {
    /// Make an invalid GcRef.
    ///
    /// # Safety
    ///
    /// Don't access this pointer.
    pub fn dangling() -> Self {
        Self {
            head_ptr: NonNull::dangling(),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub fn downgrade(&self, heap: &mut crate::GcHeap) -> crate::weak::GcWeak<T> {
        heap.downgrade(self)
    }

    /// check if this is root object
    #[inline(always)]
    pub fn is_root(&self) -> bool {
        unsafe { self.head_ptr.as_ref().is_root() }
    }

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
    pub fn try_from_ref(heap: &GcHeap, data_ref: &T) -> Option<Self>
    where
        T: GcTracable,
    {
        // 获取数据指针
        let data_ptr = data_ref as *const T as *mut u8;

        // Subtract forward to get header pointer
        let header_ptr = unsafe { data_ptr.sub(std::mem::size_of::<GcHead>()).cast::<GcHead>() };

        // Check if pointer is valid
        let header = NonNull::new(header_ptr)?;
        let type_id = unsafe { header.as_ref().gc_type_id() };

        // Verify function pointer matches
        let expected_dispose_fn: Option<unsafe fn(*mut u8)> = if std::mem::needs_drop::<T>() {
            Some(dispose_fn::<T>)
        } else {
            None
        };
        let expected_trace_fn: unsafe fn(NonNull<GcHead>, GcTraceOp) = trace_fn::<T>;

        // If type index is 0, not a valid GC object
        if type_id == 0 {
            return None;
        }

        // Check if trace/dispose callback matches
        heap.type_registry
            .with_type_id(type_id, |t| (t.trace_fn, t.dispose_fn))
            .and_then(|(trace, dispose)| {
                if std::ptr::fn_addr_eq(trace, expected_trace_fn)
                    && match (dispose, expected_dispose_fn) {
                        (Some(a), Some(b)) => std::ptr::fn_addr_eq(a, b),
                        (None, None) => true,
                        _ => false,
                    }
                {
                    Some(Self {
                        head_ptr: header,
                        _marker: PhantomData,
                    })
                } else {
                    None
                }
            })
    }

    /// Unsafe conversion from &T to GcRef<T>, main focus on speed.
    ///
    /// Safety
    /// You must ensure &T comes from GcRef<T>, otherwise consequences are unpredictable.
    #[inline(always)]
    pub unsafe fn from_ref_unchecked(data_ref: &T) -> Self
    where
        T: GcTracable,
    {
        let data_ptr = data_ref as *const T as *mut u8;
        Self {
            head_ptr: unsafe {
                NonNull::new_unchecked(data_ptr.sub(std::mem::size_of::<GcHead>()).cast::<GcHead>())
            },
            _marker: PhantomData,
        }
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

    #[cfg(debug_assertions)]
    pub fn test_valid(&self) -> bool {
        unsafe { self.head_ptr.as_ref().test_valid() }
    }
}

impl GcRef<()> {
    /// Unsafe conversion from GcHead raw pointer to untyped GcRef<()>.
    ///
    /// Safety
    /// You must ensure GcHead raw pointer comes from GcRef<T>, otherwise consequences are unpredictable.
    #[inline(always)]
    pub unsafe fn from_head_ptr(head_ptr: NonNull<GcHead>) -> Self {
        Self {
            head_ptr,
            _marker: PhantomData,
        }
    }
}

/// Garbage collection pointer wrapper (lifetime bound to GcContext)
pub struct Gc<'heap, T> {
    inner: GcRef<T>,
    _marker: std::marker::PhantomData<&'heap ()>,
}

impl<'heap, T> Deref for Gc<'heap, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl<'heap, T> DerefMut for Gc<'heap, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.deref_mut()
    }
}

impl<'heap, T: GcTracable> Clone for Gc<'heap, T> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            inner: self.inner,
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
