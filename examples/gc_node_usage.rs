use gc_lite::{
    GcError, GcHeap, GcNode, GcPartitionId, GcRef, GcResult, GcScope, GcTrace, GcTraceCtx,
    gc_type_register,
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
    scope: GcPartitionId,
    value: i32,
) -> GcResult<GcRef<StaticNode>> {
    let ctx = GcScope::new(heap, 0, scope);
    let r = ctx
        .alloc_local(StaticNode { _value: value })
        .map_err(|(err, _)| err)?;
    ctx.flush();
    Ok(r)
}

fn alloc_other(heap: &mut GcHeap, scope: GcPartitionId, value: i32) -> GcResult<GcRef<OtherNode>> {
    let ctx = GcScope::new(heap, 0, scope);
    let r = ctx
        .alloc_local(OtherNode { _value: value })
        .map_err(|(err, _)| err)?;
    ctx.flush();
    Ok(r)
}

fn main() -> GcResult<()> {
    let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
    let scope = heap.create_partition();

    let static_ref = alloc_static(&mut heap, scope, 10)?;
    let other_ref = alloc_other(&mut heap, scope, 20)?;

    println!("Static node value: {}", static_ref._value);
    println!("Other node value: {}", other_ref._value);

    println!("gc_node_usage example verified");

    Ok(())
}
