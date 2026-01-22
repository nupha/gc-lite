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

use gc_lite::{GcHeap, GcRef, GcResult, GcTracable};

fn main() -> GcResult<()> {
    println!("=== Basic usage example of partitioned garbage collection system ===");

    // Create garbage collection context
    let mut heap = GcHeap::new();

    println!("Initial state:");
    println!("  Number of partitions: {}", heap.partition_ids().len());

    // Create two partitions
    println!("\nCreate partitions:");
    let partition1 = heap.create_partition("partition1".to_string(), Some(1024));
    let partition2 = heap.create_partition("partition2".to_string(), Some(512));
    println!("  Created partition1: {:?}", partition1);
    println!("  Created partition2: {:?}", partition2);
    println!("  Number of partitions: {}", heap.partition_ids().len());

    // Allocate objects in partition1
    println!("\nAllocate objects in partition1:");
    let obj1 = heap
        .alloc(partition1, String::from("Hello"))
        .map_err(|(err, _)| err)?;
    let obj2 = heap.alloc(partition1, 42).map_err(|(err, _)| err)?;
    let obj3 = heap
        .alloc(partition1, String::from("VectorData"))
        .map_err(|(err, _)| err)?;

    println!("  Created string: '{}'", unsafe { obj1.as_ref() });
    println!("  Created number: {}", unsafe { obj2.as_ref() });
    println!("  Created string: '{}'", unsafe { obj3.as_ref() });

    // Allocate objects in partition2
    println!("\nAllocate objects in partition2:");
    let obj4 = heap
        .alloc(partition2, String::from("World"))
        .map_err(|(err, _)| err)?;
    let obj5 = heap.alloc(partition2, 99).map_err(|(err, _)| err)?;

    println!("  Created string: '{}'", unsafe { obj4.as_ref() });
    println!("  Created number: {}", unsafe { obj5.as_ref() });

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
                "  {}: {} [自动GC: {}]",
                partition.name(),
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
    let freed = heap.collect_garbage(partition1);
    println!("  Collected {} bytes", freed);

    // Verify root objects are still valid
    println!("\nVerify partition1 root objects are still valid:");
    println!("  Object1: '{}'", unsafe { obj1.as_ref() });
    println!("  Object2: {}", unsafe { obj2.as_ref() });

    // Manually trigger garbage collection for partition2
    println!("\nManually trigger garbage collection for partition2...");
    let freed = heap.collect_garbage(partition2);
    println!("  Collected {} bytes", freed);

    // Verify partition2 root objects are still valid
    println!("\nVerify partition2 root objects are still valid:");
    println!("  Object4: '{}'", unsafe { obj4.as_ref() });

    // Clear some root objects
    println!("\nClear root object status:");
    heap.set_root(obj2, false);
    println!("  Cleared object2's root status");

    // Trigger garbage collection for partition1 again
    println!("\nTrigger garbage collection for partition1 again...");
    let freed = heap.collect_garbage(partition1);
    println!("  Collected {} bytes", freed);

    // Verify remaining root objects are still valid
    println!("\nVerify remaining root objects are still valid:");
    println!("  Object1: '{}'", unsafe { obj1.as_ref() });

    // Demonstrate automatic garbage collection
    println!("\nDemonstrate automatic garbage collection...");

    // Create a small partition to demonstrate automatic GC
    let small_partition = heap.create_partition("small".to_string(), Some(500));

    // Allocate multiple objects to fill partition
    for i in 0..5 {
        let _obj = heap
            .alloc(small_partition, format!("Object {}", i))
            .map_err(|(err, _)| err)?;
    }

    println!("  Allocated 5 objects in small partition");

    // Trigger automatic garbage collection
    let auto_freed = heap.collect_garbage_auto();
    println!(
        "  Automatic garbage collection freed {} bytes of memory",
        auto_freed
    );

    // Demonstrate weak references
    println!("\nDemonstrate weak references:");
    let weak_ref = heap.downgrade(&obj1);
    println!("  Created weak reference: {:?}", weak_ref);

    // Upgrade weak reference
    match weak_ref.upgrade(&heap) {
        Some(strong_ref) => {
            let value = unsafe { strong_ref.as_ref() };
            println!("  Weak reference upgrade successful: '{}'", value);
        }
        None => {
            println!("  Weak reference upgrade failed");
        }
    }

    // Demonstrate complex types with GC references
    println!("\nDemonstrate complex types with GC references:");
    let node1 = heap
        .alloc(partition1, TestNode::new("Node 1"))
        .map_err(|(err, _)| err)?;
    let node2 = heap
        .alloc(partition1, TestNode::new("Node 2"))
        .map_err(|(err, _)| err)?;

    // Set as root objects
    heap.set_root(node1, true);
    heap.set_root(node2, true);

    // Establish references between nodes
    unsafe {
        node1.as_mut().add_child(node2);
        node2.as_mut().add_child(node1);
    }

    println!("  Created node1: {}", unsafe { node1.as_ref() });
    println!("  Created node2: {}", unsafe { node2.as_ref() });

    // Trigger garbage collection, verify circular references are handled correctly
    println!("\nGarbage collection for handling circular references...");
    let freed = heap.collect_garbage(partition1);
    println!("  回收了 {} 字节内存", freed);

    // Demonstrate partition deletion
    println!("\nDemonstrate partition deletion:");

    // Create an empty partition
    let empty_partition = heap.create_partition("empty".to_string(), Some(1024));
    println!("  Created empty partition: {:?}", empty_partition);

    // Delete empty partition
    heap.remove_partition(empty_partition);
    println!("  Deleted empty partition successfully");

    // Delete non-empty partition
    heap.remove_partition(partition1);
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
    fn trace(&self, tracer: &mut gc_lite::GcTracer) {
        // Trace all child nodes
        for child in &self.children {
            tracer.mark(*child);
        }
    }
}

impl std::fmt::Display for TestNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TestNode({})", self.name)
    }
}
