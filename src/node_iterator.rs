// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::GcPartitionId;
use crate::heap::GcHeap;
use crate::node::GcHead;

/// Iterate along node link chain
#[repr(transparent)]
pub struct NodeLinkIter<'a> {
    current: Option<NonNull<GcHead>>,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> NodeLinkIter<'a> {
    /// Create iterator from starting node
    pub fn new(starting: Option<NonNull<GcHead>>) -> Self {
        Self {
            current: starting,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<'a> Iterator for NodeLinkIter<'a> {
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
    #[inline]
    pub fn nodes(&self, partition_id: GcPartitionId) -> NodeLinkIter<'_> {
        NodeLinkIter::new(self.partition_nodes.get(&partition_id).copied().flatten())
    }
}
