# GC Lite

A Partitioned Garbage Collector.

## Features

1. **Flat Partition**: Supports multiple independent partitions without parent-child hierarchical relationships
2. **Root Objects**: Objects within each partition can be independently set as root objects
3. **Mark-Sweep**: Implements classic garbage collection algorithm
4. **Weak Reference**: Provides weak reference mechanism to avoid memory leaks caused by circular references
5. **Type Safety**: Ensures memory safety through Rust's type system

## Core Components

- **GcHeap**: Garbage collection heap, manages the lifecycle of all partitions and objects
- **GcPartitionId**: Partition identifier
- **GcPartition**: Partition information including memory usage
- **Gc<T>**: GC pointer wrapper providing safe object access
- **GcRef<T>**: Underlying GC reference for internal operations
- **GcWeak<T>**: Weak reference that doesn't prevent object collection
- **GcTracable** trait: Defines behavior that objects to be garbage collected must implement
- **GcTraceCtx**: Unified trace context for traversal and marking reachable objects

## Basic Usage

```rust
use gc_lite::{GcHeap, GcResult, GcTracable};

fn main() -> GcResult<()> {
    // Create garbage collection heap
    let mut heap = GcHeap::new();

    // Create new partition
    let partition_id = heap.create_partition("my_partition".to_string(), Some(1024));

    // Create object in specified partition
    let obj = heap.alloc(partition_id, String::from("Hello")).map_err(|(err, _)| err)?;

    // Create root object
    heap.set_root(obj, true);
    let root_obj = heap.alloc(partition_id, 42).map_err(|(err, _)| err)?;

    // Manually trigger garbage collection
    let freed = heap.collect_garbage(partition_id);
    println!("Freed {} bytes", freed);

    Ok(())
}
```

## Partition Management

```rust
use gc_lite::{GcHeap, GcResult};

let mut heap = GcHeap::new();

// Create partitions
let partition1 = heap.create_partition("partition1".to_string(), Some(1024));
let partition2 = heap.create_partition("partition2".to_string(), Some(512));

// Get partition information
if let Some(partition) = heap.partition(partition1) {
    println!("Partition: {}, Memory usage: {}/{}",
        partition.name(),
        partition.memory_used(),
        partition.memory_limit());
}

// Set memory limit (0 means unlimited)
heap.partition_mut(partition1).unwrap().set_memory_limit(2048);

// Get/set GC threshold
heap.set_gc_threshold(partition1, 1024);
let threshold = heap.gc_threshold(partition1).unwrap();

// Delete partition (must be empty)
heap.remove_partition(partition2);

// Automatic garbage collection (all partitions needing GC)
let total_freed = heap.collect_garbage_auto();
```

## Root Object Management

```rust
use gc_lite::{GcHeap, GcResult};

// Create regular object
let obj = heap.alloc(partition_id, String::from("test")).map_err(|(err, _)| err)?;

// Set as root object
heap.set_root(obj, true);

// Clear root object status
heap.set_root(obj, false);

// Directly create root object
let root_obj = heap.alloc(partition_id, 42).map_err(|(err, _)| err)?;
heap.set_root(root_obj, true);
```

## Weak References

```rust
use gc_lite::{GcHeap, GcResult};

let obj = heap.alloc(partition_id, String::from("weak test")).map_err(|(err, _)| err)?;
heap.set_root(obj, true);

// Create weak reference
let weak_ref = heap.downgrade(&obj);

// Try to upgrade weak reference
match weak_ref.upgrade(&heap) {
    Some(strong_ref) => {
        let value = unsafe { strong_ref.as_ref() };
        println!("Weak reference upgrade successful: {}", value);
    }
    None => {
        println!("Weak reference upgrade failed (object has been collected)");
    }
}
```

## Custom Types

To use custom types, implement the `GcTracable` trait:

```rust
use gc_lite::{GcTracable, GcTraceCtx, GcPartitionId};

#[derive(Debug)]
struct MyNode {
    name: String,
    children: Vec<GcRef<MyNode>>,
}

impl MyNode {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            children: Vec::new(),
        }
    }
}

unsafe impl GcTracable for MyNode {
    fn trace(&self, ctx: &mut GcTraceCtx) {
        for child in &self.children {
            ctx.add(*child);
        }
    }
}

// Traverse and collect the whole subtree starting from a node:
// let mut heap = GcHeap::new();
// let partition_id = heap.create_partition("p".to_string(), Some(1024));
// let root = heap.alloc(partition_id, MyNode::new("root")).unwrap();
// let (nodes, edges) = heap.collect_subtree(root.node_ptr(), GcPartitionId::NONE);
```

## Running Examples

```bash
# Run basic usage example
cargo run --example basic_usage

# Run tests
cargo test
```

## Manual Memory Release

```rust
use gc_lite::{GcHeap, GcResult};

// Safely release an object (checks for references)
let obj = heap.alloc(partition_id, String::from("test")).map_err(|(err, _)| err)?;
// ... use obj ...
let result = heap.free(obj);
match result {
    Ok(_) => println!("Object released successfully"),
    Err(GcError::InvalidReference) => println!("Object is still referenced"),
    _ => println!("Release failed"),
}
```

## Context Detection

```rust
use gc_lite::GcHeap;

let mut heap1 = GcHeap::new();
let mut heap2 = GcHeap::new();

let id1 = heap1.create_partition("p1".to_string(), Some(1024));
let id2 = heap2.create_partition("p2".to_string(), Some(1024));

let obj1 = heap1.alloc(id1, 42).unwrap();
let obj2 = heap2.alloc(id2, 100).unwrap();

// Check if object belongs to a heap
assert!(heap1.contains(&obj1));
assert!(!heap1.contains(&obj2));
```

## Gc Wrapper

```rust
use gc_lite::{Gc, GcHeap, GcTracable};

#[derive(Debug)]
struct Data {
    value: i32,
}

unsafe impl GcTracable for Data {
    fn trace(&self, _ctx: &mut gc_lite::GcTraceCtx) {}
}

let mut heap = GcHeap::new();
let id = heap.create_partition("test".to_string(), Some(1024));

let gc = Gc::new_in_partition(&mut heap, id, Data { value: 42 }).unwrap();

// Deref access
assert_eq!(gc.value, 42);

// Mutable access
gc.as_mut().value = 100;
assert_eq!(gc.value, 100);

// Set as root
gc.set_root(&mut heap, true);
```

## Error Handling

```rust
use gc_lite::{GcError, GcHeap};

let mut heap = GcHeap::new();
let id = heap.create_partition("test".to_string(), Some(64)); // Small limit

// Try to allocate large object
let result = heap.alloc(id, [0u8; 1024]);
match result {
    Ok(_) => println!("Allocated"),
    Err((GcError::PartitionFull, _)) => println!("Partition is full"),
    Err((GcError::AllocationFailed, _)) => println!("Memory allocation failed"),
    Err((GcError::PartitionNotFound, _)) => println!("Partition not found"),
    Err((GcError::InvalidReference, _)) => println!("Invalid reference"),
}
```

## Notes

- All objects on the heap must implement the `GcTracable` trait
- Only root objects or objects referenced by root objects (directly or indirectly) will be retained
- Weak references don't prevent objects from being garbage collected
- Circular references can be broken through weak references
- Setting memory limit to 0 means unlimited
- If memory limit is set below current usage, it will be adjusted to current usage
- Manual release (`heap.free()`) checks if object is referenced before releasing
