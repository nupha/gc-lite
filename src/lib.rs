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

pub use {
    heap::GcHeap,
    node::{Gc, GcRef},
    partition::{GcPartition, GcPartitionId},
    trace::{GcTracable, GcTracer},
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
