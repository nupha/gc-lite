// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{marker::PhantomData, ptr::NonNull};

use crate::{GcHeap, GcPartitionId, GcTracable, GcTracer, type_registry::TypeRegistry};

#[cfg(debug_assertions)]
pub(super) const GC_HEAD_MAGIC: u8 = 0x50;

/// GC object header
#[repr(C)]
pub struct GcHead {
    /// Mark bits (reachable status) and other flags
    pub(super) flags: u8,
    /// Partition ID + Type IDX (16 bits each)
    pub(super) type_partition: u32,
    /// Weak reference index, u16::MAX means none
    pub(super) weak_ref_index: u16,
    /// Pointer to next object (for list traversal)
    pub(super) next: Option<NonNull<GcHead>>,
}

impl GcHead {
    #[cfg(debug_assertions)]
    #[inline(always)]
    pub fn test_valid(&self) -> bool {
        self.flags & GC_HEAD_MAGIC == GC_HEAD_MAGIC
    }

    /// Set/clear mark bit
    #[inline(always)]
    pub(super) fn set_marked(&mut self, marked: bool) {
        if marked {
            self.flags |= 0x01;
        } else {
            self.flags &= !0x01;
        }
    }

    /// Check if marked
    #[inline(always)]
    pub fn is_marked(&self) -> bool {
        (self.flags & 0x01) != 0
    }

    /// Set/clear root object mark bit
    #[inline(always)]
    pub(super) fn set_root(&mut self, is_root: bool) {
        if is_root {
            self.flags |= 0x02;
        } else {
            self.flags &= !0x02;
        }
    }

    /// 检查是否为根对象
    #[inline(always)]
    pub fn is_root(&self) -> bool {
        (self.flags & 0x02) != 0
    }

    /// Get partition ID
    #[inline(always)]
    pub fn get_partition_id(&self) -> GcPartitionId {
        crate::partition::GcPartitionId((self.type_partition >> 16) as u16)
    }

    /// Get type ID
    #[inline(always)]
    pub fn type_id(&self) -> u16 {
        (self.type_partition & 0xFFFF) as u16
    }

    /// Get weak reference index
    #[inline(always)]
    pub fn get_weak_ref_index(&self) -> Option<usize> {
        if self.weak_ref_index == u16::MAX {
            None
        } else {
            Some(self.weak_ref_index as usize)
        }
    }

    /// Set weak reference index
    #[inline(always)]
    pub(super) fn set_weak_ref_index(&mut self, index: Option<usize>) {
        self.weak_ref_index = index.map(|i| i as u16).unwrap_or(u16::MAX);
    }

    #[inline(always)]
    pub(super) fn get_trace_fn(
        &self,
        type_registry: &TypeRegistry,
    ) -> unsafe fn(*mut u8, &mut GcTracer) {
        type_registry
            .with_type_id(self.type_id(), |t| t.trace_fn)
            .unwrap()
    }
}

/// Garbage collection reference
#[repr(transparent)]
pub struct GcRef<T> {
    pub(super) head_ptr: NonNull<GcHead>,
    pub(super) _marker: PhantomData<T>,
}

impl<T> Clone for GcRef<T> {
    #[inline(always)]
    fn clone(&self) -> Self {
        Self {
            head_ptr: self.head_ptr,
            _marker: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for GcRef<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(debug_assertions)]
        {
            write!(f, "GcRef({:p}:{:p})", self.head_ptr, self.as_ptr())
        }
        #[cfg(not(debug_assertions))]
        {
            write!(f, "GcRef({:p})", self.as_ptr())
        }
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

impl<T> GcRef<T> {
    /// get raw pointer to T
    #[inline(always)]
    pub fn as_mut_ptr(&self) -> *mut T {
        unsafe {
            #[cfg(debug_assertions)]
            debug_assert!(
                self.test_valid(),
                "gc head is not valid: {:p}",
                self.head_ptr
            );

            let data_ptr = (self.head_ptr.as_ptr() as *mut u8).add(std::mem::size_of::<GcHead>());
            data_ptr.cast::<T>()
        }
    }

    /// 获取指向T的原始指针
    #[inline(always)]
    pub fn as_ptr(&self) -> *const T {
        self.as_mut_ptr() as *const T
    }

    /// Get reference
    #[inline(always)]
    pub unsafe fn as_ref(&self) -> &T {
        unsafe { &*self.as_ptr() }
    }

    /// Get mutable reference
    #[inline(always)]
    pub unsafe fn as_mut(&self) -> &mut T {
        unsafe { &mut *self.as_mut_ptr() }
    }

    /// Downgrade to weakref
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
        let type_id = unsafe { header.as_ref().type_id() };

        // Verify function pointer matches
        let expected_dispose_fn = if std::mem::needs_drop::<T>() {
            crate::heap::dispose_fn::<T>
        } else {
            crate::heap::noop_dispose_fn
        };
        let expected_trace_fn = crate::heap::trace_fn::<T>;

        // If type index is 0, not a valid GC object
        if type_id == 0 {
            return None;
        }

        // Check if function pointer matches
        heap.type_registry
            .with_type_id(type_id, |t| (t.trace_fn, t.dispose_fn))
            .and_then(|(trace, dispose)| {
                if dispose as usize == expected_dispose_fn as usize
                    && trace as usize == expected_trace_fn as usize
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

    #[inline(always)]
    pub fn head_ptr(&self) -> NonNull<GcHead> {
        self.head_ptr
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

    /// Get internal reference
    #[inline(always)]
    pub fn as_ref(&self) -> &T {
        unsafe { self.inner.as_ref() }
    }

    /// Get mutable internal reference
    #[inline(always)]
    pub fn as_mut(&mut self) -> &mut T {
        unsafe { self.inner.as_mut() }
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

impl<'heap, T: GcTracable> Clone for Gc<'heap, T> {
    #[inline(always)]
    fn clone(&self) -> Self {
        Self {
            inner: self.inner,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<'heap, T: GcTracable> std::fmt::Debug for Gc<'heap, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gc({:?})", self.inner)
    }
}

impl<'heap, T: GcTracable> std::ops::Deref for Gc<'heap, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl<'heap, T: GcTracable> std::ops::DerefMut for Gc<'heap, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_mut()
    }
}
