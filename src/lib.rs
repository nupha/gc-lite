// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

mod allocator;
mod heap;
mod mark_sweep;
mod node;
mod node_iterator;
mod partition;
mod trace;
mod type_registry;
mod weak;
mod xref;

pub use {
    heap::GcHeap,
    node::{Gc, GcHead, GcRef},
    partition::{GcPartition, GcPartitionId},
    trace::{GcTracable, GcTraceOp, GcTraceOps, GcTracer},
    weak::GcWeak,
};

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
