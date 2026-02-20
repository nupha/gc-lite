
#[macro_export]
macro_rules! gc_type_table {
    ( $( $ty:ty $(, drop_pass = $pass:expr)?; )+ ) => {
        ::gc_lite_macros::gc_type_table_internal! {
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
