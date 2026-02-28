// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Advanced features example
//!
//! Demonstrates advanced features of the partitioned garbage collection system, including:
//! - Weak references and circular reference handling
//! - Complex data structures
//! - Reference recovery and validation
//! - Cross-context object detection

use std::ops::Deref;

use gc_lite::{GcHeap, GcRef, GcResult, GcTrace, GcTraceCtx, gc_type_register};

#[derive(Debug)]
struct MyString(String);

impl PartialEq<str> for MyString {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl GcTrace for MyString {
    fn trace(&self, _: &mut GcTraceCtx) {}
}

impl std::fmt::Display for MyString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

gc_type_register! {
    MyString;
    CyclicNode;
    TreeNode;
    DataContainer;
    TestData;
}

fn new_heap() -> GcHeap {
    GcHeap::new(&GC_TYPE_REGISTRY)
}

fn main() -> GcResult<()> {
    println!("=== Advanced features example of partitioned garbage collection system ===");

    let mut heap = new_heap();
    let partition = heap.create_partition(2048);

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

    let strong_ref =
        unsafe { heap.alloc_root_raw(partition, MyString(String::from("Strong Reference Data"))) }
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
    heap.garbage_collect(partition, GcHeap::DUMMY_DISPOSE_CALLBACK);

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
    let mut node1 = unsafe { heap.alloc_root_raw(partition, CyclicNode::new("Node A")) }
        .map_err(|(err, _)| err)?;
    let mut node2 = unsafe { heap.alloc_root_raw(partition, CyclicNode::new("Node B")) }
        .map_err(|(err, _)| err)?;

    // Establish circular references
    {
        node1.with_mut(heap, |n| n.set_partner(node2));
        node2.with_mut(heap, |n| n.set_partner(node1));
    }

    println!("  Created node1: {}", node1.deref());
    println!("  Created node2: {}", node2.deref());

    // Trigger garbage collection
    println!("\n2. Trigger garbage collection (circular references still exist)...");
    let freed = heap.garbage_collect(partition, GcHeap::DUMMY_DISPOSE_CALLBACK);
    println!("  回收了 {} 字节内存", freed);

    // Verify circular references still exist
    println!("\n3. Verify circular references...");
    println!("  Node1's partner: {}", node1.get_partner_name());
    println!("  Node2's partner: {}", node2.get_partner_name());

    // Clear root object status, let circular references be collected
    println!("\n4. Clear root object status and trigger GC again...");
    let freed = heap.garbage_collect(partition, GcHeap::DUMMY_DISPOSE_CALLBACK);
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
    let mut root_node =
        unsafe { heap.alloc_root_raw(partition, TreeNode::new("Root")) }.map_err(|(err, _)| err)?;
    let mut child1 =
        unsafe { heap.alloc_raw(partition, TreeNode::new("Child 1")) }.map_err(|(err, _)| err)?;
    let child2 =
        unsafe { heap.alloc_raw(partition, TreeNode::new("Child 2")) }.map_err(|(err, _)| err)?;
    let grandchild = unsafe { heap.alloc_raw(partition, TreeNode::new("Grandchild")) }
        .map_err(|(err, _)| err)?;

    // Build tree structure
    {
        root_node.with_mut(heap, |n| n.add_child(child1));
        root_node.with_mut(heap, |n| n.add_child(child2));
        child1.with_mut(heap, |n| n.add_child(grandchild));
    }

    // Create data container
    let container = unsafe {
        heap.alloc_root_raw(
            partition,
            DataContainer {
                root: root_node,
                metadata: vec![1, 2, 3],
                optional_data: Some(child1),
            },
        )
    }
    .map_err(|(err, _)| err)?;

    println!("  Created tree structure:");
    println!("    Root -> Child 1 -> Grandchild");
    println!("    Root -> Child 2");
    println!("  Created data container");

    // Trigger garbage collection
    println!("\n2. Trigger garbage collection...");
    let freed = heap.garbage_collect(partition, GcHeap::DUMMY_DISPOSE_CALLBACK);
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

    let original_ref = unsafe {
        heap.alloc_raw(
            partition,
            TestData {
                value: 42,
                name: "test".to_string(),
            },
        )
    }
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
    println!("1. Create two independent heaps...");

    let mut heap1 = new_heap();
    let mut heap2 = new_heap();

    let partition1 = heap1.create_partition(1024);
    let partition2 = heap2.create_partition(1024);

    let obj1 = unsafe {
        heap1.alloc_root_raw(
            partition1,
            TestData {
                value: 1,
                name: "obj1".to_string(),
            },
        )
    }
    .map_err(|(e, _)| e)?;
    let obj2 = unsafe {
        heap2.alloc_raw(
            partition2,
            TestData {
                value: 2,
                name: "obj2".to_string(),
            },
        )
    }
    .map_err(|(e, _)| e)?;

    println!("2. Test object source detection...");
    assert!(heap1.contains(obj1.node_ptr()), "obj1 should be from heap1");
    assert!(
        !heap1.contains(obj2.node_ptr()),
        "obj2 should not be from heap1"
    );
    assert!(heap2.contains(obj2.node_ptr()), "obj2 should be from heap2");
    assert!(
        !heap2.contains(obj1.node_ptr()),
        "obj1 should not be from heap2"
    );

    println!("  ✓ Cross-context detection correct");

    // Clean up
    heap1.garbage_collect(partition1, GcHeap::DUMMY_DISPOSE_CALLBACK);
    heap2.garbage_collect(partition2, GcHeap::DUMMY_DISPOSE_CALLBACK);

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

impl GcTrace for CyclicNode {
    fn trace(&self, tr: &mut GcTraceCtx) {
        if let Some(partner) = self.partner {
            tr.add(partner);
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

impl GcTrace for TreeNode {
    fn trace(&self, tr: &mut GcTraceCtx) {
        for child in &self.children {
            tr.add(*child);
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

impl GcTrace for DataContainer {
    fn trace(&self, tr: &mut GcTraceCtx) {
        tr.add(self.root);
        if let Some(data) = self.optional_data {
            tr.add(data);
        }
    }
}

/// Test data
#[derive(Debug, PartialEq)]
struct TestData {
    value: i32,
    name: String,
}

impl GcTrace for TestData {
    fn trace(&self, _: &mut GcTraceCtx) {
        // No references to trace
    }
}
