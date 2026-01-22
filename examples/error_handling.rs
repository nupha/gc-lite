// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Error handling example
//!
//! Demonstrates error handling mechanisms of the partitioned garbage collection system, including:
//! - Out of memory and partition full errors
//! - Safe release validation
//! - Partition management errors
//! - Invalid reference handling

use gc_lite::{GcError, GcHeap, GcRef, GcResult, GcTracable};

fn main() -> GcResult<()> {
    println!("=== Error handling example of partitioned garbage collection system ===");

    // Demonstrate out of memory errors
    println!("\n=== Out of memory error handling ===");
    demonstrate_out_of_memory()?;

    // Demonstrate safe release validation
    println!("\n=== Safe release validation ===");
    demonstrate_safe_free_validation()?;

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

    let mut context = GcHeap::new();
    let partition_id = context.create_partition("limited".to_string(), Some(2048)); // 2KB limit

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
    context.collect_garbage(partition_id);

    Ok(())
}

/// Demonstrate safe release validation
fn demonstrate_safe_free_validation() -> GcResult<()> {
    println!("1. Create test object...");

    let mut context = GcHeap::new();
    let partition_id = context.create_partition("test".to_string(), Some(1024));

    let data = TestData {
        value: 42,
        name: "test".to_string(),
    };
    let gc_ref: GcRef<TestData> = context.alloc(partition_id, data).unwrap();

    println!("2. Test safe release...");
    let result = context.free(gc_ref);
    assert!(result.is_ok(), "Safe release object should succeed");
    println!("  ✓ Safe release object succeeded");

    println!("3. Test double release...");
    let result2 = context.free(gc_ref);
    assert!(result2.is_err(), "Double release should fail");
    println!("  ✓ Double release detection correct");

    println!("4. Test cross-context release...");
    let mut another_context = GcHeap::new();
    let another_partition_id = another_context.create_partition("another".to_string(), Some(1024));
    let another_data = TestData {
        value: 100,
        name: "another".to_string(),
    };
    let another_gc_ref: GcRef<TestData> = another_context
        .alloc(another_partition_id, another_data)
        .unwrap();

    let result3 = context.free(another_gc_ref);
    assert!(
        result3.is_err(),
        "Releasing objects from different contexts should fail"
    );
    println!("  ✓ Cross-context release detection correct");

    // Clean up objects in another context
    unsafe {
        another_context.free_unchecked(another_gc_ref).unwrap();
    }

    Ok(())
}

/// Demonstrate partition management errors
fn demonstrate_partition_management_errors() -> GcResult<()> {
    println!("1. Test non-existent partition operations...");

    let mut context = GcHeap::new();
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
    context.remove_partition(invalid_partition);
    println!("  ✓ Removing non-existent partition silently fails");

    println!("\n2. Test non-empty partition deletion...");
    let partition_id = context.create_partition("non_empty".to_string(), Some(1024));

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
    context.remove_partition(partition_id);
    println!("  ✓ Successfully deleted non-empty partition (root objects were force cleaned)");

    Ok(())
}

/// Demonstrate GC threshold API errors
fn demonstrate_gc_threshold_errors() -> GcResult<()> {
    println!("1. Test GC threshold API...");

    let mut context = GcHeap::new();
    let partition_id = context.create_partition("threshold_test".to_string(), Some(1024));

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

/// Demonstrate reference detection errors
fn demonstrate_reference_detection_errors() -> GcResult<()> {
    println!("1. Test reference detection...");

    let mut context = GcHeap::new();
    let partition_id = context.create_partition("ref_test".to_string(), Some(1024));

    // Create two mutually referencing nodes
    let node1 = Node {
        value: 1,
        next: None,
    };
    let node2 = Node {
        value: 2,
        next: None,
    };

    let gc_ref1: GcRef<Node> = context.alloc(partition_id, node1).unwrap();
    let gc_ref2: GcRef<Node> = context.alloc(partition_id, node2).unwrap();

    // Set mutual references
    unsafe {
        (*gc_ref1.as_mut_ptr()).next = Some(gc_ref2);
        (*gc_ref2.as_mut_ptr()).next = Some(gc_ref1);
    }

    println!("2. Test releasing referenced objects...");
    let result1 = context.free(gc_ref1);
    assert!(result1.is_err(), "Releasing referenced node1 should fail");
    println!("  ✓ Cannot release referenced objects");

    let result2 = context.free(gc_ref2);
    assert!(result2.is_err(), "Releasing referenced node2 should fail");
    println!("  ✓ Cannot release referenced objects");

    // First remove mutual references
    unsafe {
        (*gc_ref1.as_mut_ptr()).next = None;
        (*gc_ref2.as_mut_ptr()).next = None;
    }

    println!("3. Test release after removing references...");
    let result3 = context.free(gc_ref1);
    assert!(
        result3.is_ok(),
        "Should be able to release node1 after removing references"
    );
    println!("  ✓ Can safely release after removing references");

    let result4 = context.free(gc_ref2);
    assert!(
        result4.is_ok(),
        "Should be able to release node2 after removing references"
    );
    println!("  ✓ Can safely release after removing references");

    Ok(())
}

// Supporting type definitions

/// Large memory data structure
#[derive(Debug)]
struct LargeData {
    data: [u8; 1024], // 1KB data
}

unsafe impl GcTracable for LargeData {
    fn trace(&self, _tracer: &mut gc_lite::GcTracer) {
        // 没有需要追踪的引用
    }
}

/// Test data structure
#[derive(Debug, PartialEq)]
struct TestData {
    value: i32,
    name: String,
}

unsafe impl GcTracable for TestData {
    fn trace(&self, _tracer: &mut gc_lite::GcTracer) {
        // 没有需要追踪的引用
    }
}

/// Node structure for reference detection testing
#[derive(Debug)]
struct Node {
    value: i32,
    next: Option<GcRef<Node>>,
}

unsafe impl GcTracable for Node {
    fn trace(&self, tracer: &mut gc_lite::GcTracer) {
        if let Some(next) = self.next {
            tracer.mark(next);
        }
    }
}
