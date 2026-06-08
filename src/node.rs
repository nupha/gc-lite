// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::gctype::{GcTypeRegistry, payload_offset_of};
use crate::{GcHeap, GcPartitionId, GcTrace, GcWeak, weak::GcWeakRawId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GcTriColor {
    White = 0b00,
    Gray = 0b01,
    Black = 0b10,
}

impl From<GcTriColor> for u32 {
    fn from(color: GcTriColor) -> Self {
        color as u32
    }
}

impl TryFrom<u32> for GcTriColor {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0b00 => Ok(GcTriColor::White),
            0b01 => Ok(GcTriColor::Gray),
            0b10 => Ok(GcTriColor::Black),
            _ => Err("Invalid value for TriColor"),
        }
    }
}

const COLOR_MASK: u32 = 0b11;

bitflags::bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GcNodeFlag :u8 {
        /// is root node
        const ROOT = 1 << 5;

        /// node is inside a GcContext
        const LOCAL = 1 << 6;

        /// internal traversal visited flag
        const TRAVERSE_VISITED = 1 << 7;
    }
}

/// GC node info
#[repr(C)]
pub struct GcHead {
    /// Attributes of node:
    /// * bit 24-31: debug sentinel (debug build)
    /// * bit 16-23: reserved
    /// * bit 8-15:  gc datatype id
    /// * bit 5-7:   flags
    /// * bit 2-4:   reserved
    /// * bit 0-1:   TriColor state
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
            .field("partition", &self.partition_id())
            .field("color", &self.color())
            .field("local", &self.is_local());

        if self.is_root() {
            s.field("root", &true);
        }
        if !self.weak_id.is_null() {
            let w = self.weak_id;
            s.field("weakref", &format!("{}#{}", w.index(), w.version()));
        }

        #[cfg(debug_assertions)]
        {
            s.field("dbg_string", &self.dbg_string);
        }

        s.finish()
    }
}

impl GcHead {
    /// Get gc node data type id
    #[inline(always)]
    pub(crate) fn dtype(&self) -> u8 {
        ((self.attrs & 0xFF00) >> 8) as u8
    }

    /// Get the current TriColor state.
    #[inline(always)]
    pub(crate) fn color(&self) -> GcTriColor {
        // This should not fail if the internal state is managed correctly.
        GcTriColor::try_from(self.attrs & COLOR_MASK).unwrap()
    }

    /// Set the TriColor state, preserving other flags.
    #[inline(always)]
    pub(crate) fn set_color(&mut self, color: GcTriColor) {
        self.attrs = (self.attrs & !COLOR_MASK) | (color as u32);
    }

    #[inline(always)]
    pub(crate) fn flags(&self) -> GcNodeFlag {
        GcNodeFlag::from_bits_truncate(self.attrs as u8)
    }

    /// Add a flag.
    #[inline(always)]
    pub(crate) fn insert_flag(&mut self, flag: GcNodeFlag) {
        self.attrs |= flag.bits() as u32;
    }

    /// Remove a flag.
    #[inline(always)]
    pub(crate) fn remove_flag(&mut self, flag: GcNodeFlag) {
        self.attrs &= !(flag.bits() as u32);
    }

    /// Check if a flag is present.
    #[inline(always)]
    pub(crate) fn contains_flag(&self, flag: GcNodeFlag) -> bool {
        (self.attrs & flag.bits() as u32) == flag.bits() as u32
    }

    /// Check if root node
    #[inline(always)]
    pub fn is_root(&self) -> bool {
        self.contains_flag(GcNodeFlag::ROOT)
    }

    #[inline(always)]
    pub fn is_local(&self) -> bool {
        self.contains_flag(GcNodeFlag::LOCAL)
    }

    #[inline]
    pub fn is_root_or_local(&self) -> bool {
        let f = self.flags();
        f.intersects(GcNodeFlag::ROOT | GcNodeFlag::LOCAL)
    }

    #[inline(always)]
    pub(super) fn traverse_visited(&self) -> bool {
        self.contains_flag(GcNodeFlag::TRAVERSE_VISITED)
    }

    #[inline(always)]
    pub(super) fn set_traverse_visited(&mut self, visited: bool) {
        if visited {
            self.insert_flag(GcNodeFlag::TRAVERSE_VISITED);
        } else {
            self.remove_flag(GcNodeFlag::TRAVERSE_VISITED);
        }
    }

    /// Get node's partition id
    #[inline(always)]
    pub fn partition_id(&self) -> GcPartitionId {
        GcPartitionId((self.partition & 0x0000_FFFF) as u16)
    }

    #[inline(always)]
    pub(crate) fn set_partition_id(&mut self, id: GcPartitionId) {
        debug_assert!(
            self.partition_id().is_null() || self.partition_id() == id,
            "set_partition_id: node already belongs to partition {}, cannot reassign to {}",
            self.partition_id().0,
            id.0,
        );
        self.partition = (self.partition & 0xFFFF_0000) | id.0 as u32;
    }

    /// Get raw pointer to payload data using the heap's type registry.
    #[inline(always)]
    pub fn payload(&self, registry: &GcTypeRegistry) -> NonNull<u8> {
        #[cfg(debug_assertions)]
        self.debug_assert_node_valid_simple();

        let info = &registry.type_info_list[self.dtype() as usize];
        info.payload_ptr(NonNull::from_ref(self))
    }

    #[inline(always)]
    pub(crate) fn payload_for<T>(&self) -> NonNull<u8> {
        #[cfg(debug_assertions)]
        self.debug_assert_node_valid_simple();

        unsafe {
            NonNull::from_ref(self)
                .cast::<u8>()
                .add(payload_offset_of::<T>())
        }
    }
}

pub trait GcNode: GcTrace {
    /// Node data type id
    const GC_TYPE_ID: u8;

    /// get gc ref
    fn gc_ref(&self) -> GcRef<Self>
    where
        Self: std::marker::Sized;

    /// get gc node head pointer
    #[inline(always)]
    fn gc_head_ptr(&self) -> std::ptr::NonNull<GcHead>
    where
        Self: std::marker::Sized,
    {
        self.gc_ref().node_ptr()
    }

    /// get gc node head info
    #[inline(always)]
    fn gc_head(&self) -> &GcHead
    where
        Self: std::marker::Sized,
    {
        unsafe { self.gc_head_ptr().as_ref() }
    }

    /// get gc node head info
    #[inline(always)]
    fn gc_head_mut(&mut self) -> &mut GcHead
    where
        Self: std::marker::Sized,
    {
        unsafe { self.gc_head_ptr().as_mut() }
    }
}

/// Garbage collection reference
#[repr(transparent)]
pub struct GcRef<T: GcNode> {
    pub(super) head_ptr: NonNull<GcHead>,
    pub(super) _marker: PhantomData<T>,
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
        write!(f, "GcRef<{:p}>", self.head_ptr)
    }
}

impl<T: GcNode> GcRef<T> {
    /// Access the underlying GC-managed object as a reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure the GC object is still alive and has not been
    /// collected (e.g., it is protected by a root/LOCAL flag, or the GC
    /// heap is guaranteed not to run a collection cycle).
    #[inline(always)]
    pub unsafe fn as_ref(&self) -> &T {
        unsafe {
            self.head_ptr
                .as_ref()
                .payload_for::<T>()
                .cast::<T>()
                .as_ref()
        }
    }

    /// Access the underlying GC-managed object as a mutable reference.
    ///
    /// # Safety
    ///
    /// Same as [`as_ref`](Self::as_ref).
    #[inline(always)]
    pub unsafe fn as_mut(&mut self) -> &mut T {
        unsafe {
            self.head_ptr
                .as_mut()
                .payload_for::<T>()
                .cast::<T>()
                .as_mut()
        }
    }

    /// Create GcRef<T> from &T reference
    ///
    /// # Safety
    ///
    /// Caller must ensure the passed reference comes from a valid GC-managed object
    /// that was allocated via `GcRef<T>`. Passing a reference from any other source
    /// (stack, heap, etc.) is undefined behavior.
    #[inline]
    pub unsafe fn try_from_ref(heap: &GcHeap, data_ref: &T) -> Option<Self> {
        let node = unsafe {
            NonNull::from_ref(data_ref)
                .cast::<u8>()
                .sub(payload_offset_of::<T>())
                .cast::<GcHead>()
        };

        if T::GC_TYPE_ID == unsafe { node.as_ref().dtype() } {
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
        let node = unsafe {
            NonNull::from_ref(data_ref)
                .cast::<u8>()
                .sub(payload_offset_of::<T>())
                .cast::<GcHead>()
        };

        #[cfg(debug_assertions)]
        unsafe {
            node.as_ref().debug_assert_node_valid_simple();
        }

        Self {
            head_ptr: node,
            _marker: PhantomData,
        }
    }

    /// Apply a mutator function to the GC-managed object, with write barrier.
    ///
    /// # Safety
    ///
    /// The caller must ensure the GC object is still alive and valid.
    /// If the `mutator` writes new GC references into the object, the write
    /// barrier will handle tri-color invariant for incremental GC.
    #[inline]
    pub unsafe fn with_write_barrier<F, R>(&mut self, heap: &mut GcHeap, mutator: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        // Only enforce the tri-color invariant when a GC marking cycle
        // is in progress for this node's partition.
        if heap.is_node_partition_marking(*self) {
            // SAFETY: self is alive (caller guarantees it via the unsafe fn contract).
            let node = unsafe { self.head_ptr.as_mut() };
            if node.color() == GcTriColor::Black {
                node.set_color(GcTriColor::Gray);
                heap.add_gray_node(self.head_ptr);
            }
        }

        // SAFETY: self is alive (caller guarantees it via the unsafe fn contract).
        let value = unsafe {
            self.head_ptr
                .as_mut()
                .payload_for::<T>()
                .cast::<T>()
                .as_mut()
        };
        mutator(value)
    }

    /// Get a raw pointer to the GC-managed payload.
    ///
    /// # Safety
    ///
    /// The caller must ensure the GC object is still alive.
    #[inline]
    pub unsafe fn as_ptr(&self) -> NonNull<T> {
        unsafe { self.head_ptr.as_ref().payload_for::<T>().cast::<T>() }
    }

    /// Create a weak reference from this GC reference.
    ///
    /// # Safety
    ///
    /// The caller must ensure the GC object is still alive.
    #[inline(always)]
    pub unsafe fn downgrade(&self, heap: &mut GcHeap) -> GcWeak<T> {
        heap.downgrade(self)
    }

    /// Check if this is a root object.
    ///
    /// # Safety
    ///
    /// The caller must ensure the GC object is still alive.
    #[inline(always)]
    pub unsafe fn is_root(&self) -> bool {
        unsafe { self.head_ptr.as_ref().is_root() }
    }

    /// Get node raw pointer (always safe — just returns the raw pointer value).
    #[inline(always)]
    pub fn node_ptr(&self) -> NonNull<GcHead> {
        self.head_ptr
    }

    /// Get node info (GC head metadata).
    ///
    /// # Safety
    ///
    /// The caller must ensure the GC object is still alive.
    #[inline(always)]
    pub unsafe fn node_info(&self) -> &GcHead {
        unsafe { self.head_ptr.as_ref() }
    }
}

/// A safe GC reference with a lifetime guarantee.
///
/// `Gc<'a, T>` wraps a [`GcRef<T>`] and provides safe `Deref`/`DerefMut`
/// implementations, where the lifetime `'a` proves that the GC object is
/// alive and protected (e.g., by a scope or a heap borrow).
///
/// # Safety invariant
///
/// The lifetime `'a` must be derived from a source that guarantees the
/// GC object will not be collected during `'a`. Typical sources:
/// - A [`GcScope`](crate::GcScope) or [`GcScopeState`](crate::GcScopeState)
///   for scope-local allocations
/// - A `&GcHeap` borrow for [`GcWeak::upgrade`](crate::GcWeak::upgrade) results
#[repr(transparent)]
pub struct Gc<'a, T: GcNode> {
    inner: GcRef<T>,
    _marker: PhantomData<&'a T>,
}

impl<'a, T: GcNode> Deref for Gc<'a, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        // SAFETY: The lifetime 'a guarantees the object is alive.
        unsafe { self.inner.as_ref() }
    }
}

impl<'a, T: GcNode> DerefMut for Gc<'a, T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: The lifetime 'a guarantees the object is alive.
        unsafe { self.inner.as_mut() }
    }
}

impl<'a, T: GcNode> Clone for Gc<'a, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, T: GcNode> Copy for Gc<'a, T> {}

impl<'a, T: GcNode> PartialEq for Gc<'a, T> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<'a, T: GcNode> Eq for Gc<'a, T> {}

impl<'a, T: GcNode> std::fmt::Debug for Gc<'a, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gc<{:p}>", self.inner.head_ptr)
    }
}

impl<'a, T: GcNode> Gc<'a, T> {
    /// Create a `Gc` from a raw `GcRef` and a lifetime proof.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the object referenced by `inner` stays
    /// alive for the entire lifetime `'a`. This is typically guaranteed
    /// by tying `'a` to a scope or a heap borrow.
    #[inline(always)]
    pub unsafe fn from_raw(inner: GcRef<T>) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    /// Convert back to the raw `GcRef` handle.
    #[inline(always)]
    pub fn into_raw(self) -> GcRef<T> {
        self.inner
    }

    /// Access the raw `GcRef` without consuming self.
    #[inline(always)]
    pub fn as_raw(&self) -> &GcRef<T> {
        &self.inner
    }
}

impl GcHeap {
    /// Establishes a directed reference from `master` to `slave` and performs
    /// the necessary write barrier for tri-color incremental GC.
    ///
    /// # When to use
    ///
    /// Call this whenever a GC node (`master`) starts referencing another GC
    /// node (`slave`) through a pointer write, e.g.:
    ///
    /// - Setting an object property to an object/string value
    /// - Pushing an element into an array
    /// - Storing a result value into a Promise
    /// - Updating a closure variable reference
    ///
    /// # Write barrier semantics
    ///
    /// In tri-color marking, if `master` is already **Black** (fully traced)
    /// and `slave` is **White** (not yet traced), the slave would be
    /// incorrectly swept as garbage. This method prevents that by:
    ///
    /// 1. Checking the colors of `master` and `slave`.
    /// 2. If `master` is Black and `slave` is White/Gray, marking `slave`
    ///    as **Gray** and enqueuing it into the gray list of its partition,
    ///    ensuring it will be traced in the current GC cycle.
    ///
    /// # Safety
    ///
    /// Both pointers must point to valid, live GC nodes managed by this heap.
    pub fn bind(&mut self, master: NonNull<GcHead>, mut slave: NonNull<GcHead>) {
        #[cfg(debug_assertions)]
        unsafe {
            master.as_ref().debug_assert_node_valid(self);
            slave.as_ref().debug_assert_node_valid(self);
        }

        // tri-color write barrier
        unsafe {
            if matches!(
                (master.as_ref().color(), slave.as_ref().color()),
                (GcTriColor::Black, GcTriColor::White | GcTriColor::Gray)
            ) {
                slave.as_mut().set_color(GcTriColor::Gray);

                let slave_pid = slave.as_ref().partition_id();
                if self.partition(slave_pid).is_some_and(|p| p.is_marking()) {
                    self.add_gray_node(slave);
                }
            }
        }

        // Cross-partition reference tracking.
        // If master and slave belong to different partitions, ensure the slave
        // is reachable from the master's partition during marking.
        unsafe {
            let master_pid = master.as_ref().partition_id();
            let slave_pid = slave.as_ref().partition_id();

            if master_pid != slave_pid && !slave_pid.is_null() && !master_pid.is_null() {
                let slave_color = slave.as_ref().color();

                if matches!(slave_color, GcTriColor::White | GcTriColor::Gray)
                    && let Some(slave_par) = self.partition_mut(slave_pid)
                    && slave_par.is_marking()
                {
                    slave.as_mut().set_color(GcTriColor::Gray);
                    if !slave_par.gray_list.contains(&slave) {
                        slave_par.gray_list.push(slave);
                    }
                }
            }
        }
    }
}

#[cfg(debug_assertions)]
impl GcHead {
    pub fn debug_set_dbg_string(&mut self, str: std::borrow::Cow<'static, str>) {
        self.dbg_string = str;
    }

    pub fn debug_dbg_string(&self) -> &std::borrow::Cow<'static, str> {
        &self.dbg_string
    }

    pub fn debug_assert_node_valid_simple(&self) {
        if !std::thread::panicking() {
            debug_assert!(
                ((self.attrs >> 24) & 0xFF) == 0xFF && self.next.is_none_or(|n| n.is_aligned()),
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
}
