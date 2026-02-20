// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Error handling example
//!
//! Demonstrates error handling mechanisms of the partitioned garbage collection system, including:
//! - Out of memory and partition full errors
//! - Safe release validation
//! - Partition management errors
//! - Invalid reference handling

use gc_lite::{GcError, GcHeap, GcNode, GcRef, GcResult, GcTracable, GcTraceCtx, gc_type_table};

fn main() -> GcResult<()> {
    println!("=== Error handling example of partitioned garbage collection system ===");

    // Demonstrate out of memory errors
    println!("\n=== Out of memory error handling ===");
    demonstrate_out_of_memory()?;

    // Demonstrate partition management errors
    println!("\n=== Partition management errors ===");
    demonstrate_partition_management_errors()?;

    // Demonstrate GC threshold API errors
    println!("\n=== GC threshold API errors ===");
    demonstrate_gc_threshold_errors()?;

    println!("\nAll error handling demonstrations completed!");
    Ok(())
}

/// Demonstrate out of memory error handling
fn demonstrate_out_of_memory() -> GcResult<()> {
    println!("1. Create limited memory partition...");

    let mut context = GcHeap::new_with_types(GC_TYPE_INFO_LIST);
    let partition_id = context.create_root_partition(2048); // 2KB limit

    // Allocate first large object (1KB + header)
    println!("2. Allocate first large object...");
    let gc1: GcRef<LargeData> = match context.alloc(partition_id, LargeData { data: [0; 1024] }) {
        Ok(gc_ref) => {
            println!("  ✓ Successfully allocated first object (1KB)");
            gc_ref
        }
        Err((error, _)) => {
            println!("  ✗ First object allocation failed: {:?}", error);
            return Ok(());
        }
    };

    // Allocate second large object (1KB + header) - should exceed 2KB limit
    println!("3. Try to allocate second large object...");
    match context.alloc(partition_id, LargeData { data: [0; 1024] }) {
        Err((GcError::PartitionFull, _)) => {
            println!("  ✓ Correctly detected partition full error");
        }
        Ok(_) => {
            println!("  ✗ Expected partition full error, but allocation succeeded");
            return Ok(());
        }
        Err((other_error, _)) => {
            println!(
                "  ✗ Expected partition full error, but got: {:?}",
                other_error
            );
            return Ok(());
        }
    }

    // Clean up - through garbage collection instead of manual release
    println!("  ✓ Automatic cleanup through GC");
    context.set_root(gc1, false);
    context.garbage_collect(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);

    Ok(())
}

/// Demonstrate partition management errors
fn demonstrate_partition_management_errors() -> GcResult<()> {
    println!("1. Test non-existent partition operations...");

    let mut context = GcHeap::new_with_types(GC_TYPE_INFO_LIST);
    let invalid_partition = gc_lite::GcPartitionId(9999); // Non-existent partition

    // Test allocating objects in non-existent partition
    match context.alloc(
        invalid_partition,
        TestData {
            value: 42,
            name: "test".to_string(),
        },
    ) {
        Err((GcError::PartitionNotFound, _)) => {
            println!("  ✓ Allocating objects in non-existent partition returns correct error");
        }
        Ok(_) => {
            println!("  ✗ Expected partition not found error, but allocation succeeded");
        }
        Err((other_error, _)) => {
            println!(
                "  ✗ Expected partition not found error, but got: {:?}",
                other_error
            );
        }
    }

    // Test getting non-existent partition information
    let partition_info = context.partition(invalid_partition);
    assert!(
        partition_info.is_none(),
        "Non-existent partition should return None"
    );
    println!("  ✓ Getting non-existent partition info returns None");

    // Test removing non-existent partition (remove_partition doesn't return error, just silently fails)
    context.remove_partition(
        invalid_partition,
        GcHeap::DUMMY_MIGRATE_CALLBACK,
        GcHeap::DUMMY_DISPOSE_CALLBACK,
    );
    println!("  ✓ Removing non-existent partition silently fails");

    println!("\n2. Test non-empty partition deletion...");
    let partition_id = context.create_root_partition(1024);

    // Allocate objects in partition
    let obj = context
        .alloc(
            partition_id,
            TestData {
                value: 1,
                name: "obj".to_string(),
            },
        )
        .unwrap();
    context.set_root(obj, true);

    // Try to delete non-empty partition (remove_partition will force cleanup)
    context.remove_partition(
        partition_id,
        GcHeap::DUMMY_MIGRATE_CALLBACK,
        GcHeap::DUMMY_DISPOSE_CALLBACK,
    );
    println!("  ✓ Successfully deleted non-empty partition (root objects were force cleaned)");

    Ok(())
}

/// Demonstrate GC threshold API errors
fn demonstrate_gc_threshold_errors() -> GcResult<()> {
    println!("1. Test GC threshold API...");

    let mut context = GcHeap::new_with_types(GC_TYPE_INFO_LIST);
    let partition_id = context.create_root_partition(1024);

    // Test default values
    println!("2. Test default threshold...");
    assert_eq!(context.gc_threshold(partition_id), Some(0));
    println!("  ✓ Default threshold is 0, automatic GC disabled");

    // Test setting threshold
    println!("3. Test setting threshold...");
    context.set_gc_threshold(partition_id, 512);
    assert_eq!(context.gc_threshold(partition_id), Some(512));
    println!("  ✓ Successfully set threshold to 512, automatic GC enabled");

    // Test setting threshold exceeding memory limit
    println!("4. Test setting threshold exceeding memory limit...");
    context.set_gc_threshold(partition_id, 2048);
    // Since threshold exceeds memory limit, will be capped at 0.8x of limit (1024 * 8 / 10 = 819)
    assert_eq!(context.gc_threshold(partition_id), Some(819));
    println!(
        "  ✓ Setting threshold exceeding memory limit automatically adjusted to 0.8x of memory limit"
    );

    // Test disabling automatic GC
    println!("5. Test disabling automatic GC...");
    context.set_gc_threshold(partition_id, 0);
    assert_eq!(context.gc_threshold(partition_id), Some(0));
    println!("  ✓ Successfully disabled automatic GC, threshold set to 0");

    // Test threshold operations on non-existent partition
    println!("6. Test threshold operations on non-existent partition...");
    let invalid_partition = gc_lite::GcPartitionId(9999);
    assert_eq!(context.gc_threshold(invalid_partition), None);
    context.set_gc_threshold(invalid_partition, 100); // Do nothing, no error returned
    println!("  ✓ Setting threshold on non-existent partition does nothing");

    Ok(())
}

// Supporting type definitions

/// Large memory data structure
#[derive(Debug)]
struct LargeData {
    data: [u8; 1024], // 1KB data
}

unsafe impl GcTracable for LargeData {
    fn trace(&self, _: &mut GcTraceCtx) {}
}

impl GcNode for LargeData {}

/// Test data structure
#[derive(Debug, PartialEq)]
struct TestData {
    value: i32,
    name: String,
}

unsafe impl GcTracable for TestData {
    fn trace(&self, _: &mut GcTraceCtx) {}
}

impl GcNode for TestData {}

/// Node structure for reference detection testing
#[derive(Debug)]
struct Node {
    value: i32,
    next: Option<GcRef<Node>>,
}

unsafe impl GcTracable for Node {
    fn trace(&self, tr: &mut GcTraceCtx) {
        if let Some(next) = self.next {
            tr.add(next);
        }
    }
}

impl GcNode for Node {}

gc_type_table! {
    LargeData;
    TestData;
}
