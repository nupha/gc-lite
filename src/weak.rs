// SPDX-License-Identifier: MIT
// Copyright (c) 2025 John Ray <996351336@qq.com>

use std::marker::PhantomData;

use crate::{GcRef, heap::GcHeap};

/// Weak reference
pub struct GcWeak<T> {
    /// Slot index in weakrefs_list
    pub(crate) slot_index: u32,
    /// Version number, used to prevent conflicts from slot reuse
    pub(crate) version: u32,
    pub(crate) _marker: PhantomData<T>,
}

impl<T> Clone for GcWeak<T> {
    fn clone(&self) -> Self {
        Self {
            slot_index: self.slot_index,
            version: self.version,
            _marker: PhantomData,
        }
    }
}

impl<T> Copy for GcWeak<T> {}

impl<T> std::fmt::Debug for GcWeak<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GcWeak({}#{})", self.slot_index, self.version)
    }
}

impl<T> PartialEq for GcWeak<T> {
    fn eq(&self, other: &Self) -> bool {
        self.slot_index == other.slot_index && self.version == other.version
    }
}

impl<T> Eq for GcWeak<T> {}

impl<T> Default for GcWeak<T> {
    fn default() -> Self {
        Self {
            slot_index: u32::MAX,
            version: u32::MAX,
            _marker: Default::default(),
        }
    }
}

impl<T> GcWeak<T> {
    /// Upgrade weak reference to strong reference
    #[inline(always)]
    pub fn upgrade(&self, context: &GcHeap) -> Option<GcRef<T>> {
        context.upgrade(self)
    }
}

impl GcHeap {
    /// Create weak reference
    pub fn downgrade<T>(&mut self, gc_ref: &GcRef<T>) -> crate::weak::GcWeak<T> {
        unsafe {
            let node = gc_ref.head_ptr.as_ptr();

            if let Some(index) = (*node).weakref_index() {
                // Weakref already exists, reuse it
                let (ver, _ptr) = self.weak_list[index as usize];
                debug_assert!(!_ptr.is_none());
                GcWeak {
                    slot_index: index as u32,
                    version: ver,
                    _marker: PhantomData,
                }
            } else {
                // Find a free slot
                let i = self
                    .weak_list
                    .iter()
                    .position(|(_, slot)| slot.is_none())
                    .unwrap_or_else(|| {
                        // No free slots, extend list
                        let n = self.weak_list.len();
                        self.weak_list.push((1, None)); // Initial version number is 1
                        n
                    });

                (*node).set_weakref_index(Some(i));

                // Set slot to point to current object, increment version number
                let curr_ver = self.weak_list[i].0;
                let version = if curr_ver == u32::MAX {
                    1
                } else {
                    curr_ver + 1
                };
                self.weak_list[i] = (version, Some(gc_ref.head_ptr));

                GcWeak {
                    slot_index: i as u32,
                    version,
                    _marker: PhantomData,
                }
            }
        }
    }

    /// Upgrade weak reference
    pub fn upgrade<T>(&self, weak_ref: &GcWeak<T>) -> Option<GcRef<T>> {
        self.weak_list
            .get(weak_ref.slot_index as usize)
            .and_then(|(version, node)| {
                if *version == weak_ref.version {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Test WeakRef's Clone and Copy traits
    #[test]
    fn test_weak_ref_clone_and_copy() {
        let weak1 = GcWeak::<String> {
            slot_index: 10,
            version: 1,
            _marker: PhantomData,
        };

        // Test Clone
        let weak2 = weak1.clone();
        assert_eq!(weak1.slot_index, weak2.slot_index);

        // Test Copy
        let weak3 = weak1;
        let weak4 = weak1; // Can be copied multiple times
        assert_eq!(weak3.slot_index, weak4.slot_index);
    }

    /// Test WeakRef's equality
    #[test]
    fn test_weak_ref_equality() {
        let weak1 = GcWeak::<i32> {
            slot_index: 5,
            version: 1,
            _marker: PhantomData,
        };

        let weak2 = GcWeak::<i32> {
            slot_index: 5,
            version: 1,
            _marker: PhantomData,
        };

        let weak3 = GcWeak::<i32> {
            slot_index: 10,
            version: 1,
            _marker: PhantomData,
        };

        // WeakRef with same slot should be equal
        assert_eq!(weak1, weak2);

        // WeakRef with different slots should not be equal
        assert_ne!(weak1, weak3);
    }

    /// Test WeakRef's upgrade method (interface test)
    #[test]
    fn test_weak_ref_upgrade_interface() {
        let heap = GcHeap::new();
        let weak_ref = GcWeak::<i32> {
            slot_index: 0,
            version: 1,
            _marker: PhantomData,
        };

        // Test upgrade interface
        let result = weak_ref.upgrade(&heap);

        // Since there's no corresponding object, upgrade should return None
        debug_assert!(result.is_none());
    }

    /// Test WeakRef's type safety
    #[test]
    fn test_weak_ref_type_safety() {
        // Create WeakRef of different types
        let weak_i32 = GcWeak::<i32> {
            slot_index: 1,
            version: 1,
            _marker: PhantomData,
        };

        let weak_string = GcWeak::<String> {
            slot_index: 1,
            version: 1,
            _marker: PhantomData,
        };

        // Different type WeakRef with same slot should be equal (because only slot is compared)
        debug_assert_eq!(weak_i32.slot_index, weak_string.slot_index);
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
        let weak_max = GcWeak::<i32> {
            slot_index: u32::MAX,
            version: 1,
            _marker: PhantomData,
        };

        debug_assert_eq!(weak_max.slot_index, u32::MAX);

        // Test minimum slot value
        let weak_min = GcWeak::<i32> {
            slot_index: 0,
            version: 1,
            _marker: PhantomData,
        };

        debug_assert_eq!(weak_min.slot_index, 0);

        // Test WeakRef comparison with same slot but different types
        let weak_i32 = GcWeak::<i32> {
            slot_index: 5,
            version: 1,
            _marker: PhantomData,
        };

        let weak_string = GcWeak::<String> {
            slot_index: 5,
            version: 1,
            _marker: PhantomData,
        };

        // Although types are different, slots are the same, should be equal
        debug_assert_eq!(weak_i32.slot_index, weak_string.slot_index);
    }

    /// Test WeakRef's serialization compatibility
    #[test]
    fn test_weak_ref_serialization_compatibility() {
        // Test WeakRef can be safely serialized and deserialized
        // This mainly tests structural layout stability

        let _weak_ref = GcWeak::<Vec<u8>> {
            slot_index: 42,
            version: 1,
            _marker: PhantomData,
        };

        // Ensure struct size is fixed
        assert_eq!(
            std::mem::size_of::<GcWeak<Vec<u8>>>(),
            std::mem::size_of::<u32>() * 2 // slot_index + version
        );

        // Ensure alignment is reasonable
        assert_eq!(
            std::mem::align_of::<GcWeak<Vec<u8>>>(),
            std::mem::align_of::<u32>()
        );
    }
}
