// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

pub mod gctype;
mod heap;
mod helpers;
mod mem;
mod node;
mod node_iterator;
mod partition;
mod sweep;
mod trace;
mod weak;
mod xref;

pub use {
    gctype::{GcTypeInfo, GcTypeRegistry, drop_fn as gctype_drop, trace_fn as gctype_trace},
    heap::GcHeap,
    helpers::{GcError, GcResult},
    node::{GcHead, GcNode, GcRef},
    partition::{GcPartition, GcPartitionId},
    trace::{GcTracable, GcTraceCtx, GcTraceRestrict},
    weak::GcWeak,
};

#[doc(hidden)]
pub use gc_lite_macros::gc_type_table_internal;

pub(crate) use helpers::unlikely;
