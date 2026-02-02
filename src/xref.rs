use std::ptr::NonNull;

use crate::{GcHead, GcHeap, GcPartitionId, GcTracer};

impl GcHead {
    /// Get cross reference partition
    #[cfg(debug_assertions)]
    pub fn xref_partition(&self) -> GcPartitionId {
        let p = (self.partition >> 16) as u16;
        GcPartitionId(p)
    }

    /// Set cross reference partition
    pub(crate) fn set_xref_partition(&mut self, pid: GcPartitionId) {
        if pid != GcPartitionId::NONE && pid != self.get_partition_id() {
            self.partition = (self.partition & 0x0000_FFFF) | ((pid.0 as u32) << 16);
        } else {
            self.partition = self.partition & 0x0000_FFFF;
        }
    }
}

impl GcHeap {
    /// Updates the node's cross-reference partition to a more general ancestor.
    /// Returns true if node's xref was updated, false if not.
    pub fn set_xref(&mut self, from_partition: GcPartitionId, node: NonNull<GcHead>) -> bool {
        debug_assert_ne!(from_partition, GcPartitionId::NONE);

        let (node_pid, xref0) = unsafe {
            let n = node.as_ref();
            (n.get_partition_id(), n.xref_partition())
        };
        debug_assert_ne!(node_pid, GcPartitionId::NONE);

        if from_partition == node_pid || from_partition == xref0 {
            return false;
        }

        // Find common parent of (from_partition, node_partition) as up
        let mut up = self.common_parent2(from_partition, node_pid);
        debug_assert_ne!(up, GcPartitionId::NONE);

        if up == node_pid || up == xref0 {
            return false;
        }

        if xref0 != GcPartitionId::NONE {
            // Find common parent of (from_partition, xref0)
            let up2 = self.common_parent2(from_partition, xref0);
            debug_assert_ne!(up2, GcPartitionId::NONE);

            if up2 == xref0 {
                return false;
            }
            up = up2;
        }

        // node.as_mut().set_xref_partition(up);

        // set xref (up) to node and resursively it's descendants
        let mut tr = GcTracer::new_internal(NonNull::from_ref(self), node_pid);
        tr.trace_internal(
            node,
            |mut n, _| unsafe {
                let x = n.as_ref().xref_partition();
                if x == xref0 {
                    n.as_mut().set_xref_partition(up);
                    true
                } else {
                    debug_assert_ne!(x, GcPartitionId::NONE);
                    let up3 = self.common_parent2(up, x);
                    if up3 != x {
                        n.as_mut().set_xref_partition(up3);
                        true
                    } else {
                        false
                    }
                }
            },
            true,
        );

        self.set_root_node(node, true);
        return true;

        // if xref0 == GcPartitionId::NONE {
        //     unsafe {
        //         node.as_mut().set_xref_partition(up);

        //         // resursively set xref of descendants
        //         let mut tr = GcTracer::new_internal(NonNull::from_ref(self), node_pid);
        //         tr.trace_internal(
        //             node,
        //             |mut n, _| {
        //                 if n.as_ref().xref_partition() == xref0 {
        //                     debug_assert!(!n.as_ref().is_root());
        //                     n.as_mut().set_xref_partition(up);
        //                     true
        //                 } else {
        //                     false
        //                 }
        //             },
        //             true,
        //         );
        //     }

        //     self.set_root_node(node, true);
        //     return true;
        // }

        // // Find common parent of (from_partition, xref0)
        // let up2 = self.common_parent2(from_partition, xref0);
        // debug_assert_ne!(up2, GcPartitionId::NONE);

        // // Update if the new common parent is more general than xref0
        // // (i.e., up2 is an ancestor of xref0 and not equal to xref0)
        // if up2 != xref0 {
        //     unsafe {
        //         node.as_mut().set_xref_partition(up2);

        //         // resursively set xref of descendants
        //         let mut tr = GcTracer::new_internal(NonNull::from_ref(self), node_pid);
        //         tr.trace_internal(
        //             node,
        //             |mut n, _| {
        //                 if n.as_ref().xref_partition() == xref0 {
        //                     debug_assert!(!n.as_ref().is_root());
        //                     n.as_mut().set_xref_partition(up);
        //                     true
        //                 } else {
        //                     false
        //                 }
        //             },
        //             true,
        //         );
        //     }

        //     self.set_root_node(node, true);
        //     return true;
        // }

        // false
    }
}

#[cfg(test)]
mod xref_tests {
    use std::ptr::NonNull;

    use super::*;

    /// 创建测试节点：使用heap.alloc创建真实节点，然后设置xref_partition
    fn create_test_node_with_xref(
        heap: &mut GcHeap,
        node_pid: GcPartitionId,
        xref_pid: GcPartitionId,
    ) -> NonNull<GcHead> {
        // 创建一个简单的i32节点
        let gc_ref = heap.alloc(node_pid, 42i32).unwrap();
        let mut head_ptr = gc_ref.head_ptr;

        // 设置xref_partition
        unsafe {
            head_ptr.as_mut().set_xref_partition(xref_pid);
        }

        head_ptr
    }

    /// 测试场景1: from_partition 比 node_partition 更上级, xref0 不存在
    /// 预期: 返回 true, xref 更新
    #[test]
    fn test_set_xref_no_existing_xref() {
        let mut heap = GcHeap::new();

        // 创建分区层次结构: Root -> NodePartition
        let root_id = heap.create_root_partition(4096);
        let node_pid = heap.create_sub_partition(root_id);

        // 创建 from_partition
        let from_pid = root_id;

        // 创建节点 (xref = NONE)
        let node = create_test_node_with_xref(&mut heap, node_pid, GcPartitionId::NONE);

        // 初始 xref 为 NONE
        unsafe {
            assert_eq!(node.as_ref().xref_partition(), GcPartitionId::NONE);
        }

        let result = heap.set_xref(from_pid, node);
        assert!(
            result,
            "set_xref should return true when xref does not exist"
        );

        unsafe {
            assert_eq!(node.as_ref().xref_partition(), from_pid);
        }
    }

    /// 测试场景2: from_partition 与 node_partition 是兄弟节点, xref0 是更上级
    /// 预期: 不更新 (common_parent(xref0, up) == xref0)
    #[test]
    fn test_set_xref_xref0_is_higher() {
        let mut heap = GcHeap::new();

        // 创建分区层次结构: Root -> NodePartition 和 Root -> FromPartition
        let root_id = heap.create_root_partition(4096);
        let node_pid = heap.create_sub_partition(root_id);
        let from_pid = heap.create_sub_partition(root_id);

        // 创建节点, xref = root_id
        let node = create_test_node_with_xref(&mut heap, node_pid, root_id);

        // 初始 xref 为 Root
        unsafe {
            assert_eq!(node.as_ref().xref_partition(), root_id);
        }

        // 调用 set_xref
        heap.set_xref(from_pid, node);

        // 预期: 不更新, xref 仍为 Root (common_parent(Root, Root) = Root = xref0)
        unsafe {
            assert_eq!(node.as_ref().xref_partition(), root_id);
        }
    }

    /// 测试场景3: from_partition 与 node_partition 是兄弟节点, xref0 是更下级
    /// 预期: 更新为 up (from_pid)
    #[test]
    fn test_set_xref_xref0_is_lower() {
        let mut heap = GcHeap::new();

        // 创建分区层次结构: Root -> FromPartition -> XrefChild
        let root_id = heap.create_root_partition(4096);
        let from_pid = heap.create_sub_partition(root_id);
        let xref0 = heap.create_sub_partition(from_pid);

        let node_pid = heap.create_sub_partition(root_id); // 兄弟节点

        // 创建节点, xref0 为 From 的子节点
        let node = create_test_node_with_xref(&mut heap, node_pid, xref0);

        // 初始 xref 为 xref0
        unsafe {
            assert_eq!(node.as_ref().xref_partition(), xref0);
        }

        // 调用 set_xref(from_partition=from_pid)
        // common_parent(node_pid, from_pid) = Root
        // common_parent(xref0, Root) = from_pid (因为 xref0 的父节点是 from_pid)
        // up = from_pid != xref0, 所以应该更新
        heap.set_xref(from_pid, node);

        // 预期: xref 更新为 from_pid
        unsafe {
            assert_eq!(node.as_ref().xref_partition(), from_pid);
        }
    }

    /// 测试场景4: xref0 是 node_partition 的祖先
    /// 预期: 更新为 up (Root)
    #[test]
    fn test_set_xref_xref0_is_ancestor_of_node() {
        let mut heap = GcHeap::new();

        // 创建分区层次结构: Root -> Xref0 -> NodeChild
        let root_id = heap.create_root_partition(4096);
        let xref0 = heap.create_sub_partition(root_id);
        let node_pid = heap.create_sub_partition(xref0); // node 是 xref0 的子节点

        let from_pid = heap.create_sub_partition(root_id); // 另一个分支

        // 创建节点, xref0 为 xref0
        let node = create_test_node_with_xref(&mut heap, node_pid, xref0);

        // 初始 xref 为 xref0
        unsafe {
            assert_eq!(node.as_ref().xref_partition(), xref0);
        }

        // 调用 set_xref(from_partition=from_pid)
        // common_parent(node_pid, from_pid) = Root
        // common_parent(xref0, Root) = Root (因为 xref0 的父节点是 Root)
        // up = Root != xref0, 所以应该更新
        heap.set_xref(from_pid, node);

        // 预期: xref 更新为 Root
        unsafe {
            assert_eq!(node.as_ref().xref_partition(), root_id);
        }
    }

    /// 测试场景5: from_partition 与 node_partition 相同
    /// 预期: 不更新 (up == node_pid)
    #[test]
    fn test_set_xref_same_partition() {
        let mut heap = GcHeap::new();

        let root_id = heap.create_root_partition(4096);
        let node_pid = heap.create_sub_partition(root_id);

        // 创建节点, xref 为 Root
        let node = create_test_node_with_xref(&mut heap, node_pid, root_id);

        unsafe {
            assert_eq!(node.as_ref().xref_partition(), root_id);
        }

        // 调用 set_xref(from_partition=node_pid), 相同分区
        heap.set_xref(node_pid, node);

        // 预期: 不更新
        unsafe {
            assert_eq!(node.as_ref().xref_partition(), root_id);
        }
    }

    /// 测试场景6: from_partition 是 node_partition 的下级
    /// 预期: 不更新 (up == node_pid)
    #[test]
    fn test_set_xref_from_is_lower() {
        let mut heap = GcHeap::new();

        let root_id = heap.create_root_partition(4096);
        let node_pid = heap.create_sub_partition(root_id);
        let from_pid = heap.create_sub_partition(node_pid); // from 是 node 的下级

        // 创建节点, xref 为 Root
        let node = create_test_node_with_xref(&mut heap, node_pid, root_id);

        unsafe {
            assert_eq!(node.as_ref().xref_partition(), root_id);
        }

        // 调用 set_xref(from_partition=from_pid)
        // common_parent(node_pid, from_pid) = node_pid
        // up == node_pid, 所以直接返回, 不更新
        heap.set_xref(from_pid, node);

        // 预期: 不更新
        unsafe {
            assert_eq!(node.as_ref().xref_partition(), root_id);
        }
    }

    /// 测试场景7: 复杂层次结构, 三个分区有共同祖先
    #[test]
    fn test_set_xref_complex_hierarchy() {
        let mut heap = GcHeap::new();

        // 创建分区层次结构:
        //       Root
        //      /    \
        //    A       B
        //   / \      \\
        //  A1  A2    B1
        let root_id = heap.create_root_partition(4096);
        let a_id = heap.create_sub_partition(root_id);
        let b_id = heap.create_sub_partition(root_id);
        let a1_id = heap.create_sub_partition(a_id);
        let _a2_id = heap.create_sub_partition(a_id);
        let b1_id = heap.create_sub_partition(b_id);

        // 场景: node 在 A1, xref 是 A, from 是 B1
        // common_parent(A1, B1) = Root
        // common_parent(A, Root) = Root
        // up = Root != A, 所以应该更新为 Root

        let node = create_test_node_with_xref(&mut heap, a1_id, a_id);

        unsafe {
            assert_eq!(node.as_ref().xref_partition(), a_id);
        }

        heap.set_xref(b1_id, node);

        unsafe {
            assert_eq!(node.as_ref().xref_partition(), root_id);
        }
    }
}
