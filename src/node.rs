// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

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
const PROTECT_COUNT_SHIFT: u32 = 2;
const PROTECT_COUNT_MASK: u32 = 0b111 << PROTECT_COUNT_SHIFT;

bitflags::bitflags! {
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GcNodeFlag :u8 {
        /// is root node
        const ROOT = 1 << 5;

        /// internal traversal visited flag
        const TRAVERSE_VISITED = 1 << 6;

        #[cfg(debug_assertions)]
        const MAGIC_NUM = 1 << 7;
    }
}

/// GC node info
pub struct GcHead {
    /// Attributes of node:
    /// * bit 24-31: debug sentinel (debug build)
    /// * bit 16-23: reserved
    /// * bit 8-15:  gc datatype id
    /// * bit 5-7:   flags
    /// * bit 2-4:   protect count (1-7 means protected)
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
            .field("color", &self.color());

        if self.is_protected() {
            s.field("protected", &self.protect_count());
        }
        if !self.weak_id.is_null() {
            let w = self.weak_id;
            s.field("weakref", &format!("{}#{}", w.index(), w.version()));
        }
        // if !self.xref().is_null() {
        //     s.field("xref", &self.xref());
        // }

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
    pub(crate) fn protect_count(&self) -> u8 {
        ((self.attrs & PROTECT_COUNT_MASK) >> PROTECT_COUNT_SHIFT) as u8
    }

    #[inline(always)]
    pub fn is_protected(&self) -> bool {
        (self.attrs & PROTECT_COUNT_MASK) != 0
    }

    #[inline(always)]
    fn set_protect_count(&mut self, count: u8) {
        debug_assert!(count <= 7);
        let count = (count as u32) << PROTECT_COUNT_SHIFT;
        self.attrs = (self.attrs & !PROTECT_COUNT_MASK) | count;
    }

    #[inline(always)]
    pub(super) fn inc_protect_count(&mut self) -> u8 {
        let mut count = self.protect_count();
        if count >= 7 {
            panic!("GcHead protect count overflow");
        }
        count += 1;
        self.set_protect_count(count);
        count
    }

    #[inline(always)]
    pub(super) fn dec_protect_count(&mut self) -> u8 {
        let mut count = self.protect_count();
        debug_assert!(count > 0);
        count -= 1;
        self.set_protect_count(count);
        count
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

    /// Set/clear root object flag
    pub(super) fn set_root(&mut self, is_root: bool) {
        if is_root {
            self.insert_flag(GcNodeFlag::ROOT);
        } else {
            self.remove_flag(GcNodeFlag::ROOT);
        }
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
        debug_assert!(self.partition_id().is_null() || self.partition_id() == id);
        self.partition = (self.partition & 0xFFFF_0000) | id.0 as u32;
    }

    /// get raw pointer to payload data
    #[inline(always)]
    pub fn payload(&self) -> NonNull<u8> {
        #[cfg(debug_assertions)]
        self.debug_assert_node_valid_simple();

        unsafe { NonNull::from_ref(self).add(1).cast::<u8>() }
    }

    /// Get GcRef<T> from node. if node is not of type T, returns None
    #[inline(always)]
    pub fn gc_ref<T: GcNode>(&self) -> Option<GcRef<T>> {
        if T::GC_TYPE_ID == self.dtype() {
            Some(GcRef::<T> {
                head_ptr: NonNull::from_ref(self),
                _marker: PhantomData,
            })
        } else {
            None
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

impl<T: GcNode> Deref for GcRef<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe { self.head_ptr.as_ref().payload().cast::<T>().as_ref() }
    }
}

impl<T: GcNode> DerefMut for GcRef<T> {
    /// FIXME: DerefMut breaks gc node write barrier. This should be disabled.
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.head_ptr.as_mut().payload().cast::<T>().as_mut() }
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

    pub fn with_mut<F, R>(&mut self, heap: &mut GcHeap, mutator: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        let head = unsafe { self.head_ptr.as_mut() };
        if head.color() == GcTriColor::Black {
            head.set_color(GcTriColor::Gray);
            heap.add_gray_node(self.head_ptr);
        }

        let value = unsafe { head.payload().cast::<T>().as_mut() };
        mutator(value)
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
    #[deprecated(note = "this may break write barrier. this is unsafe.")]
    #[inline(always)]
    pub fn node_ptr(&self) -> NonNull<GcHead> {
        self.head_ptr
    }

    /// get node info
    #[inline(always)]
    pub fn node_info(&self) -> &GcHead {
        unsafe { self.head_ptr.as_ref() }
    }

    #[inline(always)]
    pub fn to_local(self, heap: &GcHeap) -> GcLocal<T> {
        GcLocal::new(heap, self)
    }
}

pub struct GcLocal<T: GcNode> {
    gc: GcRef<T>,
    heap: NonNull<GcHeap>,
}

impl<T: GcNode> Drop for GcLocal<T> {
    fn drop(&mut self) {
        let heap = unsafe { self.heap.as_mut() };
        let mut node = self.gc.head_ptr;

        unsafe {
            let n = node.as_mut();
            let count = n.dec_protect_count();

            if count == 0
                && !n.is_root()
                && let Some(par) = heap.partition_mut(n.partition_id())
                && let Some(i) = par.root_nodes.iter().position(|&x| x == node)
            {
                par.root_nodes.swap_remove(i);
            }
        }
    }
}

impl<T: GcNode + std::fmt::Debug> std::fmt::Debug for GcLocal<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", std::ops::Deref::deref(&self))
    }
}

impl<T: GcNode + std::fmt::Display> std::fmt::Display for GcLocal<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", std::ops::Deref::deref(&self))
    }
}

impl<T: GcNode> From<GcLocal<T>> for GcRef<T> {
    #[inline(always)]
    fn from(value: GcLocal<T>) -> Self {
        value.gc
    }
}

impl<T: GcNode> std::ops::Deref for GcLocal<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.gc
    }
}

impl<T: GcNode> GcLocal<T> {
    pub fn new(heap: &GcHeap, gc: GcRef<T>) -> Self {
        let mut heap_ptr = NonNull::from_ref(heap);
        let head: NonNull<GcHead> = (&gc).into();

        unsafe {
            heap_ptr.as_mut().do_protect_node(head);
        }

        Self { gc, heap: heap_ptr }
    }

    #[inline(always)]
    pub fn get(&self) -> GcRef<T> {
        self.gc
    }
}

impl GcHeap {
    /// bind nodes relationship for directed reference: from `master` to `slave`.
    /// will perform cross scope reference update and tri-color marking.
    pub fn bind(&mut self, master: NonNull<GcHead>, mut slave: NonNull<GcHead>) {
        #[cfg(debug_assertions)]
        unsafe {
            master.as_ref().debug_assert_node_valid(self);
            slave.as_ref().debug_assert_node_valid(self);
        }

        // tri-color marking
        unsafe {
            if matches!(
                (master.as_ref().color(), slave.as_ref().color()),
                (GcTriColor::Black, GcTriColor::White | GcTriColor::Gray)
            ) {
                slave.as_mut().set_color(GcTriColor::Gray);

                if self
                    .partition(slave.as_ref().partition_id())
                    .unwrap()
                    .is_marking()
                {
                    self.add_gray_node(slave);
                }
            }
        }

        if unsafe { master.as_ref().partition_id() != slave.as_ref().partition_id() } {
            // update cross scope reference
            let xref = unsafe {
                let x = master.as_ref().xref();
                if x.is_null() {
                    master.as_ref().partition_id()
                } else {
                    x
                }
            };
            self.set_xref(xref, slave);
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
}
