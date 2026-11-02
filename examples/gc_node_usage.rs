use gc_lite::{
    GcHeap, GcPartitionId, GcRef, GcResult, GcScopeStackId, GcTrace, GcTraceCtx, gc_type_register,
};

// `create_partition` takes no parameters when `gc_arena` is disabled.
#[cfg(feature = "gc_arena")]
fn new_partition(heap: &mut GcHeap) -> GcPartitionId {
    heap.create_partition(64 * 1024, 16 * 1024)
}

#[cfg(not(feature = "gc_arena"))]
fn new_partition(heap: &mut GcHeap) -> GcPartitionId {
    heap.create_partition()
}

#[derive(Debug)]
struct StaticNode {
    _value: i32,
}

impl GcTrace for StaticNode {
    fn trace(&self, _ctx: &mut GcTraceCtx) {}
}

#[derive(Debug)]
struct OtherNode {
    _value: i32,
}

impl GcTrace for OtherNode {
    fn trace(&self, _ctx: &mut GcTraceCtx) {}
}

gc_type_register! {
    StaticNode, drop_pass = 1;
    OtherNode;
}

fn alloc_static(
    heap: &mut GcHeap,
    stack_id: GcScopeStackId,
    value: i32,
) -> GcResult<GcRef<StaticNode>> {
    heap.with_new_scope(stack_id, |ctx| {
        let r = ctx
            .alloc_local(StaticNode { _value: value })
            .map_err(|(err, _)| err)
            .map(|gc| gc.into_raw());
        ctx.clear();
        r
    })
}

fn alloc_other(
    heap: &mut GcHeap,
    stack_id: GcScopeStackId,
    value: i32,
) -> GcResult<GcRef<OtherNode>> {
    heap.with_new_scope(stack_id, |ctx| {
        let r = ctx
            .alloc_local(OtherNode { _value: value })
            .map_err(|(err, _)| err)
            .map(|gc| gc.into_raw());
        ctx.clear();
        r
    })
}

fn main() -> GcResult<()> {
    let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
    let partition = new_partition(&mut heap);
    let stack_id = heap.acquire_scope_stack(partition);

    let static_ref = alloc_static(&mut heap, stack_id, 10)?;
    let other_ref = alloc_other(&mut heap, stack_id, 20)?;

    println!(
        "Static node value: {}",
        unsafe { static_ref.as_ref() }._value
    );
    println!("Other node value: {}", unsafe { other_ref.as_ref() }._value);

    heap.release_scope_stack(stack_id);

    println!("gc_node_usage example verified");

    Ok(())
}
