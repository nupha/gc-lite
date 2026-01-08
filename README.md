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
- **PartitionId**: Partition identifier
- **PartitionInfo**: Partition information including memory usage
- **Gc<T>**: GC pointer wrapper providing safe object access
- **GcRef<T>**: Underlying GC reference for internal operations
- **GcWeak<T>**: Weak reference that doesn't prevent object collection
- **GcTracable** trait: Defines behavior that objects to be garbage collected must implement

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
        partition.name,
        partition.memory_used(),
        partition.memory_limit().unwrap_or(0));
}

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
use gc_lite::GcTracable;

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
    fn trace(&self, tracer: &mut gc_lite::GcTracer) {
        // Trace all child nodes
        for child in &self.children {
            tracer.mark(*child);
        }
    }
}
```

## Running Examples

```bash
# Run basic usage example
cargo run --example basic_usage

# Run tests
cargo test
```

## Notes

- All objects on the heap must implement the `GcTracable` trait
- Only root objects or objects referenced by root objects (directly or indirectly) will be retained
- Weak references don't prevent objects from being garbage collected
- Circular references can be broken through weak references
- Default partitions cannot be deleted
- Partitions must be empty to be deleted
