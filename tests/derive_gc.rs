// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

//! Behavioral tests for `#[derive(GcTrace)]`.
//!
//! Each test builds a small object graph on a real [`GcHeap`], runs
//! mark + sweep, and asserts on survivors. This proves the generated trace
//! impls actually keep referenced nodes alive (and that `#[gc(skip)]`
//! fields genuinely do not).

use gc_lite::{gc_type_register, GcHeap, GcPartitionId, GcRef, GcTrace};
use smallvec::SmallVec;

// ── Types under test ────────────────────────────────────────────────────────

/// Plain node: exercises `Option<GcRef<T>>` field tracing.
#[derive(Debug, GcTrace)]
struct Inner {
    next: Option<GcRef<Inner>>,
    value: u32,
}

/// Mixed fields: `Vec<GcRef<T>>`, `[u8; N]`, `String`, `Option<String>`,
/// tuple, and an explicitly skipped raw pointer.
#[derive(Debug, GcTrace)]
struct Root {
    chain: Option<GcRef<Inner>>,
    list: Vec<GcRef<Inner>>,
    label: String,
    nums: [u8; 4],
    tag: Option<String>,
    pair: (u32, String),
    #[gc(skip)]
    raw: *mut u8,
}

/// Field holding a foreign container (`SmallVec`) that gc-lite cannot impl
/// `GcTrace` for (orphan rule): routed through `#[gc(with)]`.
#[derive(Debug, GcTrace)]
struct WithBag {
    #[gc(with = "trace_smallvec")]
    items: SmallVec<[GcRef<Inner>; 2]>,
}

/// Tuple struct: unnamed-field tracing plus a skipped non-GC handle.
#[derive(Debug, GcTrace)]
struct Pair(GcRef<Inner>, #[gc(skip)] usize);

/// Enum: each variant's payload fields are traced.
#[derive(Debug, GcTrace)]
enum Tree {
    Leaf(u32),
    Branch {
        left: GcRef<Tree>,
        right: Option<GcRef<Tree>>,
    },
}

/// GC-node payloads stored **by value** inside a container field.
#[derive(Debug, GcTrace)]
struct OwnedBag {
    items: Vec<Inner>,
    tag: String,
}

/// Generic struct: the derive must add a `GcTrace` bound to `T`.
#[derive(Debug, GcTrace)]
struct Wrap<T> {
    items: Vec<T>,
}

/// Field skipped in a tuple struct still referenced nowhere else: proves
/// skip semantics on unnamed fields.
#[derive(Debug, GcTrace)]
struct SkipRoot {
    #[gc(skip)]
    hidden: Option<GcRef<Inner>>,
    keep: u32,
}

gc_type_register! {
    Inner;
    Root;
    WithBag;
    Pair;
    Tree;
    OwnedBag;
    Wrap<Inner>;
    SkipRoot;
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn new_heap() -> (GcHeap, GcPartitionId) {
    let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
    let pid = heap.create_partition(64 * 1024, 16 * 1024);
    (heap, pid)
}

fn sweep(heap: &mut GcHeap, pid: GcPartitionId) -> usize {
    if let Some(white) = heap.sweep_unlink(pid) {
        let registry = heap.type_registry();
        GcHeap::sweep_drop_payloads(&white, registry);
        heap.sweep_dispose(pid, white)
    } else {
        panic!("sweep_unlink returned None — mark phase not finished?");
    }
}

/// Full collect: mark to completion, then sweep.
fn collect(heap: &mut GcHeap, pid: GcPartitionId) {
    while !heap.mark(pid, 16) {}
    sweep(heap, pid);
}

fn inner(heap: &mut GcHeap, pid: GcPartitionId, next: Option<GcRef<Inner>>, value: u32) -> GcRef<Inner> {
    Inner::alloc_node(heap, pid, Inner { next, value }).unwrap()
}

/// `#[gc(with)]` target for the `SmallVec` field of `WithBag`.
fn trace_smallvec(v: &SmallVec<[GcRef<Inner>; 2]>, gcx: &mut gc_lite::GcTraceCtx) {
    v.as_slice().trace(gcx);
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn derived_trace_keeps_graph_alive() {
    let (mut heap, pid) = new_heap();

    // c <- b <- a chain; root references all three via different field kinds
    // (Option via .chain, Vec via .list).
    let c = inner(&mut heap, pid, None, 3);
    let b = inner(&mut heap, pid, Some(c), 2);
    let a = inner(&mut heap, pid, Some(b), 1);
    let _orphan = inner(&mut heap, pid, None, 99);

    let _root = unsafe {
        heap.alloc_root_raw(
            pid,
            Root {
                chain: Some(a),
                list: vec![a, b, c],
                label: "root".into(),
                nums: [0; 4],
                tag: Some("t".into()),
                pair: (1, "p".into()),
                raw: std::ptr::null_mut(),
            },
        )
    }
    .unwrap();

    assert_eq!(heap.nodes(pid).count(), 5);
    collect(&mut heap, pid);

    // root + a + b + c survive; the orphan is collected.
    assert_eq!(heap.nodes(pid).count(), 4);
}

#[test]
fn skip_field_does_not_keep_alive() {
    let (mut heap, pid) = new_heap();

    let hidden = inner(&mut heap, pid, None, 7);
    let _root = unsafe {
        heap.alloc_root_raw(pid, SkipRoot { hidden: Some(hidden), keep: 1 })
    }
    .unwrap();

    assert_eq!(heap.nodes(pid).count(), 2);
    collect(&mut heap, pid);

    // `hidden` is only referenced from a #[gc(skip)] field: it must be swept.
    assert_eq!(
        heap.nodes(pid).count(),
        1,
        "#[gc(skip)] must not keep the hidden node alive"
    );
}

#[test]
fn with_attribute_traces_foreign_container() {
    let (mut heap, pid) = new_heap();

    let i1 = inner(&mut heap, pid, None, 1);
    let i2 = inner(&mut heap, pid, None, 2);
    let _orphan = inner(&mut heap, pid, None, 99);

    let _root = unsafe {
        heap.alloc_root_raw(
            pid,
            WithBag {
                items: SmallVec::from_vec(vec![i1, i2]),
            },
        )
    }
    .unwrap();

    assert_eq!(heap.nodes(pid).count(), 4);
    collect(&mut heap, pid);

    // Both SmallVec-held nodes survive through the #[gc(with)] handler.
    assert_eq!(heap.nodes(pid).count(), 3);
}

#[test]
fn tuple_struct_traces_unnamed_fields() {
    let (mut heap, pid) = new_heap();

    let c = inner(&mut heap, pid, None, 5);
    let _orphan = inner(&mut heap, pid, None, 99);

    let _root = unsafe { heap.alloc_root_raw(pid, Pair(c, 42)) }.unwrap();

    assert_eq!(heap.nodes(pid).count(), 3);
    collect(&mut heap, pid);

    // root Pair + c survive; the skipped usize has no effect and the orphan dies.
    assert_eq!(heap.nodes(pid).count(), 2);
}

#[test]
fn enum_variants_are_traced() {
    let (mut heap, pid) = new_heap();

    let l1 = Tree::alloc_node(&mut heap, pid, Tree::Leaf(1)).unwrap();
    let l2 = Tree::alloc_node(&mut heap, pid, Tree::Leaf(2)).unwrap();
    let _orphan = Tree::alloc_node(&mut heap, pid, Tree::Leaf(3)).unwrap();

    let _root = unsafe {
        heap.alloc_root_raw(pid, Tree::Branch { left: l1, right: Some(l2) })
    }
    .unwrap();

    assert_eq!(heap.nodes(pid).count(), 4);
    collect(&mut heap, pid);

    // Branch + both leaves survive; unreachable Leaf(3) is collected.
    assert_eq!(heap.nodes(pid).count(), 3);
}

#[test]
fn by_value_container_fields_are_traced() {
    let (mut heap, pid) = new_heap();

    // `deep` is only reachable through items[0].next — i.e. through tracing
    // a GC-node payload stored BY VALUE inside a Vec field.
    let deep = inner(&mut heap, pid, None, 11);
    let _orphan = inner(&mut heap, pid, None, 99);

    let _root = unsafe {
        heap.alloc_root_raw(
            pid,
            OwnedBag {
                items: vec![Inner { next: Some(deep), value: 10 }],
                tag: "bag".into(),
            },
        )
    }
    .unwrap();

    // Nodes: root + deep + orphan. `items[0]` is a by-value Inner embedded
    // in the root payload — not a separate node.
    assert_eq!(heap.nodes(pid).count(), 3);
    collect(&mut heap, pid);

    // root + deep survive: `deep` is reachable only because the Vec<Inner>
    // field traces its by-value elements, whose own trace reaches `.next`.
    assert_eq!(heap.nodes(pid).count(), 2);
}

#[test]
fn generic_struct_derives_gc_trace_bound() {
    let (mut heap, pid) = new_heap();

    // Registered as `Wrap<Inner>`: registering requires `Wrap<Inner>: GcTrace`,
    // which only holds because the derive added the `T: GcTrace` bound.
    let held = inner(&mut heap, pid, None, 10);
    let _orphan = inner(&mut heap, pid, None, 99);

    let _root = unsafe {
        heap.alloc_root_raw(
            pid,
            Wrap {
                items: vec![Inner { next: Some(held), value: 1 }],
            },
        )
    }
    .unwrap();

    assert_eq!(heap.nodes(pid).count(), 3);
    collect(&mut heap, pid);

    // root + `held` (reached through the generic Vec<Inner> field) survive.
    assert_eq!(heap.nodes(pid).count(), 2);
}

#[test]
fn repeated_gc_cycles_are_stable() {
    let (mut heap, pid) = new_heap();

    let c = inner(&mut heap, pid, None, 3);
    let b = inner(&mut heap, pid, Some(c), 2);
    let a = inner(&mut heap, pid, Some(b), 1);

    let _root = unsafe {
        heap.alloc_root_raw(
            pid,
            Root {
                chain: Some(a),
                list: vec![a],
                label: "r".into(),
                nums: [0; 4],
                tag: None,
                pair: (0, String::new()),
                raw: std::ptr::null_mut(),
            },
        )
    }
    .unwrap();

    // Repeated cycles must not corrupt the live graph or leak the dead ones.
    for round in 0..3 {
        let dead = inner(&mut heap, pid, None, 100 + round);
        drop(dead);
        collect(&mut heap, pid);
        assert_eq!(heap.nodes(pid).count(), 4, "round {round}");
    }
}
