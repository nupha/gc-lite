// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

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
    heap::GcHeap,
    node::{GcHead, GcNode, GcRef, GcTypedNode},
    partition::{GcPartition, GcPartitionId},
    trace::{GcTracable, GcTraceCtx, GcTraceRestrict},
    weak::GcWeak,
};

pub use gctype::{GcTypeInfo, drop_fn as gctype_drop, trace_fn as gctype_trace};

#[macro_export]
macro_rules! gc_type_table {
    ( $( $id:expr => $ty:ty, drop_pass = $pass:expr; )+ ) => {
        pub const GC_TYPE_INFO_LUT: &[$crate::GcTypeInfo] = &[
            $(
                $crate::GcTypeInfo {
                    size: core::mem::size_of::<$ty>() as u32,
                    trace_fn: $crate::gctype_trace::<$ty>,
                    drop_fn: {
                        if core::mem::needs_drop::<$ty>() {
                            Some($crate::gctype_drop::<$ty>)
                        } else {
                            None
                        }
                    },
                    drop_pass: $pass,
                },
            )+
        ];

        $(
        impl $crate::GcTypedNode for $ty {
            const GC_TYPE_ID: u8 = $id;
        }

        impl $ty {
            pub fn alloc_node(
                heap: &mut $crate::GcHeap,
                scope: $crate::GcPartitionId,
                payload: $ty,
            ) -> Result<$crate::GcRef<$ty>, ($crate::GcError, $ty)> {
                heap.alloc_typed(scope, payload)
            }
        }
        )+
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
