// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

extern crate self as gc_lite;

mod gctype;
mod heap;
mod mem;
mod node;
mod node_iterator;
mod partition;
mod sweep;
mod trace;
mod weak;
mod xref;

pub use {
    gctype::{GcTypeInfo, drop_fn as gctype_drop, trace_fn as gctype_trace},
    heap::GcHeap,
    node::{GcHead, GcNode, GcRef, GcTypedNode},
    partition::{GcPartition, GcPartitionId},
    trace::{GcTracable, GcTraceCtx, GcTraceRestrict},
    weak::GcWeak,
};

pub use gc_lite_macros::gc_type_table_internal;

#[macro_export]
macro_rules! gc_type_table {
    ( $( $ty:ty $(, drop_pass = $pass:expr)?; )+ ) => {
        $crate::gc_type_table_internal! {
            crate_path = $crate;
            $( $ty $(, drop_pass = $pass)?; )+
        }
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcError {
    /// Memory allocation failed
    AllocationFailed,
    /// Invalid reference
    InvalidReference,
    /// Partition is full
    PartitionFull,
    /// Partition not found
    PartitionNotFound,
    /// Partition is not empty
    PartitionNotEmpty,
}

pub type GcResult<T> = Result<T, GcError>;

#[inline(always)]
pub(crate) const fn unlikely(expr: bool) -> bool {
    if expr {
        #[cold]
        const fn cold_path() -> bool {
            true
        }
        cold_path()
    } else {
        false
    }
}
