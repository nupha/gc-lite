use gc_lite::{
    GcHeap, GcPartitionId, GcRef, GcResult, GcScopeStackId, GcTrace, GcTraceCtx, gc_type_register,
};

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
            .map_err(|(err, _)| err);
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
            .map_err(|(err, _)| err);
        ctx.clear();
        r
    })
}

fn main() -> GcResult<()> {
    let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
    let partition = heap.create_partition();
    let stack_id = heap.acquire_scope_stack(partition);

    let static_ref = alloc_static(&mut heap, stack_id, 10)?;
    let other_ref = alloc_other(&mut heap, stack_id, 20)?;

    println!("Static node value: {}", static_ref._value);
    println!("Other node value: {}", other_ref._value);

    heap.release_scope_stack(stack_id);

    println!("gc_node_usage example verified");

    Ok(())
}
