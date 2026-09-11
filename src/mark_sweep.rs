// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use std::ptr::NonNull;

use crate::{
    GcHeap,
    gctype::GcTypeRegistry,
    node::{GcHead, GcNode, GcRef, GcTriColor},
    node_link::GcNodeLink,
    partition::GcPartitionId,
    trace::GcTraceCtx,
};

#[cfg(feature = "gc_arena")]
use crate::node::GcNodeFlag;

impl GcHeap {
    pub fn add_gray_node(&mut self, node: NonNull<GcHead>) {
        if unsafe { node.as_ref().color() } != GcTriColor::Black {
            let pid = unsafe { node.as_ref().partition_id() };
            self.partitions[pid.0 as usize].add_gray_node(node);
        }
    }

    /// Check whether the partition that `node` belongs to is currently marking.
    #[inline]
    pub(crate) unsafe fn is_node_partition_marking<T: GcNode>(&self, node: GcRef<T>) -> bool {
        let pid = unsafe { node.head_ptr.as_ref().partition_id() };
        self.partitions[pid.0 as usize].is_marking()
    }

    pub fn mark_reset(&mut self, partition_id: GcPartitionId) {
        let par = &mut self.partitions[partition_id.0 as usize];
        par.set_marking(false);

        // Drain gray list and clear flags so stale GRAY_LISTED bits
        // don't affect the next marking cycle.
        for mut n in par.gray_list.drain(..) {
            unsafe {
                n.as_mut().set_gray_listed(false);
            }
        }

        for n in par.nodes_mut() {
            n.set_color(GcTriColor::White);
        }
    }

    pub fn mark_prepare(&mut self, partition_id: GcPartitionId) {
        if (partition_id.0 as usize) >= self.partitions.len() {
            return;
        }

        // If the target partition is already marking, no-op (its gray_list
        // is being processed by mark_grays).
        if self.partitions[partition_id.0 as usize].is_marking() {
            return;
        }

        // Set ALL partitions to marking mode so that cross-partition
        // references can be pushed into any partition's gray_list.
        for par in &mut self.partitions {
            par.set_marking(true);
        }

        // Seed roots from ALL partitions.
        for i in 0..self.partitions.len() {
            let has_pending_grays = !self.partitions[i].gray_list.is_empty();
            let par = &mut self.partitions[i];

            for mut n in par.nodes.iter() {
                let node = unsafe { n.as_mut() };
                if node.is_root_or_local() {
                    node.set_color(GcTriColor::Gray);
                    if !has_pending_grays {
                        par.gray_list.push(n);
                    }
                } else if !has_pending_grays {
                    node.set_color(GcTriColor::White);
                }
            }

            if has_pending_grays {
                // Pending gray nodes already exist; only add root/LOCAL
                // nodes that are not already in the gray list.
                for mut n in par.nodes.iter() {
                    let node = unsafe { n.as_mut() };
                    if node.is_root_or_local() {
                        node.set_color(GcTriColor::Gray);
                        if !node.is_gray_listed() {
                            node.set_gray_listed(true);
                            par.gray_list.push(n);
                        }
                    }
                }
            }
        }
    }

    pub fn mark_grays(&mut self, partition_id: GcPartitionId, max_steps: usize) -> bool {
        if max_steps == 0 {
            return false;
        }

        // Pre-acquire the opaque pointer and type registry so we don't need &self
        // while holding a mutable borrow on the partition.
        let opaque = self.opaque();
        let node_dtypes: *const crate::gctype::GcTypeRegistry = self.node_dtypes;

        if !self.partitions[partition_id.0 as usize]
            .gray_list
            .is_empty()
        {
            let mut gcx = GcTraceCtx {
                traced_nodes: Vec::with_capacity(64),
                opaque,
                _mark: std::marker::PhantomData,
            };

            // Buffer for children that need to be pushed to a different
            // partition's gray_list (avoids borrow-checker conflicts when
            // indexing self.partitions at different indices).
            let mut cross_buffer: Vec<(u16, NonNull<GcHead>)> = Vec::with_capacity(8);
            let mut cnt = 0;

            while let Some(mut node_ptr) = self.partitions[partition_id.0 as usize].gray_list.pop()
            {
                let node = unsafe { node_ptr.as_mut() };
                // Clear gray_listed flag when popping (O(1) instead of O(n) contains)
                node.set_gray_listed(false);

                if node.color() == GcTriColor::Gray {
                    if cnt >= max_steps {
                        self.partitions[partition_id.0 as usize]
                            .gray_list
                            .push(node_ptr);
                        return false;
                    }

                    // SAFETY: node_dtypes is &'static, and we hold &mut self so the
                    // registry is guaranteed to be alive.
                    debug_assert!(
                        gcx.traced_nodes.is_empty(),
                        "trace context should be empty before tracing a new node"
                    );
                    gcx.traced_nodes.clear();

                    unsafe {
                        let dtype = node_ptr.as_ref().dtype() as usize;
                        let info = &(*node_dtypes).type_info_list[dtype];
                        (info.trace_fn)(node_ptr, &mut gcx);
                    }

                    while let Some(mut ch) = gcx.traced_nodes.pop() {
                        let child = unsafe { ch.as_mut() };

                        #[cfg(debug_assertions)]
                        child.debug_assert_node_valid_simple();

                        if matches!(child.color(), GcTriColor::White | GcTriColor::Gray) {
                            child.set_color(GcTriColor::Gray);
                            child.set_gray_listed(true);

                            let child_pid = child.partition_id();
                            if child_pid == partition_id {
                                // Same partition: push directly
                                self.partitions[partition_id.0 as usize].gray_list.push(ch);
                            } else {
                                // Cross-partition child: buffer and push after
                                // the inner loop so we don't contend with the
                                // while-let borrow on self.partitions[pid].
                                cross_buffer.push((child_pid.0, ch));
                            }
                        }
                    }

                    // Flush cross-partition children to their own partitions
                    for (cpid, node) in cross_buffer.drain(..) {
                        self.partitions[cpid as usize].gray_list.push(node);
                    }

                    // Mark current node as black.
                    node.set_color(GcTriColor::Black);

                    cnt += 1;
                }
            }
        }

        true
    }

    pub fn mark(&mut self, partition_id: GcPartitionId, max_steps: usize) -> bool {
        self.mark_prepare(partition_id);
        if max_steps > 0 {
            self.mark_grays(partition_id, max_steps)
        } else {
            false
        }
    }

    /// dispose white nodes in the partition.
    /// `on_dispose` is called BEFORE a node will be disposed.
    ///
    /// ⚠️ **DEPRECATED** — This method is unsound: it calls `drop_fn` while
    /// `&mut GcHeap` is live, and GC-node `Drop` impls that access
    /// `RuntimePtr` through raw pointers create aliasing `&mut` references
    /// (UB exposed by LTO + `panic=abort` as SEGV).
    ///
    /// **Use the three-step API instead:**
    ///
    /// ```ignore
    /// if let Some(white) = gc_heap.sweep_unlink(pid) {
    ///     // Step 2: drop payloads — MUST run with no &mut GcHeap alive,
    ///     // so the LTO optimizer never sees aliased &mut references.
    ///     GcHeap::sweep_drop_payloads(&white, registry);
    ///     // Step 3: free memory & accounting (re-acquires &mut GcHeap)
    ///     gc_heap.sweep_dispose(pid, white);
    /// }
    /// ```
    #[deprecated(
        note = "unsound — use sweep_unlink() + sweep_drop_payloads() + sweep_dispose() instead"
    )]
    pub fn sweep(
        &mut self,
        partition_id: GcPartitionId,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        if (partition_id.0 as usize) >= self.partitions.len() {
            return 0;
        }
        if let Some(link0) = {
            let par = &mut self.partitions[partition_id.0 as usize];
            if par.is_marking() && par.gray_list.is_empty() {
                par.set_marking(false);
                std::mem::take(&mut par.nodes).into_inner()
            } else {
                None // mark cycle not done
            }
        } {
            #[cfg(debug_assertions)]
            for n in crate::node_link::NodeLinkIter::new(Some(link0)) {
                unsafe {
                    debug_assert!(
                        matches!(n.as_ref().color(), GcTriColor::Black | GcTriColor::White),
                        "sweep node must be either black or white: {:?}",
                        n.as_ref()
                    );
                }
            }

            let call_on_dispose = !std::ptr::addr_eq(&on_dispose, &Self::DUMMY_DISPOSE_CALLBACK);
            let mut link1 = Some(link0);
            let mut freed_bytes = 0;

            #[cfg(feature = "gc_arena")]
            let mut holes: Vec<crate::arena::Hole> = Vec::new();

            for &pass in self.node_dtypes.drop_passes {
                let mut current = link1;
                let mut prev: Option<NonNull<GcHead>> = None;

                while let Some(mut this) = current {
                    unsafe {
                        #[cfg(debug_assertions)]
                        this.as_ref().debug_assert_node_valid(self);

                        current = this.as_mut().next;

                        let drop_pass = self.node_dtypes.type_info_list
                            [this.as_ref().dtype() as usize]
                            .drop_pass;

                        if drop_pass == pass
                            && this.as_ref().color() == GcTriColor::White
                            && !this.as_ref().is_root_or_local()
                        {
                            if let Some(mut p) = prev {
                                p.as_mut().next = current;
                            } else {
                                link1 = current;
                            }

                            if call_on_dispose {
                                on_dispose(self, this.as_ref());
                            }

                            // Cache arena info before dispose poisons GcHead
                            #[cfg(feature = "gc_arena")]
                            let arena_info = {
                                let hd = this.as_ref();
                                let is_arena = hd.contains_flag(GcNodeFlag::ARENA_ALLOC);
                                let dtype = hd.dtype() as usize;
                                let info = &self.node_dtypes.type_info_list[dtype];
                                (is_arena, info.layout().size())
                            };

                            freed_bytes += self.dispose(this);

                            // Arena hole collection (after dispose, which skips mem_dealloc)
                            #[cfg(feature = "gc_arena")]
                            if arena_info.0 {
                                // SAFETY: an arena-allocated node exists only if the
                                // partition has an arena — this is an invariant.
                                self.partitions[partition_id.0 as usize]
                                    .arena
                                    .as_ref()
                                    .unwrap()
                                    .collect_hole(&mut holes, this.cast::<u8>(), arena_info.1);
                            }
                        } else {
                            prev = Some(this);
                        }
                    }
                }

                if link1.is_none() {
                    break;
                }
            }

            #[cfg(feature = "gc_arena")]
            if let Some(ref arena) = self.partitions[partition_id.0 as usize].arena {
                arena.finish_sweep(&mut holes);
            }

            debug_assert!(
                self.partitions[partition_id.0 as usize]
                    .gray_list
                    .is_empty()
            );

            // update remainder node link of partition
            if link1.is_some() {
                #[cfg(debug_assertions)]
                for n in crate::node_link::NodeLinkIter::new(link1) {
                    unsafe {
                        debug_assert!(
                            n.as_ref().color() == GcTriColor::Black
                                || n.as_ref().is_root_or_local(),
                            "live nodes should be black, root or protected"
                        );
                    }
                }

                self.partitions[partition_id.0 as usize].nodes =
                    crate::node_link::GcNodeLink::new(link1);
            }

            // Memory accounting is already handled by dispose() which calls
            // update_mem_use() for each individual node. No need to subtract
            // freed_bytes again here.
            freed_bytes
        } else {
            0
        }
    }

    /// Phase 1 of two-phase sweep: unlink white (dead) nodes from the
    /// partition's node list WITHOUT calling `drop_fn` or disposing them.
    ///
    /// Returns the linked list of unlinked white nodes, or `None` if the
    /// mark cycle is not complete or the partition doesn't exist.
    ///
    /// Phase 1 of two-phase sweep: extract white, non-root, non-local nodes
    /// from the partition's node chain.
    ///
    /// After a completed mark phase the chain contains only Black (root/
    /// reachable) and White (unreachable) nodes. This method walks the chain,
    /// **unlinks** white non-root nodes into a returned `GcNodeLink`, and
    /// leaves the black/root nodes properly linked in the partition.
    ///
    /// The caller must then drop each returned node's payload (via the type
    /// registry's `drop_fn`) **without** holding `&mut GcHeap`, and finally
    /// call [`sweep_dispose`] to free memory and update accounting.
    ///
    /// Returns `None` if the partition is not in a post-mark state.
    pub fn sweep_unlink(&mut self, partition_id: GcPartitionId) -> Option<GcNodeLink> {
        if (partition_id.0 as usize) >= self.partitions.len() {
            return None;
        }

        let par = &mut self.partitions[partition_id.0 as usize];
        if !(par.is_marking() && par.gray_list.is_empty()) {
            return None;
        }
        par.set_marking(false);

        let link0 = std::mem::take(&mut par.nodes).into_inner()?;

        // Walk the chain, split white non-root nodes out, keep the rest.
        let mut current = Some(link0);
        let mut white_head: Option<NonNull<GcHead>> = None;
        let mut white_tail: Option<NonNull<GcHead>> = None;
        let mut survivor_head: Option<NonNull<GcHead>> = None;
        let mut survivor_tail: Option<NonNull<GcHead>> = None;

        while let Some(mut this) = current {
            unsafe {
                let next = this.as_mut().next;

                let is_white_non_root =
                    this.as_ref().color() == GcTriColor::White && !this.as_ref().is_root_or_local();

                // Detach from old chain.
                this.as_mut().next = None;

                if is_white_non_root {
                    match white_tail {
                        Some(mut tail) => tail.as_mut().next = Some(this),
                        None => white_head = Some(this),
                    }
                    white_tail = Some(this);
                } else {
                    match survivor_tail {
                        Some(mut tail) => tail.as_mut().next = Some(this),
                        None => survivor_head = Some(this),
                    }
                    survivor_tail = Some(this);
                }

                current = next;
            }
        }

        // Put survivors back — partition node chain is intact.
        par.nodes = GcNodeLink::new(survivor_head);

        debug_assert!(par.gray_list.is_empty());

        #[cfg(debug_assertions)]
        for n in crate::node_link::NodeLinkIter::new(white_head) {
            unsafe {
                let hd = n.as_ref();
                debug_assert_eq!(
                    hd.color(),
                    GcTriColor::White,
                    "sweep_unlink: white list contains non-white node: {:?}",
                    hd
                );
                debug_assert!(
                    !hd.is_root_or_local(),
                    "sweep_unlink: white list contains root/local node: {:?}",
                    hd
                );
            }
        }

        Some(GcNodeLink::new(white_head))
    }

    /// Drop payloads of every node in the white list.
    ///
    /// This is the safe part of two-phase sweep: it runs **without**
    /// `&mut GcHeap`, so `Drop` impls that access `RuntimePtr` are safe.
    ///
    /// Every node in the list is guaranteed to be white, non-root, and
    /// non-local (filtered by [`sweep_unlink`]).
    pub fn sweep_drop_payloads(white: &GcNodeLink, registry: &'static GcTypeRegistry) {
        for node in white.iter() {
            let dtype = unsafe { node.as_ref().dtype() } as usize;
            let info = &registry.type_info_list[dtype];
            if let Some(f) = info.drop_fn {
                unsafe {
                    f(info.payload_ptr(node).as_ptr());
                }
            }
        }
    }

    /// Phase 3 of two-phase sweep: dispose a list of white non-root nodes.
    ///
    /// Does **NOT** call `drop_fn` — the caller must have already dropped
    /// each node's payload via [`sweep_drop_payloads`].
    ///
    /// Handles: weak slot clearing, debug poisoning, memory accounting,
    /// arena hole collection, and deallocation. Processes nodes by
    /// `drop_pass` to respect destruction ordering.
    ///
    /// Returns the total bytes freed.
    pub fn sweep_dispose(&mut self, partition_id: GcPartitionId, white: GcNodeLink) -> usize {
        let registry = self.node_dtypes;
        let mut freed_bytes = 0;

        #[cfg(feature = "gc_arena")]
        let mut holes: Vec<crate::arena::Hole> = Vec::new();

        // Every node in `white` is already white & non-root — no color/root
        // filter needed. Just iterate by drop_pass for destruction ordering.
        let mut remaining = white;
        for &pass in registry.drop_passes {
            let mut current = remaining.into_inner();
            let mut kept_head: Option<NonNull<GcHead>> = None;
            let mut kept_tail: Option<NonNull<GcHead>> = None;

            while let Some(mut this) = current {
                unsafe {
                    current = this.as_mut().next;

                    let drop_pass =
                        registry.type_info_list[this.as_ref().dtype() as usize].drop_pass;

                    if drop_pass == pass {
                        // Correct pass — dispose this node.
                        #[cfg(debug_assertions)]
                        {
                            let hd = this.as_ref();
                            debug_assert_eq!(
                                hd.color(),
                                GcTriColor::White,
                                "sweep_dispose: non-white node in white list: {:?}",
                                hd
                            );
                            debug_assert!(
                                !hd.is_root_or_local(),
                                "sweep_dispose: root/local node in white list: {:?}",
                                hd
                            );
                        }

                        #[cfg(feature = "gc_arena")]
                        let arena_info = {
                            let hd = this.as_ref();
                            let is_arena = hd.contains_flag(GcNodeFlag::ARENA_ALLOC);
                            let dtype = hd.dtype() as usize;
                            let info = &registry.type_info_list[dtype];
                            (is_arena, info.layout().size())
                        };

                        freed_bytes += self.dispose_no_drop(this);

                        #[cfg(feature = "gc_arena")]
                        if arena_info.0 {
                            self.partitions[partition_id.0 as usize]
                                .arena
                                .as_ref()
                                .unwrap()
                                .collect_hole(&mut holes, this.cast::<u8>(), arena_info.1);
                        }
                    } else {
                        // Wrong pass — keep for next iteration.
                        this.as_mut().next = None;
                        match kept_tail {
                            Some(mut tail) => tail.as_mut().next = Some(this),
                            None => kept_head = Some(this),
                        }
                        kept_tail = Some(this);
                    }
                }
            }

            remaining = GcNodeLink::new(kept_head);
            if remaining.head().is_none() {
                break;
            }
        }

        #[cfg(feature = "gc_arena")]
        if let Some(ref arena) = self.partitions[partition_id.0 as usize].arena {
            arena.finish_sweep(&mut holes);
        }

        debug_assert!(
            self.partitions[partition_id.0 as usize]
                .gray_list
                .is_empty()
        );

        freed_bytes
    }

    /// Collect garbage on given partition: mark all reachable nodes, then
    /// sweep unmarked (white) nodes.
    ///
    /// ⚠️ **SAFETY CONTRACT** — this method holds `&mut GcHeap` across the
    /// payload-drop phase (unavoidable for any `&mut self` entry point), so
    /// it is only sound if no registered type's `drop_fn` re-enters the
    /// heap/runtime (i.e. no `Drop` impl derefs `RuntimePtr`). Payloads
    /// holding plain values (`Box<i32>`, `Vec<T>` without heap callbacks)
    /// are fine.
    ///
    /// Runtimes whose GC-node `Drop` impls access `RuntimePtr` (e.g.
    /// `StringImpl::drop` → atom removal) MUST use the three-step API
    /// instead — [`sweep_unlink`] + [`sweep_drop_payloads`] +
    /// [`sweep_dispose`] — so the drop phase runs with no `&mut GcHeap`
    /// alive on the stack (invisible to the borrow checker, visible to LTO).
    #[inline]
    pub fn garbage_collect(&mut self, partition_id: GcPartitionId) -> usize {
        if self.partition(partition_id).is_none() {
            return 0;
        }

        while !self.mark(partition_id, 64) {}

        if let Some(white) = self.sweep_unlink(partition_id) {
            Self::sweep_drop_payloads(&white, self.node_dtypes);
            self.sweep_dispose(partition_id, white)
        } else {
            0
        }
    }

    /// Dispose all nodes along chain
    pub(crate) fn dispose_all_nodes(
        &mut self,
        mut link: GcNodeLink,
        on_dispose: impl Fn(&GcHeap, &GcHead),
    ) -> usize {
        let call_on_dispose = !std::ptr::addr_eq(&on_dispose, &Self::DUMMY_DISPOSE_CALLBACK);
        let mut freed_bytes = 0;

        let pass_slice = self.node_dtypes.drop_passes;
        for &pass in pass_slice {
            log::trace!("[dipose_all] pass {pass}, count={}", link.len());

            let self_ptr: *mut GcHeap = self;

            link.filter_remove_with(
                |node| {
                    #[cfg(debug_assertions)]
                    unsafe {
                        node.debug_assert_node_valid(&*self_ptr);
                    }

                    let dtype = node.dtype() as usize;
                    let info = unsafe { &(*self_ptr).node_dtypes.type_info_list[dtype] };
                    info.drop_pass == pass
                },
                |node_ptr| unsafe {
                    if call_on_dispose {
                        on_dispose(&*self_ptr, node_ptr.as_ref());
                    }

                    freed_bytes += (&mut *self_ptr).dispose(node_ptr);
                },
            );

            if link.head().is_none() {
                break;
            }
        }

        debug_assert!(
            link.head().is_none(),
            "dispose_all_nodes: link still has nodes after disposal",
        );
        log::trace!("[dipose_all] done, freed {} bytes", freed_bytes);

        freed_bytes
    }
}

#[cfg(test)]
mod sweep_test {
    use super::*;
    use crate::{GcNode, GcRef};

    use crate::trace::{GcTrace, GcTraceCtx};

    /// Shorthand for creating a partition with default arena config.
    fn create_default_partition(heap: &mut crate::GcHeap) -> GcPartitionId {
        heap.create_partition(crate::arena::ARENA_CAPACITY, crate::arena::MAX_ARENA_ALLOC)
    }

    #[derive(Debug)]
    struct MyI32(i32);

    impl GcTrace for MyI32 {
        fn trace(&self, _: &mut GcTraceCtx) {}
    }

    crate::gc_type_register! {
        MyI32, drop_pass = 0;
    }

    /// Helper function to count nodes in a partition
    fn count_nodes_in_partition(heap: &GcHeap, partition_id: GcPartitionId) -> usize {
        heap.nodes(partition_id).count()
    }

    /// Helper function to get all node pointers in a partition
    fn get_all_nodes_in_partition(
        heap: &GcHeap,
        partition_id: GcPartitionId,
    ) -> Vec<NonNull<GcHead>> {
        heap.nodes(partition_id).collect()
    }

    /// Two-phase sweep helper: unlink → drop payloads → dispose.
    fn two_phase_sweep(heap: &mut GcHeap, pid: GcPartitionId) -> usize {
        if let Some(white) = heap.sweep_unlink(pid) {
            let registry = heap.type_registry();
            GcHeap::sweep_drop_payloads(&white, registry);
            heap.sweep_dispose(pid, white)
        } else {
            0
        }
    }

    // ── sweep_unlink / sweep_dispose tests ──────────────────────────────

    #[test]
    fn test_sweep_unlink_returns_none_when_not_marking() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = create_default_partition(&mut heap);

        // No marking cycle started → sweep_unlink returns None.
        assert!(heap.sweep_unlink(pid).is_none());
    }

    #[test]
    fn test_sweep_unlink_returns_none_for_nonexistent_partition() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        assert!(heap.sweep_unlink(GcPartitionId(9999)).is_none());
    }

    #[test]
    fn test_two_phase_sweep_basic() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = create_default_partition(&mut heap);

        let objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| unsafe { heap.alloc_raw(pid, MyI32(i)) }.unwrap())
            .collect();

        // Mark odd-indexed as root (survives GC)
        for (i, obj) in objects.iter().enumerate() {
            if i % 2 == 1 {
                unsafe {
                    let head = obj.head_ptr.as_ptr();
                    let attrs = (*head).attrs | crate::node::GcNodeFlag::ROOT.bits() as u32;
                    std::ptr::write(&mut (*head).attrs, attrs);
                }
            }
        }

        assert_eq!(count_nodes_in_partition(&heap, pid), 5);

        while !heap.mark(pid, 64) {}
        let freed = two_phase_sweep(&mut heap, pid);

        assert!(freed > 0);
        assert_eq!(
            count_nodes_in_partition(&heap, pid),
            2,
            "Only root nodes remain"
        );

        let remaining = get_all_nodes_in_partition(&heap, pid);
        let info = &GC_TYPE_REGISTRY.type_info_list[MyI32::GC_TYPE_ID as usize];
        for node in remaining {
            unsafe {
                let value = *(info.payload_ptr(node).as_ptr() as *const i32);
                assert_eq!(value % 2, 1, "Remaining nodes should have odd values");
            }
        }
    }

    #[test]
    fn test_two_phase_sweep_chain_head_removal() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = create_default_partition(&mut heap);

        let _objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| unsafe { heap.alloc_raw(pid, MyI32(i)) }.unwrap())
            .collect();

        let _ = unsafe { heap.alloc_root_raw(pid, MyI32(3)) }.unwrap();
        let _ = unsafe { heap.alloc_root_raw(pid, MyI32(4)) }.unwrap();

        while !heap.mark(pid, 64) {}
        let freed = two_phase_sweep(&mut heap, pid);

        assert!(freed > 0);
        assert_eq!(count_nodes_in_partition(&heap, pid), 2);

        let head = heap.partitions[pid.0 as usize].nodes.head();
        assert!(head.is_some());

        unsafe {
            let info = &GC_TYPE_REGISTRY.type_info_list[MyI32::GC_TYPE_ID as usize];
            let value = (*(info.payload_ptr(head.unwrap()).as_ptr() as *const MyI32)).0;
            assert_eq!(value, 4, "Chain head should be value 4");
        }

        let nodes = get_all_nodes_in_partition(&heap, pid);
        assert_eq!(nodes.len(), 2);

        unsafe {
            let info = &GC_TYPE_REGISTRY.type_info_list[MyI32::GC_TYPE_ID as usize];
            assert_eq!(
                (*(info.payload_ptr(nodes[0]).as_ptr() as *const MyI32)).0,
                4
            );
            assert_eq!(
                (*(info.payload_ptr(nodes[1]).as_ptr() as *const MyI32)).0,
                3
            );
        }
    }

    #[test]
    fn test_two_phase_sweep_all_removed() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = create_default_partition(&mut heap);

        let _objects: Vec<GcRef<MyI32>> = (0..3)
            .map(|i| unsafe { heap.alloc_raw(pid, MyI32(i)) }.unwrap())
            .collect();

        while !heap.mark(pid, 64) {}
        let freed = two_phase_sweep(&mut heap, pid);

        assert!(freed > 0);
        assert_eq!(count_nodes_in_partition(&heap, pid), 0);
        assert!(heap.partitions[pid.0 as usize].nodes.head().is_none());
    }

    #[test]
    fn test_two_phase_sweep_middle_node_removal() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = create_default_partition(&mut heap);

        let _objects: Vec<GcRef<MyI32>> = (0..5)
            .map(|i| {
                if i != 2 {
                    unsafe { heap.alloc_root_raw(pid, MyI32(i)) }.unwrap()
                } else {
                    unsafe { heap.alloc_raw(pid, MyI32(i)) }.unwrap()
                }
            })
            .collect();

        while !heap.mark(pid, 64) {}
        let freed = two_phase_sweep(&mut heap, pid);

        assert!(freed > 0);
        assert_eq!(count_nodes_in_partition(&heap, pid), 4);

        let nodes = get_all_nodes_in_partition(&heap, pid);
        let expected_values = [4, 3, 1, 0];
        let info = &GC_TYPE_REGISTRY.type_info_list[MyI32::GC_TYPE_ID as usize];
        for (i, node) in nodes.iter().enumerate() {
            unsafe {
                let value = (*(info.payload_ptr(*node).as_ptr() as *const MyI32)).0;
                assert_eq!(value, expected_values[i]);
            }
        }
    }

    #[test]
    fn test_two_phase_sweep_root_removal() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = create_default_partition(&mut heap);

        let root_obj = unsafe { heap.alloc_raw(pid, MyI32(0)) }.unwrap();
        let _objects: Vec<GcRef<MyI32>> = (1..3)
            .map(|i| unsafe { heap.alloc_raw(pid, MyI32(i)) }.unwrap())
            .collect();
        let _ = unsafe { heap.alloc_root_raw(pid, MyI32(1)) }.unwrap();
        let _ = unsafe { heap.alloc_root_raw(pid, MyI32(1)) }.unwrap();

        while !heap.mark(pid, 64) {}
        let freed = two_phase_sweep(&mut heap, pid);

        assert!(freed > 0);
        assert_eq!(count_nodes_in_partition(&heap, pid), 2);

        let remaining = get_all_nodes_in_partition(&heap, pid);
        assert!(!remaining.contains(&root_obj.head_ptr));
    }

    #[test]
    fn test_two_phase_sweep_empty_partition() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = create_default_partition(&mut heap);

        while !heap.mark(pid, 64) {}
        let freed = two_phase_sweep(&mut heap, pid);
        assert_eq!(freed, 0);
    }

    #[test]
    fn test_two_phase_sweep_nonexistent_partition() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let freed = two_phase_sweep(&mut heap, GcPartitionId(9999));
        assert_eq!(freed, 0);
    }
}
