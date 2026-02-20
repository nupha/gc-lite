// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{GcHead, GcHeap, GcPartitionId};

impl GcHead {
    /// Get cross reference scope
    pub fn xref_partition(&self) -> GcPartitionId {
        let p = (self.partition >> 16) as u16;
        GcPartitionId(p)
    }

    /// Set cross reference.
    ///
    /// # Return
    ///
    /// * true if node has xref set
    /// * false if node has xref unset
    pub(crate) fn set_xref(&mut self, xref: GcPartitionId) -> bool {
        if !xref.is_null() && xref != self.scope_id() {
            log::trace!("[set_xref] {xref:?} -> {self:?}");
            self.partition = (self.partition & 0x0000_FFFF) | ((xref.0 as u32) << 16);
            true
        } else {
            self.partition &= 0x0000_FFFF;
            false
        }
    }

    /// Unset cross reference
    #[inline(always)]
    pub fn unset_xref(&mut self) {
        self.set_xref(GcPartitionId::NONE);
    }
}

impl GcHeap {
    /// add nodes relationship for directed reference: from `master` to `slave`
    pub fn bind(&mut self, master: NonNull<GcHead>, slave: NonNull<GcHead>) {
        self.set_xref(unsafe { master.as_ref().scope_id() }, slave);
    }

    /// Updates the node's cross-reference partition to a more general ancestor.
    /// Returns true if node's xref was updated, false if not.
    pub fn set_xref(&mut self, xref: GcPartitionId, mut node: NonNull<GcHead>) -> bool {
        debug_assert!(self.partition(xref).is_some());

        let (node_pid, xref0) = unsafe {
            let n = node.as_ref();
            (n.scope_id(), n.xref_partition())
        };
        debug_assert!(self.partition(node_pid).is_some(), "{:?}", unsafe {
            node.as_ref()
        });

        if xref == node_pid || xref == xref0 {
            return false;
        }

        // Find common parent of (from_partition, node_partition) as up
        let mut up = self.common_parent2(xref, node_pid);
        debug_assert!(!up.is_null());

        if up == node_pid || up == xref0 {
            return false;
        }

        if !xref0.is_null() {
            // Find common parent of (from_partition, xref0)
            let up2 = self.common_parent2(xref, xref0);
            debug_assert!(!up2.is_null());

            if up2 == xref0 {
                return false;
            }
            up = up2;
        }

        unsafe {
            node.as_mut().set_xref(up);
        }

        self.set_root_node(node, true);
        true
    }
}

#[cfg(test)]
mod xref_tests {
    use std::ops::DerefMut;

    use super::*;
    use crate::{
        GcNode, GcRef,
        trace::{GcTracable, GcTraceCtx},
    };

    #[derive(Debug)]
    struct TestNode {
        children: Vec<GcRef<TestNode>>,
    }

    unsafe impl GcTracable for TestNode {
        fn trace(&self, tr: &mut GcTraceCtx) {
            for ch in &self.children {
                tr.add(*ch);
            }
        }
    }

    impl GcNode for TestNode {}

    crate::gc_type_register! {
        TestNode, drop_pass = 0;
    }

    fn alloc_node(heap: &mut GcHeap, pid: GcPartitionId) -> GcRef<TestNode> {
        heap.alloc(
            pid,
            TestNode {
                children: Vec::new(),
            },
        )
        .unwrap()
    }

    #[test]
    fn test_bind_sets_xref_to_common_parent_no_existing_xref() {
        let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LIST);

        let root_id = heap.create_root_partition(4096);
        let a_id = heap.create_sub_partition(root_id);
        let b_id = heap.create_sub_partition(root_id);

        let master = alloc_node(&mut heap, a_id);
        let slave = alloc_node(&mut heap, b_id);

        unsafe { (*slave.head_ptr.as_ptr()).unset_xref() };

        unsafe {
            assert_eq!(
                slave.head_ptr.as_ref().xref_partition(),
                GcPartitionId::NONE
            );
        }

        heap.bind(master.head_ptr, slave.head_ptr);

        unsafe {
            assert_eq!(slave.head_ptr.as_ref().xref_partition(), root_id);
            assert!(slave.head_ptr.as_ref().is_root());
        }
        heap.drop_partition(root_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    }

    #[test]
    fn test_bind_no_change_same_partition() {
        let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LIST);

        let root_id = heap.create_root_partition(4096);
        let a_id = heap.create_sub_partition(root_id);

        let master = alloc_node(&mut heap, a_id);
        let slave = alloc_node(&mut heap, a_id);

        unsafe {
            assert_eq!(
                slave.head_ptr.as_ref().xref_partition(),
                GcPartitionId::NONE
            );
        }

        heap.bind(master.head_ptr, slave.head_ptr);

        unsafe {
            assert_eq!(
                slave.head_ptr.as_ref().xref_partition(),
                GcPartitionId::NONE
            );
            assert!(!slave.head_ptr.as_ref().is_root());
        }
        heap.drop_partition(root_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    }

    #[test]
    fn test_set_xref_elevates_lower_existing_xref() {
        let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LIST);

        let root_id = heap.create_root_partition(4096);
        let a_id = heap.create_sub_partition(root_id);
        let a_child = heap.create_sub_partition(a_id);
        let b_id = heap.create_sub_partition(root_id);

        let master = alloc_node(&mut heap, a_id);
        let slave = alloc_node(&mut heap, b_id);

        unsafe { (*slave.head_ptr.as_ptr()).set_xref(a_child) };

        unsafe {
            assert_eq!(slave.head_ptr.as_ref().xref_partition(), a_child);
        }

        heap.bind(master.head_ptr, slave.head_ptr);

        unsafe {
            assert_eq!(slave.head_ptr.as_ref().xref_partition(), a_id);
            assert!(slave.head_ptr.as_ref().is_root());
        }
        heap.drop_partition(root_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    }

    #[test]
    fn test_set_xref_no_regression_when_existing_xref_higher() {
        let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LIST);

        let root_id = heap.create_root_partition(4096);
        let a_id = heap.create_sub_partition(root_id);
        let b_id = heap.create_sub_partition(root_id);

        let master = alloc_node(&mut heap, a_id);
        let slave = alloc_node(&mut heap, b_id);

        unsafe { (*slave.head_ptr.as_ptr()).set_xref(root_id) };

        unsafe {
            assert_eq!(slave.head_ptr.as_ref().xref_partition(), root_id);
        }

        heap.bind(master.head_ptr, slave.head_ptr);

        unsafe {
            assert_eq!(slave.head_ptr.as_ref().xref_partition(), root_id);
            assert!(!slave.head_ptr.as_ref().is_root());
        }
        heap.drop_partition(root_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    }

    #[test]
    fn test_set_xref_same_partition_no_update() {
        let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LIST);

        let root_id = heap.create_root_partition(4096);
        let a_id = heap.create_sub_partition(root_id);

        let node = alloc_node(&mut heap, a_id);

        unsafe {
            (*node.head_ptr.as_ptr()).set_xref(root_id);
            assert_eq!(node.head_ptr.as_ref().xref_partition(), root_id);
        }

        let updated = heap.set_xref(a_id, node.head_ptr);
        assert!(!updated);

        unsafe {
            assert_eq!(node.head_ptr.as_ref().xref_partition(), root_id);
            assert!(!node.head_ptr.as_ref().is_root());
        }
        heap.drop_partition(root_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    }

    #[test]
    fn test_set_xref_from_is_lower_no_update() {
        let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LIST);

        let root_id = heap.create_root_partition(4096);
        let node_pid = heap.create_sub_partition(root_id);
        let from_pid = heap.create_sub_partition(node_pid);

        let node = alloc_node(&mut heap, node_pid);

        unsafe { (*node.head_ptr.as_ptr()).set_xref(root_id) };

        unsafe {
            assert_eq!(node.head_ptr.as_ref().xref_partition(), root_id);
        }

        let updated = heap.set_xref(from_pid, node.head_ptr);
        assert!(!updated);

        unsafe {
            assert_eq!(node.head_ptr.as_ref().xref_partition(), root_id);
            assert!(!node.head_ptr.as_ref().is_root());
        }
        heap.drop_partition(root_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    }

    #[test]
    fn test_multiple_bind_converges_to_common_parent() {
        let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LIST);

        let root_id = heap.create_root_partition(4096);
        let a_id = heap.create_sub_partition(root_id);
        let b_id = heap.create_sub_partition(root_id);
        let a1_id = heap.create_sub_partition(a_id);
        let a2_id = heap.create_sub_partition(a_id);
        let b1_id = heap.create_sub_partition(b_id);

        let master_a2 = alloc_node(&mut heap, a2_id);
        let master_b1 = alloc_node(&mut heap, b1_id);
        let mut node = alloc_node(&mut heap, a1_id);

        node.deref_mut().children.push(master_a2);

        unsafe {
            (*node.head_ptr.as_ptr()).set_xref(a_id);
            assert_eq!(node.head_ptr.as_ref().xref_partition(), a_id);
        }

        heap.bind(master_b1.head_ptr, node.head_ptr);

        unsafe {
            assert_eq!(node.head_ptr.as_ref().xref_partition(), root_id);
            assert!(node.head_ptr.as_ref().is_root());
        }
        heap.drop_partition(root_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    }
}
