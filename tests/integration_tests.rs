// SPDX-License-Identifier: MIT
// Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Integration tests for gc-lite garbage collection system
//!
//! Tests cover:
//! - Partition management
//! - Object allocation and memory tracking
//! - Garbage collection
//! - Root object management
//! - Weak references
//! - Error handling

use gc_lite::{
    GcError, GcHeap, GcNode, GcPartitionId, GcRef, GcTracable, GcTraceCtx, GcTypeInfo, GcTypedNode,
    gctype_drop, gctype_trace,
};

/// Test data structure for integration tests
#[derive(Debug, PartialEq, Clone)]
struct TestData {
    value: i32,
    name: String,
}

unsafe impl GcTracable for TestData {
    fn trace(&self, _: &mut GcTraceCtx) {}
}

impl GcNode for TestData {}

/// Test node structure with GC references
struct TestNode {
    value: i32,
    children: Vec<GcRef<TestNode>>,
}

unsafe impl GcTracable for TestNode {
    fn trace(&self, tr: &mut GcTraceCtx) {
        for child in &self.children {
            tr.add(*child);
        }
    }
}

impl GcNode for TestNode {}

impl core::fmt::Debug for TestNode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TestNode")
            .field("value", &self.value)
            .field("children_count", &self.children.len())
            .finish()
    }
}

impl TestNode {
    fn new(value: i32) -> Self {
        Self {
            value,
            children: Vec::new(),
        }
    }

    fn add_child(&mut self, child: GcRef<TestNode>) {
        self.children.push(child);
    }
}

const GC_TYPE_INFO_LUT: &[GcTypeInfo] = &[
    GcTypeInfo {
        size: core::mem::size_of::<TestData>() as u32,
        trace_fn: gctype_trace::<TestData>,
        drop_fn: {
            if core::mem::needs_drop::<TestData>() {
                Some(gctype_drop::<TestData>)
            } else {
                None
            }
        },
        drop_pass: 0,
    },
    GcTypeInfo {
        size: core::mem::size_of::<TestNode>() as u32,
        trace_fn: gctype_trace::<TestNode>,
        drop_fn: {
            if core::mem::needs_drop::<TestNode>() {
                Some(gctype_drop::<TestNode>)
            } else {
                None
            }
        },
        drop_pass: 0,
    },
];

impl GcTypedNode for TestData {
    const GC_TYPE_ID: u8 = 0;
}

impl GcTypedNode for TestNode {
    const GC_TYPE_ID: u8 = 1;
}

// ============ Partition Management Tests ============

#[test]
fn test_partition_creation_and_retrieval() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);

    // Create partitions
    let id1 = heap.create_root_partition(1024);
    let id2 = heap.create_root_partition(0); // 0 for unlimited

    assert_ne!(id1, id2);
    assert_eq!(heap.partition_ids().len(), 2);

    // Verify partition info
    let partition = heap.partition(id1).unwrap();
    assert_eq!(partition.memory_limit(), 1024);
    assert_eq!(partition.memory_used(), 0);
}

#[test]
fn test_partition_removal() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(1024);

    assert!(heap.partition(id).is_some());
    assert_eq!(heap.partition_ids().len(), 1);

    heap.remove_partition(
        id,
        GcHeap::DUMMY_MIGRATE_CALLBACK,
        GcHeap::DUMMY_DISPOSE_CALLBACK,
    );

    assert!(heap.partition(id).is_none());
    assert_eq!(heap.partition_ids().len(), 0);
}

#[test]
fn test_partition_gc_threshold() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(1024);

    // Default threshold should be 0 (disabled)
    assert_eq!(heap.gc_threshold(id), Some(0));

    // Set threshold
    heap.set_gc_threshold(id, 512);
    assert_eq!(heap.gc_threshold(id), Some(512));

    // Set threshold exceeding limit should auto-adjust to 0.8x of limit
    heap.set_gc_threshold(id, 2048);
    // 1024 * 8 / 10 = 819
    assert_eq!(heap.gc_threshold(id), Some(819));
}

// ============ Memory Limit Tests ============

#[test]
fn test_allocation_fails_when_limit_exceeded() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(256); // Small limit

    // Allocate objects until we hit the limit
    let mut allocated_count = 0;
    loop {
        match heap.alloc(
            id,
            TestData {
                value: allocated_count as i32,
                name: format!("obj_{}", allocated_count),
            },
        ) {
            Ok(_) => {
                allocated_count += 1;
            }
            Err((GcError::PartitionFull, _)) => {
                // Expected when partition is full
                break;
            }
            Err((err, _)) => {
                panic!("Unexpected error: {:?}", err);
            }
        }
    }

    // Verify some objects were allocated
    assert!(allocated_count > 0);

    // Try to allocate one more - should fail
    let result = heap.alloc(
        id,
        TestData {
            value: 999,
            name: "should_fail".to_string(),
        },
    );

    assert!(matches!(result, Err((GcError::PartitionFull, _))));
}

#[test]
fn test_set_memory_limit_above_used_memory() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(1024);

    // Allocate some objects to use memory
    let _obj1 = heap
        .alloc(
            id,
            TestData {
                value: 1,
                name: "obj1".to_string(),
            },
        )
        .unwrap();

    let _obj2 = heap
        .alloc(
            id,
            TestData {
                value: 2,
                name: "obj2".to_string(),
            },
        )
        .unwrap();

    let used_memory = heap.partition(id).unwrap().memory_used();
    assert!(used_memory > 0);

    // Set limit larger than used memory - should work
    let new_limit = used_memory + 512;
    heap.partition_mut(id).unwrap().set_memory_limit(new_limit);

    // Verify limit was set correctly
    assert_eq!(heap.partition(id).unwrap().memory_limit(), new_limit);

    // Should be able to allocate more objects
    let _obj3 = heap
        .alloc(
            id,
            TestData {
                value: 3,
                name: "obj3".to_string(),
            },
        )
        .unwrap();
}

#[test]
fn test_set_memory_limit_below_used_memory() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(2048);

    // Allocate some objects to use memory
    let _obj1 = heap
        .alloc(
            id,
            TestData {
                value: 1,
                name: "obj1".to_string(),
            },
        )
        .unwrap();

    let _obj2 = heap
        .alloc(
            id,
            TestData {
                value: 2,
                name: "obj2".to_string(),
            },
        )
        .unwrap();

    let used_memory = heap.partition(id).unwrap().memory_used();
    assert!(used_memory > 0);

    // Set limit smaller than used memory - should be adjusted to used memory
    let smaller_limit = used_memory - 1;
    let applied_limit = heap
        .partition_mut(id)
        .unwrap()
        .set_memory_limit(smaller_limit);

    // The limit should be adjusted to at least the used memory
    assert_eq!(applied_limit, used_memory);
    assert_eq!(heap.partition(id).unwrap().memory_limit(), used_memory);

    // Should still be able to allocate (because limit >= used_memory)
    // But not more than the limit allows
    // Note: Since limit == used_memory, no new allocations should be allowed
    let result = heap.alloc(
        id,
        TestData {
            value: 3,
            name: "should_fail".to_string(),
        },
    );

    assert!(matches!(result, Err((GcError::PartitionFull, _))));
}

#[test]
fn test_set_unlimited_memory() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(512);

    // Set limit to 0 (unlimited)
    heap.partition_mut(id).unwrap().set_memory_limit(0);

    // Verify limit is 0 (unlimited)
    assert_eq!(heap.partition(id).unwrap().memory_limit(), 0);

    // Should be able to allocate many objects without hitting limit
    let mut allocated_count = 0;
    loop {
        match heap.alloc(
            id,
            TestData {
                value: allocated_count as i32,
                name: format!("obj_{}", allocated_count),
            },
        ) {
            Ok(_) => {
                allocated_count += 1;
                // Stop after a reasonable number to avoid infinite loop
                if allocated_count >= 100 {
                    break;
                }
            }
            Err((err, _)) => {
                panic!("Unexpected error with unlimited memory: {:?}", err);
            }
        }
    }

    assert_eq!(allocated_count, 100);
}

// ============ Object Allocation Tests ============

#[test]
fn test_object_allocation() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(2048);

    // Get initial memory usage
    let initial_memory = heap.partition(id).unwrap().memory_used();

    // Allocate an object
    let obj: GcRef<TestData> = heap
        .alloc(
            id,
            TestData {
                value: 42,
                name: "test".to_string(),
            },
        )
        .unwrap();

    // Verify partition memory was updated
    let partition = heap.partition(id).unwrap();
    let after_memory = partition.memory_used();
    assert!(after_memory > initial_memory);

    // Verify object content
    assert_eq!(obj.value, 42);
    assert_eq!(obj.name, "test");
}

#[test]
fn test_memory_usage_increases_with_allocation() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(8192);

    // Track memory usage after each allocation
    let mut memory_after_each_alloc: Vec<usize> = Vec::new();

    // Allocate multiple objects and track memory
    for i in 0..5 {
        let _obj = heap
            .alloc(
                id,
                TestData {
                    value: i as i32,
                    name: format!("obj_{}", i),
                },
            )
            .expect("allocation failed");

        let memory = heap.partition(id).unwrap().memory_used();
        memory_after_each_alloc.push(memory);
    }

    // Verify memory increases with each allocation
    for i in 1..memory_after_each_alloc.len() {
        assert!(
            memory_after_each_alloc[i] > memory_after_each_alloc[i - 1],
            "Memory should increase after each allocation"
        );
    }

    // Verify cumulative memory is correct (each object adds GcHead + T size)
    let final_memory = heap.partition(id).unwrap().memory_used();
    assert!(final_memory > 0);

    // Verify memory was freed after GC
    let root_obj = heap
        .alloc(
            id,
            TestData {
                value: 100,
                name: "root".to_string(),
            },
        )
        .unwrap();
    heap.set_root(root_obj, true);
    let freed = heap.garbage_collect(id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    assert!(freed > 0);

    let after_gc_memory = heap.partition(id).unwrap().memory_used();
    assert!(after_gc_memory < final_memory);
}

#[test]
fn test_multiple_object_allocation() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(4096);

    // Allocate multiple objects
    let mut objs: Vec<GcRef<TestData>> = Vec::new();
    for i in 0..10 {
        let obj = heap
            .alloc(
                id,
                TestData {
                    value: i as i32,
                    name: format!("obj_{}", i),
                },
            )
            .expect("allocation failed");
        objs.push(obj);
    }

    // Verify all objects
    for (i, obj) in objs.iter().enumerate() {
        assert_eq!(obj.value, i as i32);
        assert_eq!(obj.name, format!("obj_{}", i));
    }

    // Verify memory tracking
    let partition = heap.partition(id).unwrap();
    assert!(partition.memory_used() > 0);
}

#[test]
fn test_partition_full_error() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(512); // Very small limit

    // Try to allocate objects until partition is full
    let mut result = heap.alloc(
        id,
        TestData {
            value: 0,
            name: "test".to_string(),
        },
    );

    // Keep trying until we get a PartitionFull error
    let mut count = 0;
    while let Ok(_obj) = result {
        count += 1;
        result = heap.alloc(
            id,
            TestData {
                value: count,
                name: format!("test_{}", count),
            },
        );
    }

    assert!(matches!(result, Err((GcError::PartitionFull, _))));
    assert!(count > 0);
}

#[test]
fn test_invalid_partition_allocation() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let invalid_id = GcPartitionId(9999);

    let result = heap.alloc(
        invalid_id,
        TestData {
            value: 42,
            name: "test".to_string(),
        },
    );

    assert!(matches!(result, Err((GcError::PartitionNotFound, _))));
}

// ============ Root Object Tests ============

#[test]
fn test_root_object_management() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(2048);

    let obj = heap
        .alloc(
            id,
            TestData {
                value: 42,
                name: "test".to_string(),
            },
        )
        .unwrap();

    // Initially not a root
    assert!(!obj.is_root());

    // Set as root
    heap.set_root(obj, true);
    assert!(obj.is_root());

    // Clear root status
    heap.set_root(obj, false);
    assert!(!obj.is_root());
}

#[test]
fn test_root_objects_preserve_during_gc() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(2048);

    let obj = heap
        .alloc(
            id,
            TestData {
                value: 42,
                name: "test".to_string(),
            },
        )
        .unwrap();

    heap.set_root(obj, true);

    // Trigger GC
    let freed = heap.garbage_collect(id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    assert_eq!(freed, 0);

    // Object should still be valid
    assert_eq!(obj.value, 42);
}

#[test]
fn test_non_root_objects_collected() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(2048);

    // Create two objects, one is root, one is not
    let root_obj = heap
        .alloc(
            id,
            TestData {
                value: 1,
                name: "root".to_string(),
            },
        )
        .unwrap();

    let _non_root_obj = heap
        .alloc(
            id,
            TestData {
                value: 2,
                name: "non_root".to_string(),
            },
        )
        .unwrap();

    heap.set_root(root_obj, true);
    // non_root_obj is not set as root

    // Trigger GC
    let freed = heap.garbage_collect(id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    assert!(freed > 0);

    // Root object should still be valid
    assert_eq!(root_obj.value, 1);
}

// ============ Garbage Collection Tests ============

#[test]
fn test_manual_garbage_collection() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(2048);

    // Create objects with some as roots
    for i in 0..5 {
        let obj = heap
            .alloc(
                id,
                TestData {
                    value: i as i32,
                    name: format!("obj_{}", i),
                },
            )
            .unwrap();
        if i < 2 {
            heap.set_root(obj, true);
        }
    }

    // Get initial memory usage
    let before = heap.partition(id).unwrap().memory_used();

    // Trigger GC
    let freed = heap.garbage_collect(id, GcHeap::DUMMY_DISPOSE_CALLBACK);

    // Should have freed some memory
    assert!(freed > 0);

    // Verify root objects preserved
    let after = heap.partition(id).unwrap().memory_used();
    // Memory used should be less than before
    assert!(after < before);
}

#[test]
fn test_circular_reference_handling() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(2048);

    // Create two nodes that reference each other
    let mut node1 = heap.alloc(id, TestNode::new(1)).unwrap();
    let mut node2 = heap.alloc(id, TestNode::new(2)).unwrap();

    // Create circular reference
    node1.add_child(node2);
    node2.add_child(node1);

    // Set both as roots - they should be preserved
    heap.set_root(node1, true);
    heap.set_root(node2, true);

    // Verify node values are correct
    let node1_val = node1.value;
    let node2_val = node2.value;
    assert_eq!(node1_val, 1);
    assert_eq!(node2_val, 2);

    let freed = heap.garbage_collect(id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    assert_eq!(freed, 0); // Nothing freed because both are roots

    // Clear roots - circular reference should be collected
    heap.set_root(node1, false);
    heap.set_root(node2, false);

    let freed = heap.garbage_collect(id, GcHeap::DUMMY_DISPOSE_CALLBACK);
    assert!(freed > 0); // Circular reference should be freed
}

// ============ Weak Reference Tests ============

#[test]
fn test_weak_reference_creation_and_upgrade() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(2048);

    // Create object and weak reference
    let obj = heap
        .alloc(
            id,
            TestData {
                value: 42,
                name: "test".to_string(),
            },
        )
        .unwrap();

    heap.set_root(obj, true);

    let weak_ref = heap.downgrade(&obj);

    // Upgrade should succeed while object exists
    let upgraded = weak_ref.upgrade(&heap);
    assert!(upgraded.is_some());

    let upgraded_ref = upgraded.unwrap();
    assert_eq!(upgraded_ref.value, 42);
}

#[test]
fn test_weak_reference_after_collection() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(2048);

    // Create object and weak reference
    let obj = heap
        .alloc(
            id,
            TestData {
                value: 42,
                name: "test".to_string(),
            },
        )
        .unwrap();

    let weak_ref = heap.downgrade(&obj);

    // Clear root and collect
    heap.set_root(obj, false);
    heap.garbage_collect(id, GcHeap::DUMMY_DISPOSE_CALLBACK);

    // Upgrade should fail after object is collected
    let upgraded = weak_ref.upgrade(&heap);
    assert!(upgraded.is_none());
}

#[test]
fn test_multiple_weak_references() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let id = heap.create_root_partition(2048);

    let obj = heap
        .alloc(
            id,
            TestData {
                value: 42,
                name: "test".to_string(),
            },
        )
        .unwrap();

    heap.set_root(obj, true);

    // Create multiple weak references
    let weak1 = heap.downgrade(&obj);
    let weak2 = heap.downgrade(&obj);
    let weak3 = heap.downgrade(&obj);

    // All should upgrade successfully
    assert!(weak1.upgrade(&heap).is_some());
    assert!(weak2.upgrade(&heap).is_some());
    assert!(weak3.upgrade(&heap).is_some());
}

#[test]
fn test_weak_reference_after_partition_removal() {
    let mut heap = GcHeap::new_with_types(GC_TYPE_INFO_LUT);

    // 创建有层级的partitions
    let root_id = heap.create_root_partition(2048);
    let child_id = heap.create_sub_partition(root_id);

    // 在下属partition创建对象
    let obj = heap
        .alloc(
            child_id,
            TestData {
                value: 42,
                name: "test".to_string(),
            },
        )
        .unwrap();

    // 记录这些对象的GcWeak
    let weak_ref = heap.downgrade(&obj);

    // 验证弱引用可以升级
    assert!(weak_ref.upgrade(&heap).is_some());

    // 删除partition
    heap.remove_partition(
        child_id,
        GcHeap::DUMMY_MIGRATE_CALLBACK,
        GcHeap::DUMMY_DISPOSE_CALLBACK,
    );

    // 这时此partition中的对象也会释放
    // 访问这些对象的GcWeak引用，并upgrade()，应该返回None
    let upgraded = weak_ref.upgrade(&heap);
    assert!(upgraded.is_none());
}

// ============ Context Detection Tests ============

#[test]
fn test_contains_method() {
    let mut heap1 = GcHeap::new_with_types(GC_TYPE_INFO_LUT);
    let mut heap2 = GcHeap::new_with_types(GC_TYPE_INFO_LUT);

    let id1 = heap1.create_root_partition(1024);
    let id2 = heap2.create_root_partition(1024);

    let obj1 = heap1
        .alloc(
            id1,
            TestData {
                value: 1,
                name: "heap1".to_string(),
            },
        )
        .unwrap();

    let obj2 = heap2
        .alloc(
            id2,
            TestData {
                value: 2,
                name: "heap2".to_string(),
            },
        )
        .unwrap();

    // Verify object ownership
    assert!(heap1.contains(obj1.node_ptr()));
    assert!(!heap1.contains(obj2.node_ptr()));
    assert!(heap2.contains(obj2.node_ptr()));
    assert!(!heap2.contains(obj1.node_ptr()));
}

// ============ Reference Recovery Tests ============

// Note: try_from_ref requires exact type match including function pointers
// These tests are simplified to avoid type registration complexity
