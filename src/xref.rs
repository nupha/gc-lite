// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{GcHead, GcHeap, GcPartitionId};

impl GcHead {
    /// Get cross scope reference
    #[deprecated]
    pub fn xref(&self) -> GcPartitionId {
        let p = (self.partition >> 16) as u16;
        GcPartitionId(p)
    }

    /// Set cross scope reference.
    ///
    /// # Return
    ///
    /// * true if node has xref set
    /// * false if node has xref unset
    #[deprecated]
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

    /// Unset cross scope reference
    #[deprecated]
    #[inline(always)]
    pub fn unset_xref(&mut self) {
        self.set_xref(GcPartitionId::NONE);
    }
}

impl GcHeap {
    #[deprecated]
    pub const fn set_xref(&mut self, from_scope: GcPartitionId, mut node: NonNull<GcHead>) -> bool {
        false
    }

    /// Updates the node's cross-reference partition to a more general ancestor.
    /// Returns true if node's xref was updated, false if not.
    #[deprecated]
    pub(crate) fn set_xref_v0(
        &mut self,
        from_scope: GcPartitionId,
        mut node: NonNull<GcHead>,
    ) -> bool {
        debug_assert!(self.partition(from_scope).is_some());

        let (node_pid, xref0) = unsafe {
            let n = node.as_ref();
            (n.scope_id(), n.xref())
        };
        debug_assert!(self.partition(node_pid).is_some(), "{:?}", unsafe {
            node.as_ref()
        });

        if from_scope == node_pid || from_scope == xref0 {
            return false;
        }

        // Find common parent of (from_partition, node_partition) as up
        let mut up = self.common_parent2(from_scope, node_pid);
        debug_assert!(!up.is_null());

        if up == node_pid || up == xref0 {
            return false;
        }

        if !xref0.is_null() {
            // Find common parent of (from_partition, xref0)
            let up2 = self.common_parent2(from_scope, xref0);
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
