// SPDX-License-Identifier: MIT
// Copyright (c) 2025 John Ray <996351336@qq.com>

use std::marker::PhantomData;

use crate::{GcHead, GcRef, heap::GcHeap};

/// Weak reference
#[derive(PartialEq, Eq)]
pub struct GcWeak<T> {
    /// bit 24-31: slot index in weak_list
    /// bit 0-16: Version number, used to prevent conflicts from slot reuse
    pub(crate) info: u32,

    pub(crate) _marker: PhantomData<T>,
}

impl<T> Clone for GcWeak<T> {
    fn clone(&self) -> Self {
        Self {
            info: self.info,
            _marker: PhantomData,
        }
    }
}

impl<T> Copy for GcWeak<T> {}

impl<T> Default for GcWeak<T> {
    fn default() -> Self {
        Self::new(0xFF, 0xFFFF)
    }
}

impl<T> std::fmt::Debug for GcWeak<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GcWeak({}#{})", self.slot_index(), self.version())
    }
}

impl<T> GcWeak<T> {
    #[inline(always)]
    pub(crate) fn new(slot_index: u8, version: u16) -> Self {
        Self {
            info: ((slot_index as u32) << 24) | (version as u32),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub(crate) fn slot_index(&self) -> u8 {
        (self.info >> 24) as u8
    }

    #[inline(always)]
    pub(crate) fn version(&self) -> u16 {
        self.info as u16
    }

    /// Upgrade weak reference to strong reference
    #[inline(always)]
    pub fn upgrade(&self, heap: &GcHeap) -> Option<GcRef<T>> {
        heap.upgrade(self)
    }
}

impl GcHeap {
    /// Create weak reference.
    pub fn downgrade<T>(&mut self, gc_ref: &GcRef<T>) -> GcWeak<T> {
        let mut node = gc_ref.head_ptr;

        if let Some(w) = unsafe { node.as_ref().weakref_index() } {
            // Weakref already exists, reuse it
            debug_assert!((w as usize) < self.weak_slots.len());
            let (ver, _ptr) = unsafe { self.weak_slots.get_unchecked(w as usize) };
            debug_assert!(_ptr.is_some());
            GcWeak::new(w, *ver)
        } else {
            if self.weak_slots.len() == u8::MAX as usize {
                panic!("too may living weakrefs");
            }

            // Find a free slot
            let i = self
                .weak_slots
                .iter()
                .position(|(_, slot)| slot.is_none())
                .unwrap_or_else(|| {
                    // No free slots, extend list
                    let n = self.weak_slots.len();
                    self.weak_slots.push((u16::MAX, None));
                    n
                });

            unsafe {
                node.as_mut().set_weakref_index(Some(i as u8));

                // Set slot `i` with node pointer and new version number
                let curr_ver = self.weak_slots.get_unchecked(i).0;
                let version = if curr_ver == u16::MAX {
                    1
                } else {
                    curr_ver + 1
                };
                *self.weak_slots.get_unchecked_mut(i) = (version, Some(gc_ref.head_ptr));

                GcWeak::new(i as _, version)
            }
        }
    }

    /// Upgrade weak reference
    pub fn upgrade<T>(&self, weak_ref: &GcWeak<T>) -> Option<GcRef<T>> {
        self.weak_slots
            .get(weak_ref.slot_index() as usize)
            .and_then(|(version, node)| {
                if *version == weak_ref.version() {
                    *node
                } else {
                    None
                }
            })
            .and_then(|ptr| {
                Some(GcRef {
                    head_ptr: ptr,
                    _marker: PhantomData,
                })
            })
    }
}

impl GcHead {
    /// Get weak index
    #[inline(always)]
    pub(crate) fn weakref_index(&self) -> Option<u8> {
        let w = (self.attrs >> 24) as u8;
        if w != u8::MAX { Some(w) } else { None }
    }

    /// Set weak index
    #[inline(always)]
    pub(crate) fn set_weakref_index(&mut self, index: Option<u8>) {
        self.attrs =
            (self.attrs & 0x00FF_FFFF) | ((index.map(|i| i).unwrap_or(u8::MAX) as u32) << 24);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test WeakRef's Clone and Copy traits
    #[test]
    fn test_weak_ref_clone_and_copy() {
        let weak1 = GcWeak::<String>::new(10, 1);

        // Test Clone
        let weak2 = weak1.clone();
        assert_eq!(weak1.slot_index(), weak2.slot_index());

        // Test Copy
        let weak3 = weak1;
        let weak4 = weak1; // Can be copied multiple times
        assert_eq!(weak3.slot_index(), weak4.slot_index());
    }

    /// Test WeakRef's equality
    #[test]
    fn test_weak_ref_equality() {
        let weak1 = GcWeak::<i32>::new(5, 1);
        let weak2 = GcWeak::<i32>::new(5, 1);
        let weak3 = GcWeak::<i32>::new(10, 1);

        // WeakRef with same slot should be equal
        assert_eq!(weak1, weak2);

        // WeakRef with different slots should not be equal
        assert_ne!(weak1, weak3);
    }

    /// Test WeakRef's upgrade method (interface test)
    #[test]
    fn test_weak_ref_upgrade_interface() {
        let heap = GcHeap::new();
        let weak_ref = GcWeak::<i32>::new(0, 1);

        // Test upgrade interface
        let result = weak_ref.upgrade(&heap);

        // Since there's no corresponding object, upgrade should return None
        debug_assert!(result.is_none());
    }

    /// Test WeakRef's type safety
    #[test]
    fn test_weak_ref_type_safety() {
        // Create WeakRef of different types
        let weak_i32 = GcWeak::<i32>::new(1, 1);
        let weak_string = GcWeak::<String>::new(1, 1);

        // Different type WeakRef with same slot should be equal (because only slot is compared)
        debug_assert_eq!(weak_i32.slot_index(), weak_string.slot_index());
        // But they are different types
        debug_assert_eq!(
            std::any::TypeId::of::<GcWeak<i32>>(),
            std::any::TypeId::of::<GcWeak<i32>>()
        );
        debug_assert_ne!(
            std::any::TypeId::of::<GcWeak<i32>>(),
            std::any::TypeId::of::<GcWeak<String>>()
        );
    }

    /// Test WeakRef's edge cases
    #[test]
    fn test_weak_ref_edge_cases() {
        // Test maximum slot value
        let weak_max = GcWeak::<i32>::new(u8::MAX, 1);

        debug_assert_eq!(weak_max.slot_index(), u8::MAX);

        // Test minimum slot value
        let weak_min = GcWeak::<i32>::new(0, 1);
        debug_assert_eq!(weak_min.slot_index(), 0);

        // Test WeakRef comparison with same slot but different types
        let weak_i32 = GcWeak::<i32>::new(5, 1);
        let weak_string = GcWeak::<String>::new(5, 1);

        // Although types are different, slots are the same, should be equal
        debug_assert_eq!(weak_i32.slot_index(), weak_string.slot_index());
    }

    /// Test WeakRef's serialization compatibility
    #[test]
    fn test_weak_ref_serialization_compatibility() {
        // Test WeakRef can be safely serialized and deserialized
        // This mainly tests structural layout stability

        let _weak_ref = GcWeak::<Vec<u8>>::new(42, 1);

        // Ensure struct size is fixed
        assert_eq!(
            std::mem::size_of::<GcWeak<Vec<u8>>>(),
            std::mem::size_of::<u32>()
        );

        // Ensure alignment is reasonable
        assert_eq!(
            std::mem::align_of::<GcWeak<Vec<u8>>>(),
            std::mem::align_of::<u32>()
        );
    }
}
