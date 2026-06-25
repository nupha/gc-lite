// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Performance benchmark example
//!
//! Demonstrates performance characteristics of the partitioned garbage collection system, including:
//! - Allocation and collection performance for objects of different sizes
//! - GC performance for complex object graphs
//! - Memory usage efficiency analysis
//! - Automatic GC threshold performance

use gc_lite::{GcHeap, GcRef, GcTrace, GcTraceCtx, gc_type_register};
use std::time::{Duration, Instant};

fn main() {
    println!("=== Performance benchmark of partitioned garbage collection system ===");

    // Test allocation and collection of objects of different sizes
    println!("\n=== Object size performance test ===");
    benchmark_object_sizes();

    // Test GC performance of complex object graphs
    println!("\n=== Complex object graph performance test ===");
    benchmark_complex_graphs();

    // Test memory usage efficiency
    println!("\n=== Memory usage efficiency test ===");
    benchmark_memory_efficiency();

    // Test automatic GC threshold performance
    println!("\n=== Automatic GC threshold performance test ===");
    benchmark_auto_gc_threshold();

    println!("\nAll performance tests completed!");
}

/// Test allocation and collection performance for objects of different sizes
fn benchmark_object_sizes() {
    let sizes = [10, 100, 500, 1000, 5000];

    for &size in &sizes {
        println!("\nTest object count: {}", size);

        let mut context = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition = context.create_partition(64 * 1024, 16 * 1024);

        // Measure allocation performance
        let alloc_start = Instant::now();
        let mut objects = Vec::new();
        for _i in 0..size {
            let node = unsafe {
                context.alloc_raw(
                    partition,
                    SimpleNode {
                        _data: vec![0u8; 100],
                    },
                )
            } // Each node 100 bytes
            .unwrap();
            objects.push(node);
        }
        let alloc_duration = alloc_start.elapsed();

        println!("  Allocated {} objects in: {:?}", size, alloc_duration);
        println!(
            "  Average allocation time per object: {:?}",
            alloc_duration / size as u32
        );

        // Measure GC performance (all objects can be collected)
        let gc_start = Instant::now();
        let freed = context.garbage_collect(partition, GcHeap::DUMMY_DISPOSE_CALLBACK);
        let gc_duration = gc_start.elapsed();

        println!("  GC回收 {} 字节耗时: {:?}", freed, gc_duration);
        println!(
            "  Average collection time per byte: {:?}",
            if freed > 0 {
                gc_duration / freed as u32
            } else {
                Duration::from_nanos(0)
            }
        );
    }
}

/// Test GC performance of complex object graphs
fn benchmark_complex_graphs() {
    let sizes = [100, 500, 1000];

    for &size in &sizes {
        println!("\nTest complex object graph size: {} nodes", size);

        let mut context = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition = context.create_partition(64 * 1024, 16 * 1024);

        // Create complex object graph
        let graph_start = Instant::now();
        let mut nodes = Vec::new();

        // Create all nodes
        for _i in 0..size {
            let node = unsafe {
                context.alloc_raw(
                    partition,
                    GraphNode {
                        neighbors: Vec::new(),
                    },
                )
            }
            .unwrap();
            nodes.push(node);
        }

        // Establish complex dependencies
        for i in 0..size {
            {
                // Each node points to subsequent nodes
                for j in 1..=5 {
                    if i + j < size {
                        let n = nodes[i + j];
                        unsafe {
                            nodes[i]
                                .with_write_barrier(&mut context, |node| node.neighbors.push(n));
                        }
                    }
                }
                // Every 10 nodes form a cycle
                if i % 10 == 0 && i + 9 < size {
                    let n = nodes[i];
                    unsafe {
                        nodes[i + 9]
                            .with_write_barrier(&mut context, |node| node.neighbors.push(n));
                    }
                }
            }
        }
        let graph_duration = graph_start.elapsed();

        println!("  Built complex object graph in: {:?}", graph_duration);

        // Measure GC performance
        let gc_start = Instant::now();
        let freed = context.garbage_collect(partition, GcHeap::DUMMY_DISPOSE_CALLBACK);
        let gc_duration = gc_start.elapsed();

        println!("  GC回收 {} 字节耗时: {:?}", freed, gc_duration);
        println!(
            "  Object graph complexity: average {} neighbors per node",
            if size > 0 { (size * 5) / size } else { 0 }
        );
    }
}

/// Test memory usage efficiency
fn benchmark_memory_efficiency() {
    println!("\nTesting memory usage efficiency...");

    let mut context = GcHeap::new(&GC_TYPE_REGISTRY);
    context.set_memory_limit(1024 * 1024); // 1MB global limit
    let partition = context.create_partition(64 * 1024, 16 * 1024);

    // Allocate many small objects
    let small_objects_count = 1000;
    let mut small_objects = Vec::new();

    for _i in 0..small_objects_count {
        let obj = unsafe { context.alloc_raw(partition, SmallData {}) }.unwrap();
        small_objects.push(obj);
    }

    if let Some(partition_info) = context.partition(partition) {
        let used = partition_info.memory_used();
        let limit = context.memory_limit();
        let efficiency = if limit > 0 {
            (used as f64 / limit as f64) * 100.0
        } else {
            0.0
        };

        println!("  After allocating {} small objects:", small_objects_count);
        println!(
            "  Memory usage: {}/{} bytes ({:.1}%)",
            used, limit, efficiency
        );
        println!(
            "  Average overhead per object: {} bytes",
            if small_objects_count > 0 {
                used / small_objects_count
            } else {
                0
            }
        );
    }

    // Collect all objects
    let freed = context.garbage_collect(partition, GcHeap::DUMMY_DISPOSE_CALLBACK);
    println!("  Collected all objects, freed {} bytes", freed);

    // Verify complete memory collection
    if let Some(partition_info) = context.partition(partition) {
        let used_after = partition_info.memory_used();
        println!("  Memory usage after collection: {} bytes", used_after);
        println!(
            "  Memory collection rate: {:.1}%",
            if freed > 0 {
                (freed as f64 / (freed + used_after) as f64) * 100.0
            } else {
                0.0
            }
        );
    }
}

/// Test automatic GC threshold performance
fn benchmark_auto_gc_threshold() {
    println!("\nTesting automatic GC threshold performance...");

    let mut context = GcHeap::new(&GC_TYPE_REGISTRY);
    context.set_memory_limit(2048); // 2KB global limit
    let partition = context.create_partition(64 * 1024, 16 * 1024);

    // Set automatic GC threshold to 1.5KB
    context.set_gc_threshold(1500);

    // Allocate objects until automatic GC is triggered
    let mut allocated_bytes = 0;
    let mut object_count = 0;

    println!("  Allocating objects until automatic GC is triggered...");

    for _i in 0..100 {
        // Try at most 100 times
        // Allocate objects of about 100 bytes
        let node = SimpleNode {
            _data: vec![0u8; 100],
        };
        match unsafe { context.alloc_raw(partition, node) } {
            Ok(_gc_ref) => {
                allocated_bytes += 100 + std::mem::size_of::<GcRef<SimpleNode>>(); // Estimated size
                object_count += 1;

                // Check if approaching threshold
                if let Some(partition_info) = context.partition(partition)
                    && partition_info.memory_used() >= 1500
                {
                    println!(
                        "  Reached automatic GC threshold, allocated {} objects",
                        object_count
                    );
                    println!("  Estimated allocated memory: {} bytes", allocated_bytes);
                    println!(
                        "  Actual memory usage: {} bytes",
                        partition_info.memory_used()
                    );
                    break;
                }
            }
            Err(_) => {
                println!("  Allocation failed, automatic GC may have been triggered");
                break;
            }
        }
    }

    // Manually trigger GC to see effect
    let freed = context.garbage_collect(partition, GcHeap::DUMMY_DISPOSE_CALLBACK);
    println!("  Manual GC freed {} bytes", freed);
}

// Supporting type definitions

/// Simple node for performance testing
#[derive(Debug)]
struct SimpleNode {
    _data: Vec<u8>,
}

impl GcTrace for SimpleNode {
    fn trace(&self, _: &mut GcTraceCtx) {}
}

/// Graph node for complex object graph testing
#[derive(Debug)]
struct GraphNode {
    neighbors: Vec<GcRef<GraphNode>>,
}

impl GcTrace for GraphNode {
    fn trace(&self, tr: &mut GcTraceCtx) {
        for neighbor in &self.neighbors {
            tr.add(*neighbor);
        }
    }
}

/// Small data object for memory efficiency testing
#[derive(Debug)]
struct SmallData {
    // To avoid unused field warnings, no id and value fields included here
}

impl GcTrace for SmallData {
    fn trace(&self, _: &mut GcTraceCtx) {}
}

gc_type_register! {
    SimpleNode;
    GraphNode;
    SmallData;
}
