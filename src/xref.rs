// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Cross-partition reference tracking (experimental / placeholder).
//!
//! This module contains stub APIs for tracking references that cross GC partition
//! boundaries. The implementation is not yet complete — none of these methods
//! have any real effect at runtime. They are retained as placeholders for future
//! development of a remembered-set or xref-table mechanism.
//!
//! All items in this module are marked `#[deprecated]` to discourage use.

use std::ptr::NonNull;

use crate::{GcHead, GcHeap, GcPartitionId};

impl GcHead {
    /// Get cross scope reference (placeholder, not yet functional).
    #[deprecated]
    pub fn xref(&self) -> GcPartitionId {
        let p = (self.partition >> 16) as u16;
        GcPartitionId(p)
    }

    /// Set cross scope reference (placeholder, not yet functional).
    ///
    /// # Return
    ///
    /// * true if node has xref set
    /// * false if node has xref unset
    #[deprecated]
    pub(crate) fn set_xref(&mut self, xref: GcPartitionId) -> bool {
        if !xref.is_null() && xref != self.partition_id() {
            log::trace!("[set_xref] {xref:?} -> {self:?}");
            self.partition = (self.partition & 0x0000_FFFF) | ((xref.0 as u32) << 16);
            true
        } else {
            self.partition &= 0x0000_FFFF;
            false
        }
    }

    /// Unset cross scope reference (placeholder, not yet functional).
    #[deprecated]
    #[inline(always)]
    pub fn unset_xref(&mut self) {
        self.set_xref(GcPartitionId::NONE);
    }
}

impl GcHeap {
    /// Cross-partition reference registration (placeholder, not yet functional).
    #[deprecated]
    pub const fn set_xref(&mut self, from_scope: GcPartitionId, node: NonNull<GcHead>) -> bool {
        false
    }
}
