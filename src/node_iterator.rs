// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{GcPartition, GcPartitionId, heap::GcHeap, node::GcHead};

#[derive(Debug, Default)]
#[repr(transparent)]
pub(crate) struct GcNodeLink {
    link_head: Option<NonNull<GcHead>>,
}

impl GcNodeLink {
    pub fn new(head: Option<NonNull<GcHead>>) -> Self {
        Self { link_head: head }
    }

    pub fn into_inner(self) -> Option<NonNull<GcHead>> {
        self.link_head
    }

    pub fn head(&self) -> Option<NonNull<GcHead>> {
        self.link_head
    }

    /// Prepend node to the head of the link, link head is updated
    pub fn prepend(&mut self, node: NonNull<GcHead>) {
        unsafe {
            (*node.as_ptr()).next = self.link_head;
        }
        self.link_head = Some(node);
    }

    #[inline(always)]
    pub fn iter(&self) -> NodeLinkIter<'_> {
        NodeLinkIter::new(self.link_head)
    }

    pub fn len(&self) -> usize {
        self.iter().count()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.link_head.is_some()
    }

    pub fn filter_remove_with(
        &mut self,
        mut predicate: impl FnMut(&GcHead) -> bool,
        mut remove_fn: impl FnMut(NonNull<GcHead>),
    ) {
        let mut current = self.link_head;
        let mut prev: Option<NonNull<GcHead>> = None;

        while let Some(node) = current {
            unsafe {
                let node_ref = node.as_ref();
                if predicate(node_ref) {
                    // predicate returns true, so remove the node
                    let next = node_ref.next;
                    if let Some(mut prev_node) = prev {
                        // It's not the head of the list
                        prev_node.as_mut().next = next;
                    } else {
                        // It's the head of the list
                        self.link_head = next;
                    }
                    current = next;
                    remove_fn(node);
                } else {
                    // predicate returns false, keep the node and move to the next
                    prev = Some(node);
                    current = node_ref.next;
                }
            }
        }
    }
}

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
        if let Some(current) = self.current {
            self.current = unsafe { current.as_ref().next };
            Some(current)
        } else {
            None
        }
    }
}

impl GcHeap {
    #[inline]
    pub fn nodes(&self, partition_id: GcPartitionId) -> NodeLinkIter<'_> {
        NodeLinkIter::new(
            self.partitions
                .get(&partition_id)
                .and_then(|p| p.nodes.head()),
        )
    }
}

pub struct NodeLinkIterMut<'a> {
    current: Option<NonNull<GcHead>>,
    _marker: std::marker::PhantomData<&'a mut GcHead>,
}

impl<'a> NodeLinkIterMut<'a> {
    pub fn new(head: Option<NonNull<GcHead>>) -> Self {
        Self {
            current: head,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<'a> Iterator for NodeLinkIterMut<'a> {
    type Item = &'a mut GcHead;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(current) = self.current {
            unsafe {
                self.current = current.as_ref().next;
                Some(&mut *current.as_ptr())
            }
        } else {
            None
        }
    }
}

impl GcPartition {
    pub fn nodes_mut(&mut self) -> NodeLinkIterMut<'_> {
        NodeLinkIterMut::new(self.nodes.head())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gc_node_link_empty() {
        let link = GcNodeLink::new(None);
        let mut iter = link.iter();
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_gc_node_link_single() {
        let mut node = GcHead {
            attrs: 0,
            partition: 0,
            weak_id: crate::weak::GcWeakRawId::NULL,
            next: None,
            #[cfg(debug_assertions)]
            dbg_string: "".into(),
        };
        let node_ptr = NonNull::from(&mut node);
        let link = GcNodeLink::new(Some(node_ptr));
        let mut iter = link.iter();
        assert_eq!(iter.next(), Some(node_ptr));
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_gc_node_link_multiple() {
        let mut node3 = GcHead {
            attrs: 0,
            partition: 0,
            weak_id: crate::weak::GcWeakRawId::NULL,
            next: None,
            #[cfg(debug_assertions)]
            dbg_string: "node3".into(),
        };
        let node3_ptr = NonNull::from(&mut node3);

        let mut node2 = GcHead {
            attrs: 0,
            partition: 0,
            weak_id: crate::weak::GcWeakRawId::NULL,
            next: Some(node3_ptr),
            #[cfg(debug_assertions)]
            dbg_string: "node2".into(),
        };
        let node2_ptr = NonNull::from(&mut node2);

        let mut node1 = GcHead {
            attrs: 0,
            partition: 0,
            weak_id: crate::weak::GcWeakRawId::NULL,
            next: Some(node2_ptr),
            #[cfg(debug_assertions)]
            dbg_string: "node1".into(),
        };
        let node1_ptr = NonNull::from(&mut node1);

        let link = GcNodeLink::new(Some(node1_ptr));
        let mut iter = link.iter();

        assert_eq!(iter.next(), Some(node1_ptr));
        assert_eq!(iter.next(), Some(node2_ptr));
        assert_eq!(iter.next(), Some(node3_ptr));
        assert!(iter.next().is_none());
    }

    fn create_nodes(count: usize) -> (Vec<GcHead>, GcNodeLink) {
        let mut nodes: Vec<GcHead> = (0..count)
            .map(|i| GcHead {
                attrs: i as u32,
                partition: 0,
                weak_id: crate::weak::GcWeakRawId::NULL,
                next: None,
                #[cfg(debug_assertions)]
                dbg_string: format!("node{}", i).into(),
            })
            .collect();

        let mut link = GcNodeLink::new(None);
        // Iterate backwards over the vector to prepend correctly, building a list like (n-1)->(n-2)->...->0
        for node in nodes.iter_mut().rev() {
            let node_ptr = NonNull::from(node);
            link.prepend(node_ptr);
        }

        (nodes, link)
    }

    #[test]
    fn test_filter_remove_from_empty_list() {
        let mut link = GcNodeLink::new(None);
        let mut removed_count = 0;
        link.filter_remove_with(|_| true, |_| removed_count += 1);
        assert_eq!(removed_count, 0);
        assert!(link.iter().next().is_none());
    }

    #[test]
    fn test_filter_remove_none() {
        let (_nodes, mut link) = create_nodes(3);
        let mut removed_count = 0;
        link.filter_remove_with(|_| false, |_| removed_count += 1);
        assert_eq!(removed_count, 0);
        assert_eq!(link.len(), 3);
    }

    #[test]
    fn test_filter_remove_all() {
        let (_nodes, mut link) = create_nodes(3);
        let mut removed_count = 0;
        link.filter_remove_with(|_| true, |_| removed_count += 1);
        assert_eq!(removed_count, 3);
        assert!(link.iter().next().is_none());
    }

    #[test]
    fn test_filter_remove_first() {
        let (_nodes, mut link) = create_nodes(3);
        let mut removed_count = 0;
        link.filter_remove_with(|node| node.attrs == 0, |_| removed_count += 1);
        assert_eq!(removed_count, 1);
        assert_eq!(link.len(), 2);
        let mut iter = link.iter();
        assert_eq!(unsafe { iter.next().unwrap().as_ref().attrs }, 1);
        assert_eq!(unsafe { iter.next().unwrap().as_ref().attrs }, 2);
    }

    #[test]
    fn test_filter_remove_last() {
        let (_nodes, mut link) = create_nodes(3);
        let mut removed_count = 0;
        link.filter_remove_with(|node| node.attrs == 2, |_| removed_count += 1);
        assert_eq!(removed_count, 1);
        assert_eq!(link.len(), 2);
        let mut iter = link.iter();
        assert_eq!(unsafe { iter.next().unwrap().as_ref().attrs }, 0);
        assert_eq!(unsafe { iter.next().unwrap().as_ref().attrs }, 1);
    }

    #[test]
    fn test_filter_remove_middle() {
        let (_nodes, mut link) = create_nodes(3);
        let mut removed_count = 0;
        link.filter_remove_with(|node| node.attrs == 1, |_| removed_count += 1);
        assert_eq!(removed_count, 1);
        assert_eq!(link.len(), 2);
        let mut iter = link.iter();
        assert_eq!(unsafe { iter.next().unwrap().as_ref().attrs }, 0);
        assert_eq!(unsafe { iter.next().unwrap().as_ref().attrs }, 2);
    }

    #[test]
    fn test_filter_remove_every_other() {
        let (_nodes, mut link) = create_nodes(5);
        let mut removed_count = 0;
        link.filter_remove_with(|node| node.attrs % 2 != 0, |_| removed_count += 1);
        assert_eq!(removed_count, 2);
        assert_eq!(link.len(), 3);
        let mut iter = link.iter();
        assert_eq!(unsafe { iter.next().unwrap().as_ref().attrs }, 0);
        assert_eq!(unsafe { iter.next().unwrap().as_ref().attrs }, 2);
        assert_eq!(unsafe { iter.next().unwrap().as_ref().attrs }, 4);
    }
}
