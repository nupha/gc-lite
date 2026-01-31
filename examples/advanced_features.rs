// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Advanced features example
//!
//! Demonstrates advanced features of the partitioned garbage collection system, including:
//! - Weak references and circular reference handling
//! - Complex data structures
//! - Reference recovery and validation
//! - Cross-context object detection

use std::ops::Deref;

use gc_lite::{GcHeap, GcRef, GcResult, GcTracable};

fn main() -> GcResult<()> {
    println!("=== Advanced features example of partitioned garbage collection system ===");

    let mut heap = GcHeap::new();
    let partition = heap.create_root_partition(2048);

    // Demonstrate weak reference functionality
    println!("\n=== Weak reference functionality demonstration ===");
    demonstrate_weak_references(&mut heap, partition)?;

    // Demonstrate circular reference handling
    println!("\n=== Circular reference handling demonstration ===");
    demonstrate_cyclic_references(&mut heap, partition)?;

    // Demonstrate complex data structures
    println!("\n=== Complex data structures demonstration ===");
    demonstrate_complex_structures(&mut heap, partition)?;

    // Demonstrate reference recovery functionality
    println!("\n=== Reference recovery functionality demonstration ===");
    demonstrate_reference_recovery(&mut heap, partition)?;

    // Demonstrate cross-context detection
    println!("\n=== Cross-context detection demonstration ===");
    demonstrate_cross_context_detection()?;

    println!("\nAll advanced feature demonstrations completed!");
    Ok(())
}

/// Demonstrate weak reference functionality
fn demonstrate_weak_references(
    heap: &mut GcHeap,
    partition: gc_lite::GcPartitionId,
) -> GcResult<()> {
    println!("1. Create strong and weak references...");

    let strong_ref = heap
        .alloc(partition, String::from("Strong Reference Data"))
        .map_err(|(err, _)| err)?;

    let weak_ref = heap.downgrade(&strong_ref);
    println!("  Created strong reference: {:?}", strong_ref);
    println!("  Created weak reference: {:?}", weak_ref);

    // Upgrade weak reference
    println!("\n2. Upgrade weak reference...");
    match weak_ref.upgrade(heap) {
        Some(upgraded) => {
            let data = upgraded.deref();
            println!("  Weak reference upgrade successful: '{}'", data);
            assert_eq!(data, "Strong Reference Data");
        }
        None => println!("  Weak reference upgrade failed"),
    }

    // Try upgrading after releasing strong reference
    println!("\n3. Upgrade weak reference after releasing strong reference...");
    heap.set_root(strong_ref, false);
    heap.collect_garbage(partition);

    match weak_ref.upgrade(heap) {
        Some(_) => {
            println!("  Weak reference can still be upgraded (object may still be in memory)")
        }
        None => println!("  Weak reference upgrade failed (object has been collected)"),
    }

    Ok(())
}

/// Demonstrate circular reference handling
fn demonstrate_cyclic_references(
    heap: &mut GcHeap,
    partition: gc_lite::GcPartitionId,
) -> GcResult<()> {
    println!("1. Create circular reference nodes...");

    // Create two mutually referencing nodes
    let mut node1 = heap
        .alloc(partition, CyclicNode::new("Node A"))
        .map_err(|(err, _)| err)?;
    let mut node2 = heap
        .alloc(partition, CyclicNode::new("Node B"))
        .map_err(|(err, _)| err)?;

    // Establish circular references
    {
        node1.set_partner(node2);
        node2.set_partner(node1);
    }

    println!("  Created node1: {}", node1.deref());
    println!("  Created node2: {}", node2.deref());

    // Set as root objects
    heap.set_root(node1, true);
    heap.set_root(node2, true);

    // Trigger garbage collection
    println!("\n2. Trigger garbage collection (circular references still exist)...");
    let freed = heap.collect_garbage(partition);
    println!("  回收了 {} 字节内存", freed);

    // Verify circular references still exist
    println!("\n3. Verify circular references...");
    println!("  Node1's partner: {}", node1.get_partner_name());
    println!("  Node2's partner: {}", node2.get_partner_name());

    // Clear root object status, let circular references be collected
    println!("\n4. Clear root object status and trigger GC again...");
    heap.set_root(node1, false);
    heap.set_root(node2, false);

    let freed = heap.collect_garbage(partition);
    println!(
        "  Freed {} bytes of memory (circular references correctly collected)",
        freed
    );

    Ok(())
}

/// Demonstrate complex data structures
fn demonstrate_complex_structures(
    heap: &mut GcHeap,
    partition: gc_lite::GcPartitionId,
) -> GcResult<()> {
    println!("1. Create complex data structures...");

    // Create multiple nodes
    let mut root_node = heap
        .alloc(partition, TreeNode::new("Root"))
        .map_err(|(err, _)| err)?;
    let mut child1 = heap
        .alloc(partition, TreeNode::new("Child 1"))
        .map_err(|(err, _)| err)?;
    let mut child2 = heap
        .alloc(partition, TreeNode::new("Child 2"))
        .map_err(|(err, _)| err)?;
    let mut grandchild = heap
        .alloc(partition, TreeNode::new("Grandchild"))
        .map_err(|(err, _)| err)?;

    // Build tree structure
    {
        root_node.add_child(child1);
        root_node.add_child(child2);
        child1.add_child(grandchild);
    }

    // Create data container
    let container = heap
        .alloc(
            partition,
            DataContainer {
                root: root_node,
                metadata: vec![1, 2, 3],
                optional_data: Some(child1),
            },
        )
        .map_err(|(err, _)| err)?;

    heap.set_root(container, true);

    println!("  Created tree structure:");
    println!("    Root -> Child 1 -> Grandchild");
    println!("    Root -> Child 2");
    println!("  Created data container");

    // Trigger garbage collection
    println!("\n2. Trigger garbage collection...");
    let freed = heap.collect_garbage(partition);
    println!("  回收了 {} 字节内存", freed);

    // Verify data structure integrity
    println!("\n3. Verify data structure integrity...");
    {
        println!("  Container root node: {}", container.root.name);
        println!("  Metadata length: {}", container.metadata.len());
        println!(
            "  Optional data exists: {}",
            container.optional_data.is_some()
        );
    }

    Ok(())
}

/// Demonstrate reference recovery functionality
fn demonstrate_reference_recovery(
    heap: &mut GcHeap,
    partition: gc_lite::GcPartitionId,
) -> GcResult<()> {
    println!("1. Create object and get reference...");

    let original_ref = heap
        .alloc(
            partition,
            TestData {
                value: 42,
                name: "test".to_string(),
            },
        )
        .map_err(|(err, _)| err)?;

    let data_ref = original_ref.deref();
    println!("  Original reference: {:?}", original_ref);
    println!("  Data: {:?}", data_ref);

    // Recover GcRef from reference
    println!("\n2. Recover GcRef from reference...");
    let recovered_ref = GcRef::try_from_ref(heap, data_ref);

    match recovered_ref {
        Some(recovered) => {
            println!("  Recovery successful: {:?}", recovered);
            let recovered_data = recovered.deref();
            println!("  Recovered data: {:?}", recovered_data);
            println!("  Data equal: {}", data_ref == recovered_data);
            println!("  Reference equal: {}", original_ref == recovered);
        }
        None => println!("  Recovery failed (possibly type registration issue)"),
    }

    // Test invalid reference recovery - create an object not in GC heap
    println!("\n3. Test invalid reference recovery...");
    let local_data = TestData {
        value: 100,
        name: "local".to_string(),
    };
    let invalid_result = GcRef::try_from_ref(heap, &local_data);
    println!(
        "  Invalid reference recovery result: {:?} (should be None)",
        invalid_result
    );

    Ok(())
}

/// Demonstrate cross-context detection
fn demonstrate_cross_context_detection() -> GcResult<()> {
    println!("1. Create two independent contexts...");

    let mut context1 = GcHeap::new();
    let mut context2 = GcHeap::new();

    let partition1 = context1.create_root_partition(1024);
    let partition2 = context2.create_root_partition(1024);

    let obj1 = context1
        .alloc(
            partition1,
            TestData {
                value: 1,
                name: "obj1".to_string(),
            },
        )
        .unwrap();
    let obj2 = context2
        .alloc(
            partition2,
            TestData {
                value: 2,
                name: "obj2".to_string(),
            },
        )
        .unwrap();

    println!("2. Test object source detection...");
    assert!(
        context1.contains(obj1.head_ptr()),
        "obj1 should be from context1"
    );
    assert!(
        !context1.contains(obj2.head_ptr()),
        "obj2 should not be from context1"
    );
    assert!(
        context2.contains(obj2.head_ptr()),
        "obj2 should be from context2"
    );
    assert!(
        !context2.contains(obj1.head_ptr()),
        "obj1 should not be from context2"
    );

    println!("  ✓ Cross-context detection correct");

    // Clean up
    context1.set_root(obj1, false);
    context1.collect_garbage(partition1);
    context2.set_root(obj2, false);
    context2.collect_garbage(partition2);

    Ok(())
}

// Supporting type definitions

/// Circular reference node
#[derive(Debug)]
struct CyclicNode {
    name: String,
    partner: Option<GcRef<CyclicNode>>,
}

impl CyclicNode {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            partner: None,
        }
    }

    fn set_partner(&mut self, partner: GcRef<CyclicNode>) {
        self.partner = Some(partner);
    }

    fn get_partner_name(&self) -> String {
        self.partner
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "None".to_string())
    }
}

unsafe impl GcTracable for CyclicNode {
    fn trace(&self, tracer: &mut gc_lite::GcTracer) {
        if let Some(partner) = self.partner {
            tracer.add(partner);
        }
    }
}

impl std::fmt::Display for CyclicNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CyclicNode({})", self.name)
    }
}

/// Tree node
#[derive(Debug)]
struct TreeNode {
    name: String,
    children: Vec<GcRef<TreeNode>>,
}

impl TreeNode {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            children: Vec::new(),
        }
    }

    fn add_child(&mut self, child: GcRef<TreeNode>) {
        self.children.push(child);
    }
}

unsafe impl GcTracable for TreeNode {
    fn trace(&self, tracer: &mut gc_lite::GcTracer) {
        for child in &self.children {
            tracer.add(*child);
        }
    }
}

impl std::fmt::Display for TreeNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TreeNode({}, {} children)",
            self.name,
            self.children.len()
        )
    }
}

/// Data container
#[derive(Debug)]
struct DataContainer {
    root: GcRef<TreeNode>,
    metadata: Vec<i32>,
    optional_data: Option<GcRef<TreeNode>>,
}

unsafe impl GcTracable for DataContainer {
    fn trace(&self, tracer: &mut gc_lite::GcTracer) {
        tracer.add(self.root);
        if let Some(data) = self.optional_data {
            tracer.add(data);
        }
    }
}

/// Test data
#[derive(Debug, PartialEq)]
struct TestData {
    value: i32,
    name: String,
}

unsafe impl GcTracable for TestData {
    fn trace(&self, _tracer: &mut gc_lite::GcTracer) {
        // No references to trace
    }
}
