// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::HashMap, ptr::NonNull};

use crate::{
    GcTraceRestrict, GcTracer,
    node::{GcHead, GcRef},
    partition::{GcPartitionId, GcPartitionMgr},
    trace::GcTracable,
    type_registry::TypeRegistry,
};

pub struct GcHeap {
    /// Partition management
    pub(super) mgr: GcPartitionMgr,
    /// LUT: Object list heads for each partition
    pub(super) partition_nodes: HashMap<GcPartitionId, Option<NonNull<GcHead>>>,
    /// LUT: Root object lists for each partition
    pub(super) partition_root_nodes: HashMap<GcPartitionId, Vec<NonNull<GcHead>>>,
    /// Weak reference list, each slot stores (version, GcHeader)
    pub(super) weak_slots: Vec<(u16, Option<NonNull<GcHead>>)>,
    /// Type registry
    pub(super) gc_data_types: TypeRegistry,

    #[cfg(debug_assertions)]
    pub(crate) dbg_living_nodes: std::collections::HashSet<NonNull<GcHead>>,

    /// User provided opaque raw pointer
    opaque: *mut u8,
}

impl Drop for GcHeap {
    fn drop(&mut self) {
        // heap world is gone, dealloc all nodes live in it, regardless their status.
        log::trace!("[heap::drop]");

        let mut kv = std::mem::take(&mut self.partition_nodes);

        for (_, link) in kv.drain() {
            if let Some(link) = link {
                self.dispose_all_nodes(link, Self::DUMMY_DISPOSE_CALLBACK);
            }
        }

        debug_assert!(
            self.dbg_living_nodes.is_empty(),
            "[O.o] has leaked nodes {:?}",
            self.dbg_living_nodes
        );
    }
}

impl GcHeap {
    pub const DUMMY_MIGRATE_CALLBACK: fn(&GcHead, GcPartitionId) = |_, _| {};
    pub const DUMMY_DISPOSE_CALLBACK: fn(&GcHead) = |_| {};

    /// Create a new garbage collection heap
    pub fn new() -> Self {
        let partitions = GcPartitionMgr::new();

        Self {
            mgr: partitions,
            partition_nodes: HashMap::with_capacity(8),
            partition_root_nodes: HashMap::with_capacity(8),
            weak_slots: Vec::new(),
            gc_data_types: TypeRegistry::new(),
            opaque: std::ptr::null_mut(),

            #[cfg(debug_assertions)]
            dbg_living_nodes: std::collections::HashSet::with_capacity(1024),
        }
    }

    #[inline(always)]
    pub const fn opaque(&self) -> *mut u8 {
        self.opaque
    }

    pub const fn set_opaque(&mut self, opaque: *mut u8) {
        self.opaque = opaque;
    }

    /// Get garbage collection threshold for partition (bytes)
    ///
    /// A return value of 0 means automatic GC is disabled
    pub fn gc_threshold(&self, partition_id: GcPartitionId) -> Option<usize> {
        self.partition(partition_id)
            .map(|partition| partition.gc_threshold())
    }

    /// Set garbage collection threshold for partition (in bytes)
    ///
    /// # Parameters
    /// - `partition_id`: Partition ID
    /// - `threshold`: New garbage collection threshold (bytes)
    ///   - A value of 0 disables automatic GC
    ///   - If threshold exceeds partition memory limit, it's automatically set to the memory limit
    ///
    /// # Notes
    /// - If the partition doesn't exist, this method does nothing
    pub fn set_gc_threshold(&mut self, partition_id: GcPartitionId, threshold: usize) {
        if let Some(partition) = self.partition_mut(partition_id) {
            partition.set_gc_threshold(if threshold > 0 {
                let lim = partition.memory_limit();
                if lim > 0 && threshold > lim {
                    lim
                } else {
                    threshold
                }
            } else {
                threshold
            });
        }
    }

    /// Attach a node to partition's nodes chain.
    ///
    /// # Note
    ///
    /// This method **DO NOT** increase partitions' mem_use.
    #[inline]
    pub(crate) fn attach(&mut self, partition_id: GcPartitionId, mut node: NonNull<GcHead>) {
        debug_assert!(!partition_id.is_null());
        debug_assert!(self.partition_nodes.contains_key(&partition_id));

        unsafe {
            debug_assert!(node.as_ref().scope_id().is_null());
            debug_assert!(node.as_ref().next.is_none());

            node.as_mut().set_partition_id(partition_id);

            let cur_head = self
                .partition_nodes
                .remove(&partition_id)
                .unwrap_unchecked();

            node.as_mut().next = cur_head;
            self.partition_nodes.insert(partition_id, Some(node));
        }
    }

    // /// Remove a node from partition
    // pub(crate) fn detach(&mut self, node: NonNull<GcHead>) {
    //     let partition_id = unsafe { node.as_ref().get_partition_id() };

    //     if !partition_id.is_null() {
    //         let chain = self.partition_nodes.get_mut(&partition_id).unwrap();

    //         let mut current = *chain;
    //         let mut prev: Option<NonNull<GcHead>> = None;

    //         while let Some(header) = current {
    //             unsafe {
    //                 if header == node {
    //                     // take out from chain
    //                     if let Some(mut p) = prev {
    //                         p.as_mut().next = header.as_ref().next;
    //                     } else {
    //                         *chain = header.as_ref().next;
    //                     }

    //                     if node.as_ref().is_root() {
    //                         if let Some(roots) = self.partition_root_nodes.get_mut(&partition_id) {
    //                             roots.retain(|p| node != *p);
    //                         }
    //                         (*node.as_ptr()).set_root(false);
    //                     }

    //                     // clear partition id
    //                     (*node.as_ptr()).partition = GcPartitionId::NONE.0 as _;

    //                     return;
    //                 }

    //                 prev = Some(header);
    //                 current = header.as_ref().next;
    //             }
    //         }

    //         #[cfg(debug_assertions)]
    //         unreachable!("node not exist");
    //     }
    // }

    /// Set/unset a node to be root
    pub fn set_root_node(&mut self, node: NonNull<GcHead>, is_root: bool) {
        unsafe {
            let pid = (*node.as_ptr()).scope_id();

            (*node.as_ptr()).set_root(is_root);

            if is_root {
                // Add to partition's root object list, create if doesn't exist
                let roots = self
                    .partition_root_nodes
                    .entry(pid)
                    .or_insert_with(|| Vec::with_capacity(8));
                if !roots.contains(&node) {
                    roots.push(node);
                }
            } else {
                // Remove from partition's root object list
                if let Some(roots) = self.partition_root_nodes.get_mut(&pid) {
                    if let Some(pos) = roots.iter().position(|&r| r == node) {
                        roots.swap_remove(pos);
                    }
                }
            }
        }
    }

    /// Set/unset a gc_ref to be root
    #[inline(always)]
    pub fn set_root<T: GcTracable>(&mut self, gc_ref: GcRef<T>, is_root: bool) {
        self.set_root_node(gc_ref.head_ptr, is_root);
    }

    // /// Safely manually release an object
    // ///
    // /// This method performs GC mark verification before release to ensure the object is not referenced by other objects.
    // /// If the object is referenced, it returns an error to prevent dangling pointer issues.
    // ///
    // /// # 参数
    // /// - `gc_ref`: 要释放的垃圾回收引用
    // ///
    // /// # Return Value
    // /// - `Ok(())`: Release successful
    // /// - `Err(GcError::InvalidReference)`: Object is not allocated from this context or is referenced by other objects
    // /// - `Err(GcError::PartitionNotFound)`: The partition where the object is located does not exist
    // ///
    // /// # 注意
    // /// - 如果对象是根对象，会先将其从根对象列表中移除
    // /// - 释放后，该引用将变为无效，不应再使用
    // pub fn free<T>(&mut self, gc_ref: GcRef<T>) -> GcResult<usize> {
    //     // Perform GC mark verification to check if object is referenced
    //     if self.is_node_referenced(gc_ref)? {
    //         Err(GcError::InvalidReference)
    //     } else {
    //         // Object is not referenced, safe to release
    //         unsafe { self.free_unchecked(gc_ref) }
    //     }
    // }

    // /// Unsafe quick release of an object
    // ///
    // /// This method does not check if the object is referenced by other objects, it releases directly.
    // /// If the object is being referenced, it will cause dangling pointer and memory safety issues.
    // ///
    // /// # Safety
    // /// The caller must ensure that no other objects reference this object, otherwise it will cause undefined behavior.
    // ///
    // /// # 参数
    // /// - `gc_ref`: 要释放的垃圾回收引用
    // ///
    // /// # Return Value
    // /// - `Ok(())`: Release successful
    // /// - `Err(GcError::InvalidReference)`: Object is not allocated from this context
    // /// - `Err(GcError::PartitionNotFound)`: The partition where the object is located does not exist
    // ///
    // /// # 注意
    // /// - 如果对象是根对象，会先将其从根对象列表中移除
    // /// - 释放后，该引用将变为无效，不应再使用
    // pub unsafe fn free_unchecked<T>(&mut self, gc_ref: GcRef<T>) -> GcResult<usize> {
    //     let header = gc_ref.head_ptr();
    //     let partition_id = unsafe { header.as_ref().get_partition_id() };
    //     debug_assert_ne!(partition_id, GcPartitionId::NONE);

    //     if !self.contains(header) {
    //         // not allocated in this heap
    //         return Err(GcError::InvalidReference);
    //     }

    //     // If object is a root object, unset root
    //     if let Some(roots) = self.partition_roots.get_mut(&partition_id) {
    //         if let Some(i) = roots.iter().position(|&r| r == header) {
    //             roots.swap_remove(i);
    //         }
    //     }

    //     self.detach(header);
    //     unsafe { Ok(self.dispose(header)) }
    // }

    /// Check if `node` was allocated in this heap
    pub fn contains(&self, node: NonNull<GcHead>) -> bool {
        self.nodes(unsafe { node.as_ref().scope_id() })
            .any(|p| p == node)
    }

    /// 检测一个节点是否从指定的起始节点开始能被追踪到
    ///
    /// # 参数
    /// - `node`: 要检测的目标节点
    /// - `starts`: 起始节点的迭代器
    ///
    /// # 返回值
    /// - `true`: 如果从任意起始节点开始，通过追踪引用关系能找到目标节点
    /// - `false`: 如果从所有起始节点都无法追踪到目标节点
    pub fn is_node_reachable(
        &mut self,
        node: NonNull<GcHead>,
        starts: impl Iterator<Item = NonNull<GcHead>>,
    ) -> bool {
        use std::collections::HashSet;

        // 如果目标节点不在堆中，直接返回 false
        if !self.contains(node) {
            return false;
        }

        let partition_id = unsafe { node.as_ref().scope_id() };

        // 用于记录已访问的节点，避免循环引用导致的无限递归
        let mut visited = HashSet::new();
        // 使用栈进行深度优先搜索
        let mut stack: Vec<NonNull<GcHead>> = Vec::new();

        for start in starts {
            if start == node {
                return true;
            }
            if !self.contains(start) {
                continue;
            }

            if unsafe { start.as_ref().scope_id() } == partition_id {
                stack.push(start);
                visited.insert(start);
            }
        }

        let mut tr = self.tracer(GcTraceRestrict::Collect(partition_id));
        tr.trace_iter(stack.iter().copied(), GcTracer::MARK_FUNC);

        let b = unsafe { node.as_ref().is_marked() };
        tr.clear_marked_flag();

        b
    }
}

#[cfg(test)]
mod heap_tests {
    use crate::trace::GcTraceOp;

    use super::*;

    #[test]
    fn test_is_node_reachable() {
        use crate::trace::GcTracable;

        // 定义一个简单的结构体，包含对其他 GC 对象的引用
        #[derive(Debug)]
        struct Node {
            next: Option<GcRef<Node>>,
            value: i32,
        }

        unsafe impl GcTracable for Node {
            fn trace(&self, mut tr: GcTraceOp) {
                if let Some(next) = self.next {
                    tr.add(next);
                }
            }
        }

        let mut heap = GcHeap::new();
        let partition_id = heap.create_root_partition(4096);

        // 创建三个节点：A -> B -> C
        let node_c: GcRef<Node> = heap
            .alloc(
                partition_id,
                Node {
                    next: None,
                    value: 3,
                },
            )
            .unwrap();
        let node_b: GcRef<Node> = heap
            .alloc(
                partition_id,
                Node {
                    next: Some(node_c),
                    value: 2,
                },
            )
            .unwrap();
        let node_a: GcRef<Node> = heap
            .alloc(
                partition_id,
                Node {
                    next: Some(node_b),
                    value: 1,
                },
            )
            .unwrap();

        // 测试 1: 从 A 开始，应该能追踪到 B 和 C
        let starts = vec![node_a.head_ptr].into_iter();
        assert!(
            heap.is_node_reachable(node_b.head_ptr, starts.clone()),
            "B should be reachable from A"
        );
        assert!(
            heap.is_node_reachable(node_c.head_ptr, starts,),
            "C should be reachable from A"
        );

        // 测试 2: 从 B 开始，应该能追踪到 C，但不能追踪到 A
        let starts = vec![node_b.head_ptr].into_iter();
        assert!(
            heap.is_node_reachable(node_c.head_ptr, starts.clone()),
            "C should be reachable from B"
        );

        // 测试 3: 从 C 开始，不能追踪到 A 或 B
        let starts = vec![node_c.head_ptr].into_iter();
        assert!(
            !heap.is_node_reachable(node_a.head_ptr, starts.clone()),
            "A should not be reachable from C"
        );
        assert!(
            !heap.is_node_reachable(node_b.head_ptr, starts),
            "B should not be reachable from C"
        );

        // 测试 4: 从 A 开始，节点自身应该是可达的
        let starts = vec![node_a.head_ptr].into_iter();
        assert!(
            heap.is_node_reachable(node_a.head_ptr, starts),
            "A should be reachable from itself"
        );

        // 测试 5: 空起始迭代器应该返回 false
        let starts = vec![].into_iter();
        assert!(
            !heap.is_node_reachable(node_a.head_ptr, starts),
            "No node should be reachable from empty starts"
        );

        // 测试 6: 创建不相关的节点 D，从 A 开始不能追踪到 D
        let node_d: GcRef<Node> = heap
            .alloc(
                partition_id,
                Node {
                    next: None,
                    value: 4,
                },
            )
            .unwrap();
        let starts = vec![node_a.head_ptr].into_iter();
        assert!(
            !heap.is_node_reachable(node_d.head_ptr, starts),
            "D should not be reachable from A"
        );
    }
}
