// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use crate::{
    GcHeap, GcPartitionId, GcTrace, GcTraceCtx, GcWeak,
    gctype::{GcTypeRegistry, payload_offset_of},
    weak::GcWeakRawId,
};

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

/// Generational bits, stored in the otherwise unused `attrs` bits 16-17.
///
/// They deliberately live outside [`GcNodeFlag`] (a `u8`, bits 2-7) so the
/// existing flag layout is untouched.
#[cfg(feature = "gc_generations")]
const GEN_YOUNG: u32 = 1 << 16;
/// Node is in the remembered set: an old node with a possibly-young child.
#[cfg(feature = "gc_generations")]
const REMEMBERED: u32 = 1 << 17;
/// Survival counter (5 bits, 0..=31) used for X-generation promotion.
#[cfg(feature = "gc_generations")]
const AGE_SHIFT: u32 = 18;
#[cfg(feature = "gc_generations")]
const AGE_MASK: u32 = 0b1_1111 << AGE_SHIFT;
/// Largest survival count that fits in the age bits.
#[cfg(feature = "gc_generations")]
pub const MAX_AGE: u32 = 0b1_1111;

bitflags::bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GcNodeFlag :u8 {
        /// allocated from arena (bump+free-list)
        const ARENA_ALLOC = 1 << 2;
        /// node is in gray list (mark phase optimization)
        const GRAY_LISTED = 1 << 3;
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
    /// * bit 16-23: generational bits (bit16 young, bit17 remembered, bits
    ///   18-22 survival age) when the `gc_generations` feature is on; reserved
    ///   otherwise
    /// * bit 8-15:  gc datatype id
    /// * bit 5-7:   flags
    /// * bit 2-4:   reserved
    /// * bit 0-1:   TriColor state
    pub(super) attrs: u32,

    /// XRef partition id (16bit) + Partition id (16bit)
    pub(super) partition: u32,

    pub(super) weak_id: GcWeakRawId,

    /// Hint: this node's index in the LOCAL scope's cache, or
    /// [`SlotHint::NONE`].
    ///
    /// On 64-bit targets it fills the padding that already sat before `next`,
    /// so it costs no extra byte; it lets a scope remove a node without
    /// scanning its cache. It is only a hint: every use verifies that it still
    /// names the node and falls back to a scan when it does not.
    ///
    /// On 32-bit targets the header is exactly packed (no padding), so the
    /// hint is a zero-sized no-op there and removals keep the scan — see
    /// [`SlotHint`].
    pub(super) cache_slot: SlotHint,

    /// Pointer to next object (for list traversal)
    pub(super) next: Option<NonNull<GcHead>>,
}

/// The scope-cache slot hint stored in [`GcHead::cache_slot`].
///
/// With 8-byte pointers the field before `next` leaves a 4-byte hole in the
/// header, so the hint is a `u32`. With 4-byte pointers there is no hole and
/// the header is fully packed: the hint becomes `()` (a zero-sized field, which
/// a `repr(C)` struct places without moving anything), so a node keeps its size
/// and `GcScopeState::remove_node` falls back to the scan it always did.
#[cfg(target_pointer_width = "64")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct SlotHint(u32);

/// See the 64-bit variant: on a packed header the hint would cost 4 bytes per
/// node, so it degrades to a no-op and lookups scan.
#[cfg(not(target_pointer_width = "64"))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct SlotHint(());

#[cfg(target_pointer_width = "64")]
impl SlotHint {
    /// No hint recorded.
    pub(super) const NONE: Self = Self(u32::MAX);

    /// The hint for `index`.
    #[inline(always)]
    pub(super) fn of(index: usize) -> Self {
        if index < u32::MAX as usize {
            Self(index as u32)
        } else {
            Self::NONE
        }
    }

    /// `index` when this hint still names `node`'s slot in `cache`.
    #[inline(always)]
    pub(super) fn names(self, cache: &[NonNull<GcHead>], node: NonNull<GcHead>) -> Option<usize> {
        let slot = self.0 as usize;
        if slot < cache.len() && cache[slot] == node {
            Some(slot)
        } else {
            None
        }
    }
}

#[cfg(not(target_pointer_width = "64"))]
impl SlotHint {
    /// No hint recorded (the hint does not exist on this target).
    pub(super) const NONE: Self = Self(());

    /// The hint for `index` — never recorded on this target.
    #[inline(always)]
    pub(super) fn of(_index: usize) -> Self {
        Self::NONE
    }

    /// Always `None`: without a hint every lookup scans.
    #[inline(always)]
    pub(super) fn names(self, _cache: &[NonNull<GcHead>], _node: NonNull<GcHead>) -> Option<usize> {
        None
    }
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

        s.finish()
    }
}

impl GcHead {
    /// Get gc node data type id
    #[inline(always)]
    pub fn dtype(&self) -> u8 {
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

    /// Clear the LOCAL flag on this node.
    ///
    /// This is the inverse of `alloc_local`'s flag-setting side effect, but the
    /// scope cache is **not** touched: the node keeps its cache entry, and the
    /// flag no longer protects it, so the GC may free it while the cache's
    /// later `clear()` still writes to it. Prefer
    /// [`GcHeap::release_local`](crate::GcHeap::release_local), which removes
    /// the entry *and* clears the flag.
    ///
    /// Only use this when nothing caches the node — e.g. a value that already
    /// lost its protection — and make sure it is reachable through some other
    /// root before clearing the flag.
    ///
    /// # Safety
    ///
    /// The caller must ensure the node is still alive and that clearing the
    /// flag does not violate GC invariants (e.g., the node must be reachable
    /// through some other root or reference after the flag is cleared).
    #[inline(always)]
    pub unsafe fn clear_local_flag(&mut self) {
        self.remove_flag(GcNodeFlag::LOCAL);
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

    /// Check if node is already in the gray list (O(1) check)
    #[inline(always)]
    pub(super) fn is_gray_listed(&self) -> bool {
        (self.attrs & (GcNodeFlag::GRAY_LISTED.bits() as u32)) != 0
    }

    /// Set/clear the gray-listed flag
    #[inline(always)]
    pub(super) fn set_gray_listed(&mut self, b: bool) {
        if b {
            self.attrs |= GcNodeFlag::GRAY_LISTED.bits() as u32;
        } else {
            self.attrs &= !(GcNodeFlag::GRAY_LISTED.bits() as u32);
        }
    }

    /// Whether the node is in the young generation (`gc_generations`).
    #[cfg(feature = "gc_generations")]
    #[inline(always)]
    pub fn is_young(&self) -> bool {
        (self.attrs & GEN_YOUNG) != 0
    }

    #[cfg(feature = "gc_generations")]
    #[inline(always)]
    pub(super) fn set_young(&mut self, b: bool) {
        if b {
            self.attrs |= GEN_YOUNG;
        } else {
            self.attrs &= !GEN_YOUNG;
        }
    }

    /// Move this node from the young generation to the old generation.
    ///
    /// Only the generation bits change; the caller is responsible for moving
    /// the node between the partition's young and old chains. Kept in one place
    /// so the minor promotion path and the bulk major merge cannot drift apart.
    #[cfg(feature = "gc_generations")]
    #[inline(always)]
    pub(super) fn promote_to_old(&mut self) {
        self.set_young(false);
        self.set_age(0);
    }

    /// Whether the node is a remembered-set source (`gc_generations`).
    #[cfg(feature = "gc_generations")]
    #[inline(always)]
    pub fn is_remembered(&self) -> bool {
        (self.attrs & REMEMBERED) != 0
    }

    /// How many minors the node has survived while still young
    /// (`gc_generations`, 0 = allocated since the last minor).
    #[cfg(feature = "gc_generations")]
    #[inline(always)]
    pub fn age(&self) -> u32 {
        (self.attrs & AGE_MASK) >> AGE_SHIFT
    }

    #[cfg(feature = "gc_generations")]
    #[inline(always)]
    pub(crate) fn set_age(&mut self, age: u32) {
        self.attrs = (self.attrs & !AGE_MASK) | (age.min(MAX_AGE) << AGE_SHIFT);
    }

    #[cfg(feature = "gc_generations")]
    #[inline(always)]
    pub(super) fn set_remembered(&mut self, b: bool) {
        if b {
            self.attrs |= REMEMBERED;
        } else {
            self.attrs &= !REMEMBERED;
        }
    }

    /// Get node's partition id
    #[inline(always)]
    pub fn partition_id(&self) -> GcPartitionId {
        GcPartitionId((self.partition & 0x0000_FFFF) as u16)
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
    #[inline(always)]
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

impl<T: GcNode> GcTrace for GcRef<T> {
    #[inline(always)]
    fn trace(&self, gcx: &mut GcTraceCtx) {
        gcx.add(*self);
    }
}

impl<T: GcNode> GcRef<T> {
    pub unsafe fn from_node_ptr(head_ptr: NonNull<GcHead>) -> Self {
        Self {
            head_ptr,
            _marker: PhantomData::<T>,
        }
    }

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
        if unsafe { heap.is_node_partition_marking(*self) } {
            // SAFETY: self is alive (caller guarantees it via the unsafe fn contract).
            let node = unsafe { self.head_ptr.as_mut() };
            if node.color() == GcTriColor::Black {
                node.set_color(GcTriColor::Gray);
                heap.add_gray_node(self.head_ptr);
            }
        }

        // Generational fallback when the written value is unknown: remember the
        // source so the next minor re-traces its outgoing edges. This over-
        // records (it cannot tell whether the value is young), which is safe.
        #[cfg(feature = "gc_generations")]
        unsafe {
            let node = self.head_ptr.as_ref();
            if !node.is_young() && !node.is_remembered() {
                self.head_ptr.as_mut().set_remembered(true);
                heap.remembered.push(self.head_ptr);
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
    #[inline(always)]
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

        // Generational remembered set: an old master gaining a young slave is
        // the one edge kind a minor cannot see by scanning the young chain.
        // The source bit dedupes the Vec; it is cleared at minor/major end.
        #[cfg(feature = "gc_generations")]
        unsafe {
            let (master_young, slave_young, master_remembered) = (
                master.as_ref().is_young(),
                slave.as_ref().is_young(),
                master.as_ref().is_remembered(),
            );
            if !master_young && slave_young && !master_remembered {
                let mut master = master;
                master.as_mut().set_remembered(true);
                self.remembered.push(master);
            }
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

            if master_pid != slave_pid {
                let slave_color = slave.as_ref().color();

                if matches!(slave_color, GcTriColor::White | GcTriColor::Gray)
                    && let Some(slave_par) = self.partition_mut(slave_pid)
                    && slave_par.is_marking()
                {
                    slave.as_mut().set_color(GcTriColor::Gray);
                    if !slave.as_ref().is_gray_listed() {
                        slave.as_mut().set_gray_listed(true);
                        slave_par.gray_list.push(slave);
                    }
                }
            }
        }
    }

    /// Mark `owner` dirty for minor GC.
    ///
    /// "Minor" means a generational minor collection that re-scans the
    /// remembered set (not tri-color/incremental marking). Use this when
    /// `owner`'s traced reachability changes but there is no specific child
    /// edge to report via [`bind`](Self::bind). If `owner` is old and not yet
    /// remembered, it is added to the remembered set so minor GC re-traces it.
    ///
    /// Unlike `bind`, this does not depend on the child being young and does
    /// not perform tri-color child marking.
    ///
    /// # Safety
    ///
    /// `owner` must point to a valid, live GC node managed by this heap.
    #[cfg(feature = "gc_generations")]
    pub fn set_minor_dirty(&mut self, mut owner: NonNull<GcHead>) {
        #[cfg(debug_assertions)]
        unsafe {
            owner.as_ref().debug_assert_node_valid(self);
        }

        unsafe {
            if !owner.as_ref().is_young() && !owner.as_ref().is_remembered() {
                owner.as_mut().set_remembered(true);
                self.remembered.push(owner);
            }
        }
    }
}

#[cfg(debug_assertions)]
impl GcHead {
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

/// The header layout is load-bearing: the slot hint must never grow a node.
/// 64-bit: `attrs`/`partition`/`weak_id` (12B), the 4-byte hole before `next`
/// holds the hint, `next` at 16 → 24B, the same as before the hint existed.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(core::mem::size_of::<GcHead>() == 24);
    assert!(core::mem::offset_of!(GcHead, cache_slot) == 12);
    assert!(core::mem::offset_of!(GcHead, next) == 16);
};

/// 32-bit: the header is exactly packed, so the hint must be zero-sized and
/// `next` must still follow `weak_id` directly → 16B, unchanged.
#[cfg(not(target_pointer_width = "64"))]
const _: () = {
    assert!(core::mem::size_of::<GcHead>() == 16);
    assert!(core::mem::offset_of!(GcHead, next) == 12);
    assert!(core::mem::size_of::<SlotHint>() == 0);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_slot_hint_is_none() {
        let info = GcHead {
            attrs: 0xFF00_0000,
            partition: 0,
            weak_id: GcWeakRawId::NULL,
            cache_slot: SlotHint::NONE,
            next: None,
        };
        assert_eq!(info.cache_slot, SlotHint::NONE);
        assert!(!info.is_root_or_local());
    }

    #[test]
    fn the_hint_names_only_its_own_slot() {
        let mut a = GcHead {
            attrs: 0xFF00_0000,
            partition: 0,
            weak_id: GcWeakRawId::NULL,
            cache_slot: SlotHint::of(1),
            next: None,
        };
        let a_ptr = NonNull::from(&mut a);
        let mut b = GcHead {
            attrs: 0xFF00_0000,
            partition: 0,
            weak_id: GcWeakRawId::NULL,
            cache_slot: SlotHint::NONE,
            next: None,
        };
        let b_ptr = NonNull::from(&mut b);
        let cache = [b_ptr, a_ptr];
        if cfg!(target_pointer_width = "64") {
            assert_eq!(a.cache_slot.names(&cache, a_ptr), Some(1));
        } else {
            // No hint room on this target: every lookup scans.
            assert_eq!(a.cache_slot.names(&cache, a_ptr), None);
        }
        // The same slot does not name another node.
        assert_eq!(a.cache_slot.names(&cache, b_ptr), None);
        // An out-of-range hint is ignored rather than trusted.
        assert_eq!(SlotHint::of(7).names(&cache, a_ptr), None);
        assert_eq!(SlotHint::NONE.names(&cache, a_ptr), None);
    }

    /// The 32-bit header relies on a zero-sized field adding no bytes to a
    /// `repr(C)` struct; assert that rule on whatever target runs the tests by
    /// mirroring the 32-bit field widths with `u32` in place of the pointers.
    #[test]
    fn a_zero_sized_field_adds_no_bytes() {
        #[repr(C)]
        struct NarrowHead {
            attrs: u32,
            partition: u32,
            weak_id: u32,
            cache_slot: (),
            next: u32,
        }
        assert_eq!(std::mem::size_of::<NarrowHead>(), 16);
        assert_eq!(std::mem::offset_of!(NarrowHead, next), 12);
        assert_eq!(
            std::mem::size_of::<SlotHint>(),
            if cfg!(target_pointer_width = "64") {
                4
            } else {
                0
            }
        );
    }
}
