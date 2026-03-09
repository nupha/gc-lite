# GC Lite

A Partitioned Garbage Collector.

## Features

1. **Flat partitions**: Multiple independent partitions without parent–child hierarchy
2. **Tri-color marking**: Classic tri-color mark–sweep algorithm with gray lists per partition
3. **Root and scope based lifetime**: Root nodes and stack scopes define node reachability
4. **Weak references**: Non-owning references that do not prevent collection
5. **Global memory limit and GC threshold**: Heap-wide memory limit with automatic GC triggering
6. **Type-safe tracing**: All managed types implement `GcTrace` and are registered in a static type table

## Core Components

- **GcHeap**: Owns all partitions, tracks memory usage and runs garbage collection
- **GcPartitionId / GcPartition**: Partition identifier and per-partition statistics
- **GcRef<T>**: Typed GC reference that dereferences to `&T`
- **GcWeak<T>**: Weak reference that can be upgraded to `GcRef<T>`
- **GcTrace / GcTraceCtx**: Tracing trait and context used by the collector
- **GcScope**: Scoped allocation context for local nodes to be protected from being garbage collected
- **GcTypeRegistry / gc_type_register!**: Static type table and helper macro used to register GC types

Before using the heap, every GC-managed type must be registered through the `gc_type_register!` macro. This macro generates a static `GC_TYPE_REGISTRY` variable that holds all type metadata and automatically defines `GC_TYPE_ID` and helper constructors (such as `alloc_node`) for each registered type. The symbol name `GC_TYPE_REGISTRY` is created by the macro itself and is passed to `GcHeap::new(&GC_TYPE_REGISTRY)` in all examples below.

## Basic Usage

```rust
use gc_lite::{GcHeap, GcRef, GcResult, GcTrace, GcTraceCtx, gc_type_register};

#[derive(Debug)]
struct MyData {
    value: i32,
}

impl GcTrace for MyData {
    fn trace(&self, _: &mut GcTraceCtx) {}
}

// Register gc managed data types
gc_type_register! {
    MyData;
}

fn main() -> GcResult<()> {
    let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
    let partition = heap.create_partition();

    let value: GcRef<MyData> =
        unsafe { heap.alloc_root_raw(partition, MyData { value: 42 }) }
            .map_err(|(err, _)| err)?;

    println!("value = {}", value.value);

    let freed = heap.garbage_collect(partition, GcHeap::DUMMY_DISPOSE_CALLBACK);
    println!("freed {} bytes", freed);

    Ok(())
}
```

All subsequent examples assume that types have been registered with `gc_type_register!`
and that a `GcHeap` instance and at least one partition already exist.

## Partition Management

```rust
use gc_lite::GcHeap;

let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);

let partition1 = heap.create_partition();
let partition2 = heap.create_partition();

for id in heap.partition_ids() {
    if let Some(partition) = heap.partition(id) {
        println!("partition {:?}, used {} bytes", id, partition.memory_used());
    }
}

heap.set_memory_limit(2048);
heap.set_gc_threshold(1024);

heap.remove_partition(partition2, GcHeap::DUMMY_DISPOSE_CALLBACK);
```

## Root Objects and Scopes

```rust
use gc_lite::{GcHeap, GcRef, GcScope};

let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
let partition = heap.create_partition();

let scope = GcScope::new(&mut heap, partition);

let root: GcRef<MyData> = scope.alloc_root(MyData { value: 1 }).unwrap();
let local: GcRef<MyData> = scope.alloc_local(MyData { value: 2 }).unwrap();

scope.flush();

let freed = heap.garbage_collect(partition, GcHeap::DUMMY_DISPOSE_CALLBACK);
println!("freed {} bytes", freed);
```

## Weak References

```rust
use gc_lite::{GcHeap, GcRef, GcWeak};

let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
let partition = heap.create_partition();

let strong: GcRef<MyData> =
    unsafe { heap.alloc_root_raw(partition, MyData { value: 10 }) }
        .map_err(|(err, _)| err)?;

let weak: GcWeak<MyData> = heap.downgrade(&strong);

match weak.upgrade(&heap) {
    Some(value) => println!("upgraded: {}", value.value),
    None => println!("node collected"),
}
```

## Data Types

```rust
use gc_lite::{GcRef, GcTrace, GcTraceCtx, gc_type_register};

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

    fn add_child(&mut self, child: GcRef<MyNode>) {
        self.children.push(child);
    }
}

impl GcTrace for MyNode {
    fn trace(&self, ctx: &mut GcTraceCtx) {
        for child in &self.children {
            ctx.add(*child);
        }
    }
}

gc_type_register! {
    MyNode;
}
```

## Errors

```rust
use gc_lite::{GcError, GcHeap, GcResult, GcTrace, GcTraceCtx, gc_type_register};

#[derive(Debug)]
struct Large([u8; 1024]);

impl GcTrace for Large {
    fn trace(&self, _: &mut GcTraceCtx) {}
}

gc_type_register! {
    Large;
}

fn main() -> GcResult<()> {
    let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
    heap.set_memory_limit(64);
    let partition = heap.create_partition();

    match unsafe { heap.alloc_raw(partition, Large([0; 1024])) } {
        Ok(_) => println!("allocated"),
        Err((GcError::PartitionFull, _)) => println!("partition is full"),
        Err((GcError::AllocationFailed, _)) => println!("allocation failed"),
        Err((GcError::PartitionNotFound, _)) => println!("partition not found"),
        Err((GcError::InvalidReference, _)) => println!("invalid reference"),
    }

    Ok(())
}
```

## Running Examples

```bash
cargo run --example basic_usage
cargo run --example advanced_features
cargo run --example error_handling
cargo run --example performance_benchmark

cargo test
```

## Notes

- Every type stored in the heap must implement the `GcTrace` trait and be registered via `gc_type_register!`
- Only root or scope-protected nodes, or nodes reachable from them, are retained
- Weak references do not keep nodes alive
- Setting the heap memory limit to `0` disables the limit
- Automatic GC is controlled by a heap-wide GC threshold; `0` disables automatic GC
- The collector uses a tri-color mark–sweep algorithm internally (white/gray/black)
