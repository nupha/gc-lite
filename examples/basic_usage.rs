// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Basic usage example
//!
//! Demonstrates basic usage of the partitioned garbage collection system, including:
//! - Creating partitions and allocating objects
//! - Setting root objects
//! - Manual and automatic garbage collection
//! - Weak reference usage
//! - Partition management

use std::ops::Deref;

use gc_lite::{GcHeap, GcRef, GcResult, GcTracable, GcTraceCtx, gc_type_register};

#[derive(Debug)]
struct MyString(String);

#[derive(Debug)]
struct MyI32(i32);

unsafe impl GcTracable for MyString {
    fn trace(&self, _: &mut GcTraceCtx) {}
}

unsafe impl GcTracable for MyI32 {
    fn trace(&self, _: &mut GcTraceCtx) {}
}

impl std::fmt::Display for MyString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for MyI32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

gc_type_register! {
    MyString, drop_pass = 0;
    MyI32;
    TestNode;
}

fn main() -> GcResult<()> {
    println!("=== Basic usage example of partitioned garbage collection system ===");

    // Create garbage collection context
    let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);

    println!("Initial state:");
    println!("  Number of partitions: {}", heap.partition_ids().len());

    // Create two partitions
    println!("\nCreate partitions:");
    let partition1 = heap.create_root_partition(1024);
    let partition2 = heap.create_root_partition(512);
    println!("  Created partition1: {:?}", partition1);
    println!("  Created partition2: {:?}", partition2);
    println!("  Number of partitions: {}", heap.partition_ids().len());

    // Allocate objects in partition1
    println!("\nAllocate objects in partition1:");
    let obj1 = heap
        .alloc(partition1, MyString(String::from("Hello")))
        .map_err(|(err, _)| err)?;
    let obj2 = heap.alloc(partition1, MyI32(42)).map_err(|(err, _)| err)?;
    let obj3 = heap
        .alloc(partition1, MyString(String::from("VectorData")))
        .map_err(|(err, _)| err)?;

    println!("  Created string: '{}'", obj1.deref());
    println!("  Created number: {}", obj2.deref());
    println!("  Created string: '{}'", obj3.deref());

    // Allocate objects in partition2
    println!("\nAllocate objects in partition2:");
    let obj4 = heap
        .alloc(partition2, MyString(String::from("World")))
        .map_err(|(err, _)| err)?;
    let obj5 = heap.alloc(partition2, MyI32(99)).map_err(|(err, _)| err)?;

    println!("  Created string: '{}'", obj4.deref());
    println!("  Created number: {}", obj5.deref());

    // Display partition status
    println!("\nPartition status:");
    for partition_id in heap.partition_ids() {
        if let Some(partition) = heap.partition(partition_id) {
            let limit = partition.memory_limit();
            let usage = if limit > 0 {
                format!(
                    "{}/{} bytes ({:.1}%)",
                    partition.memory_used(),
                    limit,
                    (partition.memory_used() as f64 / limit as f64) * 100.0
                )
            } else {
                format!("{}/∞ bytes", partition.memory_used())
            };
            println!(
                "  {:?}: {} [自动GC: {}]",
                partition_id,
                usage,
                if partition.gc_threshold() > 0 {
                    "Enabled"
                } else {
                    "Disabled"
                }
            );
        }
    }

    // Set some root objects
    println!("\nSet root objects:");
    heap.set_root(obj1, true);
    heap.set_root(obj2, true);
    heap.set_root(obj4, true);
    println!("  Set 3 root objects");

    // Manually trigger garbage collection for partition1
    println!("\nManually trigger garbage collection for partition1...");
    let freed = heap.garbage_collect(partition1, GcHeap::DUMMY_DISPOSE_CALLBACK);
    println!("  Collected {} bytes", freed);

    // Verify root objects are still valid
    println!("\nVerify partition1 root objects are still valid:");
    println!("  Object1: '{}'", obj1.deref());
    println!("  Object2: {}", obj2.deref());

    // Manually trigger garbage collection for partition2
    println!("\nManually trigger garbage collection for partition2...");
    let freed = heap.garbage_collect(partition2, GcHeap::DUMMY_DISPOSE_CALLBACK);
    println!("  Collected {} bytes", freed);

    // Verify partition2 root objects are still valid
    println!("\nVerify partition2 root objects are still valid:");
    println!("  Object4: '{}'", obj4.deref());

    // Clear some root objects
    println!("\nClear root object status:");
    heap.set_root(obj2, false);
    println!("  Cleared object2's root status");

    // Trigger garbage collection for partition1 again
    println!("\nTrigger garbage collection for partition1 again...");
    let freed = heap.garbage_collect(partition1, GcHeap::DUMMY_DISPOSE_CALLBACK);
    println!("  Collected {} bytes", freed);

    // Verify remaining root objects are still valid
    println!("\nVerify remaining root objects are still valid:");
    println!("  Object1: '{}'", obj1.deref());

    // Demonstrate automatic garbage collection
    println!("\nDemonstrate automatic garbage collection...");

    // Create a small partition to demonstrate automatic GC
    let small_partition = heap.create_root_partition(500);

    // Allocate multiple objects to fill partition
    for i in 0..5 {
        let _obj = heap
            .alloc(small_partition, MyString(format!("Object {}", i)))
            .map_err(|(err, _)| err)?;
    }

    println!("  Allocated 5 objects in small partition");

    // Demonstrate weak references
    println!("\nDemonstrate weak references:");
    let weak_ref = heap.downgrade(&obj1);
    println!("  Created weak reference: {:?}", weak_ref);

    // Upgrade weak reference
    match weak_ref.upgrade(&heap) {
        Some(strong_ref) => {
            println!(
                "  Weak reference upgrade successful: '{}'",
                strong_ref.deref()
            );
        }
        None => {
            println!("  Weak reference upgrade failed");
        }
    }

    // Demonstrate complex types with GC references
    println!("\nDemonstrate complex types with GC references:");
    let mut node1 = heap
        .alloc(partition1, TestNode::new("Node 1"))
        .map_err(|(err, _)| err)?;
    let mut node2 = heap
        .alloc(partition1, TestNode::new("Node 2"))
        .map_err(|(err, _)| err)?;

    // Set as root objects
    heap.set_root(node1, true);
    heap.set_root(node2, true);

    // Establish references between nodes
    {
        node1.add_child(node2);
        node2.add_child(node1);
    }

    println!("  Created node1: {}", node1.deref());
    println!("  Created node2: {}", node2.deref());

    // Trigger garbage collection, verify circular references are handled correctly
    println!("\nGarbage collection for handling circular references...");
    let freed = heap.garbage_collect(partition1, GcHeap::DUMMY_DISPOSE_CALLBACK);
    println!("  回收了 {} 字节内存", freed);

    // Demonstrate partition deletion
    println!("\nDemonstrate partition deletion:");

    // Create an empty partition
    let empty_partition = heap.create_root_partition(1024);
    println!("  Created empty partition: {:?}", empty_partition);

    // Delete empty partition
    heap.remove_partition(
        empty_partition,
        GcHeap::DUMMY_MIGRATE_CALLBACK,
        GcHeap::DUMMY_DISPOSE_CALLBACK,
    );
    println!("  Deleted empty partition successfully");

    // Delete non-empty partition
    heap.remove_partition(
        partition1,
        GcHeap::DUMMY_MIGRATE_CALLBACK,
        GcHeap::DUMMY_DISPOSE_CALLBACK,
    );
    println!("  Deleted non-empty partition successfully");

    println!("\nExample completed!");
    Ok(())
}

/// Test node structure with GC references
#[derive(Debug)]
struct TestNode {
    name: String,
    children: Vec<GcRef<TestNode>>,
}

impl TestNode {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            children: Vec::new(),
        }
    }

    fn add_child(&mut self, child: GcRef<TestNode>) {
        self.children.push(child);
    }
}

unsafe impl GcTracable for TestNode {
    fn trace(&self, tr: &mut GcTraceCtx) {
        // Trace all child nodes
        for child in &self.children {
            tr.add(*child);
        }
    }
}

impl std::fmt::Display for TestNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TestNode({})", self.name)
    }
}
