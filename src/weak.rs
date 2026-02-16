// SPDX-License-Identifier: MIT
// Copyright (c) 2025 John Ray <996351336@qq.com>

use std::marker::PhantomData;

use crate::{GcNode, GcRef, heap::GcHeap};

/// bit 16-31: slot index in weak_list
/// bit 0-15:  version
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct GcWeakRawId(u32);

impl GcWeakRawId {
    pub(crate) const NULL: Self = Self(0);

    pub const fn index(&self) -> u16 {
        (self.0 >> 16) as u16
    }

    pub const fn version(&self) -> u16 {
        self.0 as u16
    }

    pub const fn is_null(&self) -> bool {
        self.version() == 0
    }
}

/// Weak reference
#[derive(PartialEq)]
#[repr(transparent)]
pub struct GcWeak<T: GcNode> {
    /// bit 16-31: slot index in weak_list
    /// bit 0-15:  version
    pub(crate) weak_id: GcWeakRawId,

    pub(crate) _marker: PhantomData<T>,
}

impl<T: GcNode> Clone for GcWeak<T> {
    fn clone(&self) -> Self {
        Self {
            weak_id: self.weak_id,
            _marker: PhantomData,
        }
    }
}

impl<T: GcNode> Copy for GcWeak<T> {}

impl<T: GcNode> Default for GcWeak<T> {
    fn default() -> Self {
        Self {
            weak_id: GcWeakRawId::NULL,
            _marker: PhantomData,
        }
    }
}

impl<T: GcNode> std::fmt::Debug for GcWeak<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GcWeak({}#{})", self.index(), self.version())
    }
}

impl<T: GcNode> GcWeak<T> {
    pub(crate) fn new(index: u16, version: u16) -> Self {
        debug_assert!(version > 0);
        Self {
            weak_id: GcWeakRawId(((index as u32) << 16) | (version as u32)),
            _marker: PhantomData,
        }
    }

    pub(crate) fn from_id(weak_id: GcWeakRawId) -> Self {
        Self {
            weak_id,
            _marker: PhantomData,
        }
    }

    /// get weakref index
    #[inline(always)]
    pub fn index(&self) -> u16 {
        self.weak_id.index()
    }

    #[inline(always)]
    pub fn version(&self) -> u16 {
        self.weak_id.version()
    }

    /// Upgrade weak reference to strong reference
    #[inline(always)]
    pub fn upgrade(&self, heap: &GcHeap) -> Option<GcRef<T>> {
        heap.upgrade(self)
    }
}

impl GcHeap {
    /// Create weak reference.
    pub fn downgrade<T: GcNode>(&mut self, gc_ref: &GcRef<T>) -> GcWeak<T> {
        let node = unsafe {
            let mut h = gc_ref.head_ptr;
            h.as_mut()
        };

        if !node.weak_id.is_null() {
            // Weakref already exists, reuse it
            #[cfg(debug_assertions)]
            {
                debug_assert!((node.weak_id.index() as usize) < self.weak_slots.len());
                let (ver, ptr) = self.weak_slots[node.weak_id.index() as usize];
                debug_assert!(
                    ver == node.weak_id.version() && ptr.is_some_and(|p| p == gc_ref.head_ptr)
                );
            }
            GcWeak::from_id(node.weak_id)
        } else {
            if self.weak_slots.len() == u16::MAX as usize {
                panic!("too may weakrefs");
            }

            // Get free slot
            let i = self
                .weak_slots
                .iter()
                .position(|(_, slot)| slot.is_none())
                .unwrap_or_else(|| {
                    let n = self.weak_slots.len();
                    self.weak_slots.push((0, None));
                    n
                });

            unsafe {
                // Set slot `i` with node pointer and new version number
                let curr_ver = self.weak_slots.get_unchecked(i).0;
                let version = if curr_ver == u16::MAX {
                    1
                } else {
                    curr_ver + 1
                };

                let weak = GcWeak::new(i as _, version);
                *self.weak_slots.get_unchecked_mut(i) = (version, Some(gc_ref.head_ptr));
                node.weak_id = weak.weak_id;

                weak
            }
        }
    }

    /// Upgrade weak reference
    pub fn upgrade<T: GcNode>(&self, weak_ref: &GcWeak<T>) -> Option<GcRef<T>> {
        if !weak_ref.weak_id.is_null() {
            self.weak_slots
                .get(weak_ref.index() as usize)
                .and_then(|(version, node)| {
                    if *version == weak_ref.version() {
                        *node
                    } else {
                        None
                    }
                })
                .map(|ptr| GcRef {
                    head_ptr: ptr,
                    _marker: PhantomData,
                })
        } else {
            None
        }
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
        assert_eq!(weak1.index(), weak2.index());

        // Test Copy
        let weak3 = weak1;
        let weak4 = weak1; // Can be copied multiple times
        assert_eq!(weak3.index(), weak4.index());
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
        debug_assert_eq!(weak_i32.index(), weak_string.index());
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
        let weak_max = GcWeak::<i32>::new(u16::MAX, 1);

        debug_assert_eq!(weak_max.index(), u16::MAX);

        // Test minimum slot value
        let weak_min = GcWeak::<i32>::new(0, 1);
        debug_assert_eq!(weak_min.index(), 0);

        // Test WeakRef comparison with same slot but different types
        let weak_i32 = GcWeak::<i32>::new(5, 1);
        let weak_string = GcWeak::<String>::new(5, 1);

        // Although types are different, slots are the same, should be equal
        debug_assert_eq!(weak_i32.index(), weak_string.index());
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
