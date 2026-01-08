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
    /// Create iterator from partition list head
    pub(crate) fn new(partition_head: Option<NonNull<GcHead>>) -> Self {
        Self {
            current: partition_head,
            _marker: std::marker::PhantomData,
        }
    }

    /// Create iterator from GcContext and partition ID
    pub(crate) fn from_context(context: &'a GcHeap, partition_id: GcPartitionId) -> Self {
        let partition_head = context
            .partition_heads
            .get(&partition_id)
            .copied()
            .flatten();

        Self::new(partition_head)
    }
}

impl<'a> Iterator for NodeIterator<'a> {
    type Item = NonNull<GcHead>;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.current?;
        unsafe {
            let result = current;
            self.current = (*current.as_ptr()).next;
            Some(result)
        }
    }
}

impl GcHeap {
    /// Get node iterator for specified partition
    #[inline(always)]
    pub fn partition_node_iter(&self, partition_id: GcPartitionId) -> NodeIterator<'_> {
        NodeIterator::from_context(self, partition_id)
    }
}
