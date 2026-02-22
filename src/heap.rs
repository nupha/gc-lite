// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::{collections::HashMap, ptr::NonNull};

use crate::{
    GcNode, GcRef,
    gctype::GcTypeRegistry,
    node::{GcHead, GcTriColor},
    partition::{GcPartition, GcPartitionId},
    trace::GcTraceCtx,
};

pub struct GcHeap {
    /// Partition management
    pub(super) partitions: HashMap<GcPartitionId, GcPartition>,
    /// Weak reference list, each slot stores (version, GcHeader)
    pub(super) weak_slots: Vec<(u16, Option<NonNull<GcHead>>)>,
    /// Registered GC data type info
    pub(super) node_dtypes: &'static GcTypeRegistry,

    /// User provided opaque raw pointer
    opaque: *mut u8,

    #[cfg(debug_assertions)]
    pub(crate) dbg_dropping_root_partition: Option<GcPartitionId>,
    #[cfg(debug_assertions)]
    pub(crate) dbg_living_nodes: std::collections::HashSet<NonNull<GcHead>>,
}

impl Drop for GcHeap {
    fn drop(&mut self) {
        // heap world is gone, dealloc all nodes live in it, regardless their status.
        log::trace!("[heap::drop]");

        let mut kv = std::mem::take(&mut self.partitions);

        for (_, mut partition) in kv.drain() {
            if let Some(link) = partition.nodes.take() {
                self.dispose_all_nodes(link, Self::DUMMY_DISPOSE_CALLBACK);
            }
        }

        #[cfg(debug_assertions)]
        debug_assert!(
            self.dbg_living_nodes.is_empty(),
            "[O.o][heap drop] leaked nodes {:?}",
            self.dbg_living_nodes
        );
    }
}

impl GcHeap {
    //
    // default callbacks
    //
    pub const DUMMY_MIGRATE_CALLBACK: fn(&GcHeap, &GcHead, GcPartitionId) = |_, _, _| {};
    pub const DUMMY_DISPOSE_CALLBACK: fn(&GcHeap, &GcHead) = |_, _| {};

    /// Create a new garbage collection heap with an explicit GC type registry
    pub fn new(registry: &'static GcTypeRegistry) -> Self {
        Self {
            partitions: HashMap::new(),
            weak_slots: Vec::new(),
            opaque: std::ptr::null_mut(),
            node_dtypes: registry,

            #[cfg(debug_assertions)]
            dbg_dropping_root_partition: None,
            #[cfg(debug_assertions)]
            dbg_living_nodes: std::collections::HashSet::with_capacity(128),
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

    /// Check if the given partition ID is an ancestor of the specified partition
    ///
    /// # Parameters
    /// - `this`: The target partition to check
    /// - `ancestor`: The potential ancestor partition ID
    ///
    /// # Returns
    /// `true` if `ancestor` is an ancestor of `this`, `false` otherwise
    pub fn is_ancestor_of(&self, this: GcPartitionId, ancestor: GcPartitionId) -> bool {
        debug_assert_ne!(this, GcPartitionId::NONE);
        debug_assert_ne!(ancestor, GcPartitionId::NONE);

        let mut current_id = this;
        while current_id != GcPartitionId::NONE {
            if current_id == ancestor {
                return true;
            } else if let Some(p) = self.partitions.get(&current_id) {
                current_id = p.parent;
            } else {
                #[cfg(debug_assertions)]
                unreachable!();
                #[cfg(not(debug_assertions))]
                break;
            }
        }

        false
    }

    pub fn drop_partition(
        &mut self,
        partition_id: GcPartitionId,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        if partition_id.is_null() {
            return 0;
        }

        #[cfg(debug_assertions)]
        {
            self.dbg_dropping_root_partition = Some(partition_id);
        }

        let mut freed_bytes = 0;
        let mut to_drop = vec![partition_id];
        let mut i = 0;

        while i < to_drop.len() {
            let current_id = to_drop[i];
            i += 1;

            if let Some(mut partition) = self.partitions.remove(&current_id) {
                // Add children to the drop list
                to_drop.extend_from_slice(&partition.children);

                // Dispose all nodes in the current partition
                if let Some(head) = partition.nodes.take() {
                    freed_bytes += self.dispose_all_nodes(head, &on_dispose);
                }

                // Remove from parent's children list
                if !partition.parent.is_null() {
                    if let Some(parent_partition) = self.partitions.get_mut(&partition.parent) {
                        if let Some(pos) = parent_partition
                            .children
                            .iter()
                            .position(|&id| id == current_id)
                        {
                            parent_partition.children.swap_remove(pos);
                        }
                    }
                }
            }
        }

        #[cfg(debug_assertions)]
        {
            self.dbg_dropping_root_partition = None;
        }

        freed_bytes
    }

    /// Attach a node to partition's nodes chain.
    ///
    /// # Note
    ///
    /// This method **DO NOT** increase partitions' mem_use.
    #[inline]
    pub(crate) fn attach_node(&mut self, partition_id: GcPartitionId, mut node: NonNull<GcHead>) {
        debug_assert!(!partition_id.is_null());

        unsafe {
            debug_assert!(node.as_ref().scope_id().is_null());
            debug_assert!(node.as_ref().next.is_none());
            debug_assert!(node.as_ref().xref().is_null());

            node.as_mut().set_scope_id(partition_id);
        }

        let par = self.partitions.get_mut(&partition_id).unwrap();
        let link_head = par.nodes.take();
        unsafe {
            node.as_mut().next = link_head;
        }
        par.nodes = Some(node);
    }

    /// Set/unset a node to be root
    pub fn set_root_node(&mut self, mut node: NonNull<GcHead>, is_root: bool) {
        unsafe {
            node.as_mut().set_root(is_root);

            let pid = node.as_ref().scope_id();
            if let Some(partition) = self.partitions.get_mut(&pid) {
                if is_root {
                    // Add to partition's root object list
                    if !partition.root_nodes.contains(&node) {
                        partition.root_nodes.push(node);
                    }
                } else {
                    // Remove from partition's root object list
                    if let Some(pos) = partition.root_nodes.iter().position(|&r| r == node) {
                        partition.root_nodes.swap_remove(pos);
                    }
                }
            }
        }
    }

    /// Set/unset a gc_ref to be root
    #[inline(always)]
    pub fn set_root<T: GcNode>(&mut self, gc_ref: GcRef<T>, is_root: bool) {
        self.set_root_node(gc_ref.head_ptr, is_root);
    }

    pub fn get_roots(
        &self,
        partition_id: GcPartitionId,
    ) -> impl Iterator<Item = NonNull<GcHead>> + '_ {
        self.partitions
            .get(&partition_id)
            .map(|p| p.root_nodes.iter().copied())
            .into_iter()
            .flatten()
    }

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

        let mut ctx = GcTraceCtx::new(self, true);
        ctx.trace_iter(stack.iter().copied(), GcTraceCtx::MARK_FUNC);

        let b = unsafe { node.as_ref().color() != GcTriColor::White };

        // Reset all nodes to white for the next GC cycle
        for p_id in self.partition_ids() {
            for mut n in self.nodes(p_id) {
                unsafe {
                    n.as_mut().set_color(GcTriColor::White);
                }
            }
        }

        b
    }

    /// Update memory usage with rollup to parent partitions
    ///
    /// # Parameters
    /// - `id`: Partition ID
    /// - `delta`: Size change (positive to add, negative to subtract)
    ///
    /// # Returns
    /// Updated memory usage of the specified partition
    pub(crate) fn update_mem_use(&mut self, id: GcPartitionId, delta: i32) -> usize {
        let mut cur_id = id;
        let mut res = 0;

        while cur_id != GcPartitionId::NONE {
            if let Some(par) = self.partitions.get_mut(&cur_id) {
                if delta >= 0 {
                    par.memory_used += delta as usize;
                } else {
                    debug_assert!(par.memory_used >= (-delta) as usize);
                    par.memory_used -= (-delta) as usize;
                }
                if cur_id == id {
                    res = par.memory_used;
                }
                cur_id = par.parent;
            } else {
                break;
            }
        }

        res
    }
}

#[cfg(test)]
mod heap_tests {
    use crate::{GcNode, trace::GcTracable};

    use super::*;

    #[derive(Debug)]
    struct Node {
        next: Option<GcRef<Node>>,
        value: i32,
    }

    unsafe impl GcTracable for Node {
        fn trace(&self, tr: &mut GcTraceCtx) {
            if let Some(next) = self.next {
                tr.add(next);
            }
        }
    }

    crate::gc_type_register! {
        Node, drop_pass = 0;
    }

    #[test]
    fn test_is_node_reachable() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
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
