// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::GcPartitionId;
use crate::heap::GcHeap;
use crate::node::GcHead;

/// Node iterator
///
/// Used to traverse all GC nodes in a specified partition
/// Returns GcHead pointer, doesn't care about specific type
pub struct NodeIterator<'a> {
    current: Option<NonNull<GcHead>>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> NodeIterator<'a> {
    /// Create iterator from chain head
    pub(crate) fn new(head: Option<NonNull<GcHead>>) -> Self {
        Self {
            current: head,
            _marker: std::marker::PhantomData,
        }
    }

    pub(crate) fn from_heap(heap: &'a GcHeap, partition_id: GcPartitionId) -> Self {
        Self::new(heap.partition_heads.get(&partition_id).copied().flatten())
    }
}

impl<'a> Iterator for NodeIterator<'a> {
    type Item = NonNull<GcHead>;

    fn next(&mut self) -> Option<Self::Item> {
        let cur = self.current?;
        unsafe {
            self.current = (*cur.as_ptr()).next;
        }
        Some(cur)
    }
}

impl GcHeap {
    /// Get node iterator for specified partition
    #[inline(always)]
    pub fn nodes_iter(&self, partition_id: GcPartitionId) -> NodeIterator<'_> {
        NodeIterator::from_heap(self, partition_id)
    }
}
