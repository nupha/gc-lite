use {
    core::ptr::NonNull,
    std::{cell::UnsafeCell, marker::PhantomData, num::NonZeroU32},
};

#[cfg(debug_assertions)]
use std::cell::Cell;

use crate::{
    heap::GcHeap,
    helpers::GcError,
    node::{Gc, GcHead, GcNode, GcNodeFlag, SlotHint},
    partition::GcPartitionId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct GcScopeStackId(u16);

impl std::fmt::Display for GcScopeStackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub(crate) struct ScopeStack {
    /// sequence of scopes
    pub(crate) list: Vec<GcScopeState<'static>>,
    /// which partition this stack is associated with, None means the stack is not used
    pub(crate) partition: Option<GcPartitionId>,
}

impl ScopeStack {
    pub(super) fn new(partition: Option<GcPartitionId>) -> Self {
        Self {
            list: Vec::with_capacity(8),
            partition,
        }
    }
}

/// The GC scope state
#[derive(Debug)]
pub struct GcScopeState<'s> {
    heap: NonNull<GcHeap>,
    partition_id: GcPartitionId,
    stack_id: GcScopeStackId,
    depth: NonZeroU32,
    cache: UnsafeCell<Option<Vec<NonNull<GcHead>>>>,
    #[cfg(debug_assertions)]
    borrow_flag: Cell<bool>,
    _marker: PhantomData<&'s ()>,
}

impl<'s> Drop for GcScopeState<'s> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<'s> GcScopeState<'s> {
    pub fn new(
        heap: &'s mut GcHeap,
        stack_id: GcScopeStackId,
        partition_id: GcPartitionId,
    ) -> Self {
        debug_assert!(
            (stack_id.0 as usize) < heap.scope_stacks.len(),
            "GcScopeState stack_id {} out of bounds (max {})",
            stack_id.0,
            heap.scope_stacks.len(),
        );

        let depth = heap.scope_max_depth(stack_id) + 1;
        Self {
            heap: NonNull::from_ref(heap),
            stack_id,
            partition_id,
            depth: NonZeroU32::new(depth).unwrap(),
            cache: UnsafeCell::new(None),
            #[cfg(debug_assertions)]
            borrow_flag: Cell::new(false),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub fn partition_id(&self) -> GcPartitionId {
        self.partition_id
    }

    #[inline(always)]
    pub fn stack_id(&self) -> GcScopeStackId {
        self.stack_id
    }

    #[inline(always)]
    pub fn heap(&self) -> &GcHeap {
        unsafe { self.heap.as_ref() }
    }

    #[inline(always)]
    pub fn heap_mut(&mut self) -> &mut GcHeap {
        unsafe { self.heap.as_mut() }
    }

    #[inline(always)]
    pub fn depth(&self) -> u32 {
        self.depth.get()
    }

    #[inline(always)]
    pub fn count(&self) -> usize {
        unsafe { (*self.cache.get()).as_ref().map_or(0, |c| c.len()) }
    }

    /// get parent scope, and its level.
    pub fn parent(&self) -> Option<(&GcScopeState<'_>, u32)> {
        let d = self.depth();
        if d > 1 {
            let parent_index = d - 2;
            self.heap().scope_stacks[self.stack_id.0 as usize]
                .list
                .get(parent_index as usize)
                .map(|s| (s, d - 1))
        } else {
            None
        }
    }

    /// alloc a local node in scope.
    pub fn alloc_local<T: GcNode>(&self, payload: T) -> Result<Gc<'s, T>, (GcError, T)> {
        let r = unsafe { (*self.heap.as_ptr()).alloc_raw(self.partition_id, payload)? };
        self.add_node(r.head_ptr);
        // SAFETY: The node is now protected by this scope's LOCAL flag,
        // and 's is tied to the GcScopeState's lifetime (which outlives
        // this call and is typically tied to a &mut GcHeap borrow,
        // preventing GC collection).
        Ok(unsafe { Gc::from_raw(r) })
    }

    fn add_node(&self, mut node: NonNull<GcHead>) {
        unsafe {
            debug_assert!(
                !node.as_ref().is_root_or_local(),
                "add_node: node is already root or local",
            );

            node.as_mut().insert_flag(GcNodeFlag::LOCAL);

            if let Some(par) = (*self.heap.as_ptr()).partition_mut(self.partition_id)
                && par.is_marking()
            {
                par.add_gray_node(node);
            }
        }

        #[cfg(debug_assertions)]
        {
            debug_assert!(!self.borrow_flag.get(), "GcScopeState: reentrant borrow");
            self.borrow_flag.set(true);
        }

        unsafe {
            let cache = &mut *self.cache.get();
            let cache = cache.get_or_insert_with(|| Vec::with_capacity(1));
            node.as_mut().cache_slot = SlotHint::of(cache.len());
            cache.push(node);
        }

        #[cfg(debug_assertions)]
        self.borrow_flag.set(false);
    }

    // if node is neither root, nor local, then add it to `self` scope
    pub fn add_non_local(&self, node: NonNull<GcHead>) -> bool {
        if unsafe { node.as_ref().is_root_or_local() } {
            false
        } else {
            self.add_node(node);
            true
        }
    }

    /// Remove a LOCAL node from this scope's cache and clear its LOCAL flag.
    ///
    /// This is the inverse of `add_node`: it undoes the LOCAL protection
    /// that was granted when the node was allocated in this scope.
    /// After this call, the node is no longer protected by this scope
    /// and will be traced by GC solely through other GC references.
    ///
    /// Returns `true` if the node was found and removed from this scope's cache.
    /// Returns `false` if the node was not in this scope's cache (e.g., it was
    /// already promoted, or belongs to a different scope).
    ///
    /// O(1) on the recorded slot hint; a cache that lost it (see [`clear`])
    /// falls back to a scan.
    ///
    /// # Safety
    ///
    /// The caller must ensure the node is still alive and valid.
    /// This method does NOT check whether the node is the current promote node;
    /// the caller (e.g., `ScopedContext::throw`) is responsible for ensuring
    /// the node is not the promote target.
    ///
    /// [`clear`]: GcScopeState::clear
    pub fn remove_node(&self, mut node: NonNull<GcHead>) -> bool {
        #[cfg(debug_assertions)]
        unsafe {
            node.as_ref().debug_assert_node_valid_simple();
        }
        let cache_opt = unsafe { &mut *self.cache.get() };
        let Some(cache) = cache_opt.as_mut() else {
            return false;
        };
        let Some(pos) = locate(cache, node) else {
            return false;
        };
        swap_remove_at(cache, pos);
        unsafe {
            node.as_mut().cache_slot = SlotHint::NONE;
            node.as_mut().remove_flag(GcNodeFlag::LOCAL);
        }
        true
    }

    pub fn contains(&self, node: NonNull<GcHead>) -> bool {
        unsafe {
            (*self.cache.get())
                .as_ref()
                .is_some_and(|c| c.contains(&node))
        }
    }

    /// Move a node from this scope's cache to the target scope's cache.
    ///
    /// The node retains its LOCAL flag — it is simply transferred from one
    /// scope's protection to another's. This is used when a return value
    /// needs to be moved from a child scope to its parent scope before
    /// the child scope is destroyed.
    ///
    /// Returns `true` if the node was found in this scope and moved.
    /// Returns `false` if the node was not in this scope's cache (e.g., it
    /// is a root node, or already belongs to another scope).
    ///
    /// O(1) on the recorded slot hint, like [`remove_node`].
    ///
    /// [`remove_node`]: GcScopeState::remove_node
    pub fn promote_node_to(&self, node: NonNull<GcHead>, target: &GcScopeState<'_>) -> bool {
        let cache_opt = unsafe { &mut *self.cache.get() };

        if let Some(cache) = cache_opt.as_mut()
            && let Some(pos) = locate(cache, node)
        {
            debug_assert!(
                unsafe { node.as_ref().is_local() },
                "promote_node_to: node is not LOCAL",
            );
            swap_remove_at(cache, pos);
            let target_cache = unsafe { &mut *target.cache.get() };
            let target_cache = target_cache.get_or_insert_with(|| Vec::with_capacity(1));
            unsafe {
                node.as_ptr().as_mut().unwrap().cache_slot = SlotHint::of(target_cache.len());
            }
            target_cache.push(node);
            true
        } else {
            // Node not found in this scope's cache. This is safe — the node
            // must be reachable through some other GC root or LOCAL node,
            // otherwise it would have been collected. Common scenarios:
            //
            //   - ROOT node: never in any scope cache, no protection needed.
            //   - LOCAL in an ancestor scope: already protected by that scope.
            //   - flags=0 (was LOCAL, scope was cleared): the node is still
            //     alive because it is referenced by another GC root/LOCAL
            //     (e.g., a Bytecode constant pool, an object property, an
            //     array element, a closure variable). It does not need scope
            //     protection — it is kept alive by its referrer.
            false
        }
    }

    // clear and unprotect locals nodes in this scope, remote LOCAL flag of each
    pub fn clear(&self) {
        let cache_opt = unsafe { &mut *self.cache.get() };
        if let Some(lst) = cache_opt.take() {
            for mut n in lst {
                unsafe {
                    n.as_mut().remove_flag(crate::node::GcNodeFlag::LOCAL);
                    // The cache is gone: drop the hint so a later removal does
                    // not trust an index into a cache that no longer exists.
                    n.as_mut().cache_slot = SlotHint::NONE;
                }
            }
        }
    }
}

/// Index of `node` in `cache`: the node's slot hint when it still names it,
/// otherwise a linear scan. The hint makes the common case O(1); the scan keeps
/// the answer right whenever the hint is stale, and the identity check inside
/// [`SlotHint::names`] is what makes a stale hint harmless instead of a wrong
/// removal.
#[inline]
fn locate(cache: &[NonNull<GcHead>], node: NonNull<GcHead>) -> Option<usize> {
    if let Some(pos) = unsafe { node.as_ref().cache_slot }.names(cache, node) {
        return Some(pos);
    }
    #[cfg(test)]
    SCANS.with(|s| s.set(s.get() + 1));
    cache.iter().position(|&n| n == node)
}

#[cfg(test)]
thread_local! {
    /// Test witness for "the hint answered": the number of cache scans `locate`
    /// had to do on this thread. O(1) removal must leave it unchanged.
    static SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_scans() -> usize {
    SCANS.with(|s| s.replace(0))
}

/// `Vec::swap_remove`, keeping the slot hint of the node it moves into `pos`.
#[inline]
fn swap_remove_at(cache: &mut Vec<NonNull<GcHead>>, pos: usize) {
    cache.swap_remove(pos);
    if let Some(moved) = cache.get(pos).copied() {
        unsafe { moved.as_ptr().as_mut().unwrap().cache_slot = SlotHint::of(pos) };
    }
}

/// Handle to a GC scope, used to:
///
/// 1. reference the scope state
/// 2. manage lifetime of the scope, pop the scope when the handle is dropped
#[derive(Debug)]
pub struct GcScope<'s> {
    heap: NonNull<GcHeap>,
    stack_id: GcScopeStackId,
    index: u32,
    _marker: PhantomData<&'s ()>,
}

impl<'s> Drop for GcScope<'s> {
    fn drop(&mut self) {
        unsafe {
            let heap = self.heap.as_mut();
            let stack = &heap.scope_stacks[self.stack_id.0 as usize];

            debug_assert!(
                !stack.list.is_empty(),
                "GcScope dropped but scope stack {} is empty",
                self.stack_id.0,
            );
            debug_assert_eq!(
                stack.list.len() as u32 - 1,
                self.index,
                "GcScope dropped out of LIFO order: scope stack {} has {} entries, expected top index {}",
                self.stack_id.0,
                stack.list.len(),
                self.index,
            );

            heap.pop_scope(self.stack_id);
        }
    }
}

impl<'s> std::ops::Deref for GcScope<'s> {
    type Target = GcScopeState<'s>;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        unsafe {
            let stack = &self.heap.as_ref().scope_stacks[self.stack_id.0 as usize];
            debug_assert!(
                (self.index as usize) < stack.list.len(),
                "GcScope index {} out of bounds for stack {} (len {})",
                self.index,
                self.stack_id.0,
                stack.list.len(),
            );

            std::mem::transmute::<&GcScopeState<'_>, &GcScopeState<'s>>(
                stack.list.get(self.index as usize).unwrap_unchecked(),
            )
        }
    }
}

impl<'s> std::ops::DerefMut for GcScope<'s> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe {
            let stack = &mut self.heap.as_mut().scope_stacks[self.stack_id.0 as usize];
            debug_assert!(
                (self.index as usize) < stack.list.len(),
                "GcScope index {} out of bounds for stack {} (len {})",
                self.index,
                self.stack_id.0,
                stack.list.len(),
            );

            std::mem::transmute::<&mut GcScopeState<'_>, &mut GcScopeState<'s>>(
                stack.list.get_mut(self.index as usize).unwrap_unchecked(),
            )
        }
    }
}

impl GcHeap {
    /// find an idle scope stack, or create a new if none available
    pub fn acquire_scope_stack(&mut self, par: GcPartitionId) -> GcScopeStackId {
        for (id, stack) in self.scope_stacks.iter_mut().enumerate() {
            if stack.partition.is_none() {
                debug_assert!(
                    stack.list.is_empty(),
                    "acquire_scope_stack: idle stack {} has non-empty scope list",
                    id,
                );
                stack.partition = Some(par);
                return GcScopeStackId(id as u16);
            }
        }

        let id = u16::try_from(self.scope_stacks.len()).expect("too many scope stacks");
        self.scope_stacks.push(ScopeStack::new(Some(par)));
        GcScopeStackId(id)
    }

    /// Release the LOCAL protection `node` holds, wherever it lives.
    ///
    /// A value can be cached on a scope of a *different* stack than the caller
    /// (cross-realm exceptions are the known case). Clearing only the flag
    /// would leave the node in that scope's cache; the cache's later
    /// [`clear`](GcScopeState::clear) would then write to a node the GC may
    /// already have freed. Returns `true` when some scope held it.
    pub fn release_local(&self, node: NonNull<GcHead>) -> bool {
        for stack in &self.scope_stacks {
            for state in &stack.list {
                if state.remove_node(node) {
                    return true;
                }
            }
        }
        false
    }

    pub fn release_scope_stack(&mut self, stack_id: GcScopeStackId) {
        let stack = &mut self.scope_stacks[stack_id.0 as usize];
        debug_assert!(
            stack.list.is_empty(),
            "release_scope_stack: stack {} has {} scopes still active",
            stack_id.0,
            stack.list.len(),
        );
        stack.partition.take();
    }

    /// get max depth of a scope stack
    #[inline(always)]
    pub fn scope_max_depth(&self, stack_id: GcScopeStackId) -> u32 {
        self.scope_stacks[stack_id.0 as usize].list.len() as _
    }

    /// get scope state specified by (stack_id, depth).
    /// where `depth` is 1-based (1 means index #0)
    pub fn scope(&self, stack_id: GcScopeStackId, depth: u32) -> Option<&GcScopeState<'_>> {
        if depth > 0 {
            self.scope_stacks[stack_id.0 as usize]
                .list
                .get(depth as usize - 1)
        } else {
            None
        }
    }

    /// Get scope state by 0-based index without bounds check.
    ///
    /// # Safety
    ///
    /// Caller must ensure `index < scope_max_depth(stack_id)`.
    #[inline(always)]
    pub unsafe fn scope_unchecked(
        &self,
        stack_id: GcScopeStackId,
        index: u32,
    ) -> &GcScopeState<'_> {
        debug_assert!(
            (index as usize) < self.scope_stacks[stack_id.0 as usize].list.len(),
            "scope_unchecked: index {index} out of bounds for stack {} (len {})",
            stack_id.0,
            self.scope_stacks[stack_id.0 as usize].list.len(),
        );
        // SAFETY: caller ensures index < scope_max_depth(stack_id)
        unsafe {
            std::mem::transmute::<&GcScopeState<'static>, &GcScopeState<'_>>(
                self.scope_stacks[stack_id.0 as usize]
                    .list
                    .get_unchecked(index as usize),
            )
        }
    }

    /// pop the last scope from the stack
    #[inline(always)]
    fn pop_scope(&mut self, stack_id: GcScopeStackId) -> Option<GcScopeState<'_>> {
        self.scope_stacks[stack_id.0 as usize].list.pop()
    }

    /// push a new scope on the stack, returns new scope's handle.
    /// the scope will be automatically popped when this handle is dropped.
    ///
    /// # Panics
    ///
    /// Panics if an earlier handle is dropped while a later handle is still alive
    /// (LIFO order violation is enforced at runtime).
    pub fn new_scope<'s>(&'s mut self, stack_id: GcScopeStackId) -> GcScope<'s> {
        let heap = NonNull::from_ref(self);
        let stack = &mut self.scope_stacks[stack_id.0 as usize];
        debug_assert!(
            stack.partition.is_some(),
            "scope stack {stack_id:?} is not acquired"
        );

        let list_len = stack.list.len();
        if list_len >= u32::MAX as usize {
            panic!("scope stack overflow: depth {} exceeds u32::MAX", list_len);
        }
        let depth = list_len as u32 + 1;
        let state = GcScopeState {
            heap,
            stack_id,
            partition_id: stack.partition.unwrap(),
            depth: NonZeroU32::new(depth).unwrap(),
            cache: UnsafeCell::new(None),
            #[cfg(debug_assertions)]
            borrow_flag: Cell::new(false),
            _marker: PhantomData,
        };

        stack.list.push(unsafe {
            // SAFETY: It is safe because the GcHeap owns the GcScope, and we ensure that
            // the GcScope does not outlive the GcHeap.
            std::mem::transmute::<GcScopeState<'_>, GcScopeState<'static>>(state)
        });

        GcScope {
            heap,
            stack_id,
            index: depth - 1,
            _marker: PhantomData,
        }
    }

    /// get last (top-most) scope from the stack
    #[inline]
    pub fn current_scope(&self, stack_id: GcScopeStackId) -> Option<&GcScopeState<'_>> {
        let s = self.scope_stacks[stack_id.0 as usize].list.last();
        unsafe {
            // SAFETY: It is safe because the GcHeap owns the GcScope, and we ensure that
            // the GcScope does not outlive the GcHeap.
            std::mem::transmute::<Option<&GcScopeState<'static>>, Option<&GcScopeState<'_>>>(s)
        }
    }

    /// run closure with last (top-most) scope from the stack
    #[inline]
    pub fn with_current_scope_of<R>(
        &mut self,
        stack_id: GcScopeStackId,
        f: impl FnOnce(&mut GcScopeState) -> R,
    ) -> Option<R> {
        self.scope_stacks[stack_id.0 as usize]
            .list
            .last_mut()
            .map(f)
    }

    #[inline]
    pub fn with_new_scope<R>(
        &mut self,
        stack_id: GcScopeStackId,
        f: impl FnOnce(GcScope<'_>) -> R,
    ) -> R {
        let scope = self.new_scope(stack_id);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(scope)));

        match result {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{GcRef, GcTraceCtx, trace::GcTrace};

    use super::*;

    // `create_partition` takes no parameters when `gc_arena` is disabled.
    #[cfg(feature = "gc_arena")]
    fn new_partition(heap: &mut GcHeap) -> GcPartitionId {
        heap.create_partition(64 * 1024, 16 * 1024)
    }

    #[cfg(not(feature = "gc_arena"))]
    fn new_partition(heap: &mut GcHeap) -> GcPartitionId {
        heap.create_partition()
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

    #[derive(Debug)]
    struct Node {
        next: Option<GcRef<Node>>,
        value: i32,
    }

    impl GcTrace for Node {
        fn trace(&self, tr: &mut GcTraceCtx) {
            if let Some(next) = self.next {
                tr.add(next);
            }
        }
    }

    crate::gc_type_register! {
        Node, drop_pass = 0;
    }

    #[test]
    fn test_gc_context_local_flag_set_and_cleared_on_commit() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack_id = heap.acquire_scope_stack(partition_id);

        let head;

        {
            let ctx = heap.new_scope(stack_id);
            let node = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.as_raw().node_ptr();

            unsafe {
                assert!(head.as_ref().is_local());
            }

            ctx.clear();

            unsafe {
                assert!(!head.as_ref().is_local());
            }
        }

        unsafe {
            assert!(!head.as_ref().is_local());
        }
    }

    /// Cross-realm case: the caller is on another scope stack, so it must be
    /// able to strip the LOCAL protection through the heap. Leaving the node in
    /// the owning cache while clearing the flag lets the GC free it, and that
    /// cache's later `clear()` writes to freed memory.
    #[test]
    fn test_release_local_removes_a_node_from_another_stacks_cache() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack_id = heap.acquire_scope_stack(partition_id);

        let ctx = heap.new_scope(stack_id);
        let node = ctx
            .alloc_local(Node {
                next: None,
                value: 1,
            })
            .unwrap();
        let head = node.as_raw().node_ptr();

        unsafe { assert!(head.as_ref().is_local()) };
        assert_eq!(ctx.count(), 1);

        assert!(
            ctx.heap().release_local(head),
            "the owning scope's cache holds the node"
        );

        unsafe { assert!(!head.as_ref().is_local()) };
        assert_eq!(ctx.count(), 0);
        assert!(!ctx.heap().release_local(head), "nothing holds it any more");
    }

    #[test]
    fn test_gc_context_alloc_protects_and_unprotects_on_drop() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack_id = heap.acquire_scope_stack(partition_id);

        let head;
        {
            let mut ctx = heap.new_scope(stack_id);
            let node = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.as_raw().node_ptr();

            unsafe {
                assert!(head.as_ref().is_local());
            }

            while !ctx.heap_mut().mark(partition_id, 64) {}
            let removed = two_phase_sweep(ctx.heap_mut(), partition_id);
            assert_eq!(removed, 0);
            ctx.clear();
        }

        unsafe {
            assert!(!head.as_ref().is_local());
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = two_phase_sweep(&mut heap, partition_id);
        assert!(removed_after > 0);
    }

    #[test]
    fn test_gc_context_add_sets_local_and_clears_on_commit() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack_id = heap.acquire_scope_stack(partition_id);

        let node: GcRef<Node> = unsafe {
            heap.alloc_raw(
                partition_id,
                Node {
                    next: None,
                    value: 1,
                },
            )
        }
        .unwrap();

        let head = node.head_ptr;

        unsafe {
            assert!(!head.as_ref().is_local());
        }

        {
            let ctx = heap.new_scope(stack_id);
            let added = ctx.add_non_local(head);
            assert!(added);

            unsafe {
                assert!(head.as_ref().is_local());
            }

            ctx.clear();

            unsafe {
                assert!(!head.as_ref().is_local());
            }
        }

        unsafe {
            assert!(!head.as_ref().is_local());
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = two_phase_sweep(&mut heap, partition_id);
        assert!(removed_after > 0);
    }

    #[test]
    fn test_gc_context_add_on_local_node_returns_false() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack_id = heap.acquire_scope_stack(partition_id);

        let ctx = heap.new_scope(stack_id);
        let node = ctx
            .alloc_local(Node {
                next: None,
                value: 1,
            })
            .unwrap();

        let head = node.as_raw().node_ptr();

        unsafe {
            assert!(head.as_ref().is_local());
        }
        let added = ctx.add_non_local(head);
        assert!(!added);

        unsafe {
            assert!(head.as_ref().is_local());
        }

        ctx.clear();

        unsafe {
            assert!(!head.as_ref().is_local());
        }
    }

    #[test]
    fn test_root_node_not_collected_by_sweep() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);

        let head;

        unsafe {
            let node = heap
                .alloc_root_raw(
                    partition_id,
                    Node {
                        next: None,
                        value: 1,
                    },
                )
                .unwrap();

            head = node.head_ptr;

            assert!(node.is_root());
            assert!(head.as_ref().is_root());
            assert!(!head.as_ref().is_local());
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = two_phase_sweep(&mut heap, partition_id);
        assert_eq!(removed_after, 0);
    }

    #[test]
    fn test_gc_context_alloc_multiple_nodes() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack_id = heap.acquire_scope_stack(partition_id);

        let head1;
        let head2;

        {
            let mut ctx = heap.new_scope(stack_id);
            let n1 = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();
            let n2 = ctx
                .alloc_local(Node {
                    next: None,
                    value: 2,
                })
                .unwrap();

            head1 = n1.as_raw().node_ptr();
            head2 = n2.as_raw().node_ptr();

            unsafe {
                assert!(head1.as_ref().is_local());
                assert!(head2.as_ref().is_local());
            }

            while !ctx.heap_mut().mark(partition_id, 64) {}
            let removed = two_phase_sweep(ctx.heap_mut(), partition_id);
            assert_eq!(removed, 0);
            ctx.clear();
        }

        unsafe {
            assert!(!head1.as_ref().is_local());
            assert!(!head2.as_ref().is_local());
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = two_phase_sweep(&mut heap, partition_id);
        assert!(removed_after >= 2);
    }

    #[test]
    fn test_gc_context_reset_unprotects_and_clears_cache() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack_id = heap.acquire_scope_stack(partition_id);

        let head;

        {
            let mut ctx = heap.new_scope(stack_id);
            let node = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.as_raw().node_ptr();

            unsafe {
                assert!(head.as_ref().is_local());
            }

            ctx.clear();

            unsafe {
                assert!(!head.as_ref().is_local());
            }

            while !ctx.heap_mut().mark(partition_id, 64) {}
            let removed_before = two_phase_sweep(ctx.heap_mut(), partition_id);
            assert!(removed_before > 0);
        }
    }

    #[test]
    fn test_gc_context_level_for_heap_scopes() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack_id = heap.acquire_scope_stack(partition_id);

        assert_eq!(heap.scope_max_depth(stack_id), 0);

        let heap_ptr = &mut heap as *mut GcHeap;
        unsafe {
            let s1 = (&mut *heap_ptr).new_scope(stack_id);
            let s2 = (&mut *heap_ptr).new_scope(stack_id);
            let s3 = (&mut *heap_ptr).new_scope(stack_id);

            assert_eq!(s3.depth(), 3);
            assert_eq!(s2.depth(), 2);
            assert_eq!(s1.depth(), 1);

            let ctx3 = (*heap_ptr).scope(stack_id, 3).unwrap();
            assert_eq!(ctx3.depth(), 3);
            let ctx2 = (*heap_ptr).scope(stack_id, 2).unwrap();
            assert_eq!(ctx2.depth(), 2);
            let ctx1 = (*heap_ptr).scope(stack_id, 1).unwrap();
            assert_eq!(ctx1.depth(), 1);
        }
    }

    #[test]
    fn test_gc_context_parent_mut_returns_parent_and_level() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack_id = heap.acquire_scope_stack(partition_id);

        let heap_ptr = &mut heap as *mut GcHeap;
        unsafe {
            let scope1 = (&mut *heap_ptr).new_scope(stack_id);
            let scope2 = (&mut *heap_ptr).new_scope(stack_id);
            let scope3 = (&mut *heap_ptr).new_scope(stack_id);

            assert_eq!(scope3.depth(), 3);
            let (parent, parent_level) = scope3.parent().unwrap();
            assert_eq!(parent_level, 2);
            assert_eq!(parent.depth(), 2);

            drop(scope3);

            assert_eq!(scope2.depth(), 2);
            let (parent, parent_level) = scope2.parent().unwrap();
            assert_eq!(parent_level, 1);
            assert_eq!(parent.depth(), 1);

            drop(scope2);

            assert_eq!(scope1.depth(), 1);
            assert!(scope1.parent().is_none());
        }
    }

    #[test]
    fn test_multi_scope_stacks_are_independent() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack1 = heap.acquire_scope_stack(partition_id);
        let stack2 = heap.acquire_scope_stack(partition_id);

        assert_ne!(stack1, stack2);
        assert_eq!(heap.scope_max_depth(stack1), 0);
        assert_eq!(heap.scope_max_depth(stack2), 0);

        let heap_ptr = &mut heap as *mut GcHeap;
        unsafe {
            let _s1a = (&mut *heap_ptr).new_scope(stack1);
            let _s2a = (&mut *heap_ptr).new_scope(stack2);
            let _s1b = (&mut *heap_ptr).new_scope(stack1);

            assert_eq!((*heap_ptr).scope_max_depth(stack1), 2);
            assert_eq!((*heap_ptr).scope_max_depth(stack2), 1);
            assert_eq!((*heap_ptr).current_scope(stack1).unwrap().depth(), 2);
            assert_eq!((*heap_ptr).current_scope(stack2).unwrap().depth(), 1);

            (*heap_ptr).with_current_scope_of(stack1, |ctx| {
                let (parent, parent_level) = ctx.parent().unwrap();
                assert_eq!(parent_level, 1);
                assert_eq!(parent.depth(), 1);
                assert_eq!(parent.stack_id(), stack1);
            });
        }

        assert_eq!(heap.scope_max_depth(stack1), 0);
        assert_eq!(heap.scope_max_depth(stack2), 0);

        heap.release_scope_stack(stack1);
        heap.release_scope_stack(stack2);

        let reused = heap.acquire_scope_stack(partition_id);
        assert_eq!(reused, stack1);
    }

    #[test]
    fn test_gc_scope_drop_is_lifo_per_stack() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = new_partition(&mut heap);
        let stack1 = heap.acquire_scope_stack(partition_id);
        let stack2 = heap.acquire_scope_stack(partition_id);
        let heap_ptr = &mut heap as *mut GcHeap;

        unsafe {
            let scope1 = (&mut *heap_ptr).new_scope(stack1);
            let scope2 = (&mut *heap_ptr).new_scope(stack2);

            drop(scope1);
            drop(scope2);
        }

        assert_eq!(heap.scope_max_depth(stack1), 0);
        assert_eq!(heap.scope_max_depth(stack2), 0);
    }

    // ── slot hints: removal without scanning the cache ──

    /// Whether this target has room for the hint (an 8-byte pointer leaves a
    /// hole before `next`). Without it the hint is a no-op and every removal
    /// falls through to the scan, so the tests below expect one scan per call.
    const HINT: bool = cfg!(target_pointer_width = "64");

    /// Remove `node` and mirror the cache's `swap_remove` locally, so the hint
    /// of every surviving node can be checked against its new index.
    fn remove_and_mirror(
        ctx: &GcScope<'_>,
        mirror: &mut Vec<NonNull<GcHead>>,
        node: NonNull<GcHead>,
    ) {
        let slot = mirror
            .iter()
            .position(|&n| n == node)
            .expect("node is in the mirror");
        assert_eq!(
            unsafe { node.as_ref().cache_slot },
            SlotHint::of(slot),
            "hint does not name the node's slot",
        );
        assert!(ctx.remove_node(node));
        mirror.swap_remove(slot);
        for (i, n) in mirror.iter().enumerate() {
            assert_eq!(
                unsafe { n.as_ref().cache_slot },
                SlotHint::of(i),
                "node {i} lost its hint after a swap_remove",
            );
        }
    }

    #[test]
    fn removal_uses_the_slot_hint_instead_of_scanning() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = new_partition(&mut heap);
        let sid = heap.acquire_scope_stack(pid);
        let ctx = heap.new_scope(sid);

        let mut mirror: Vec<NonNull<GcHead>> = (0..64)
            .map(|i| {
                ctx.alloc_local(Node {
                    next: None,
                    value: i,
                })
                .unwrap()
                .as_raw()
                .node_ptr()
            })
            .collect();

        // Middle-heavy order: every removal moves some other node into the
        // freed slot, which is exactly the case a scan used to pay for.
        let mut order = mirror.clone();
        order.rotate_left(7);
        take_scans();
        for node in order {
            remove_and_mirror(&ctx, &mut mirror, node);
            assert!(!ctx.contains(node));
        }
        assert_eq!(
            take_scans(),
            if HINT { 0 } else { 64 },
            "a hinted removal still scanned the cache",
        );
        assert_eq!(ctx.count(), 0);
    }

    #[test]
    fn a_stale_hint_never_removes_another_node() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = new_partition(&mut heap);
        let sid = heap.acquire_scope_stack(pid);
        let ctx = heap.new_scope(sid);

        let a = ctx
            .alloc_local(Node {
                next: None,
                value: 1,
            })
            .unwrap()
            .as_raw()
            .node_ptr();
        let b = ctx
            .alloc_local(Node {
                next: None,
                value: 2,
            })
            .unwrap()
            .as_raw()
            .node_ptr();

        // Forge the hint a node would carry after a cache that no longer
        // exists: it points at another node's slot.
        unsafe { b.as_ptr().as_mut().unwrap().cache_slot = SlotHint::of(0) };
        assert_eq!(take_scans(), 0);
        assert!(ctx.remove_node(b), "the scan fallback must still find b");
        assert_eq!(take_scans(), 1, "a wrong hint must fall back to the scan");
        assert!(ctx.contains(a), "a wrong hint removed the wrong node");

        // A node that is not in the cache at all: still a clean `false`.
        assert!(!ctx.remove_node(b));
        assert!(ctx.contains(a));
    }

    #[test]
    fn clear_invalidates_the_hints() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = new_partition(&mut heap);
        let sid = heap.acquire_scope_stack(pid);
        let ctx = heap.new_scope(sid);

        let heads: Vec<NonNull<GcHead>> = (0..8)
            .map(|i| {
                ctx.alloc_local(Node {
                    next: None,
                    value: i,
                })
                .unwrap()
                .as_raw()
                .node_ptr()
            })
            .collect();

        ctx.clear();
        for h in &heads {
            assert_eq!(
                unsafe { h.as_ref().cache_slot },
                SlotHint::NONE,
                "clear left a hint pointing into the dropped cache",
            );
            assert!(!ctx.remove_node(*h));
        }
    }

    #[test]
    fn promotion_carries_the_hint_to_the_target_scope() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = new_partition(&mut heap);
        let sid = heap.acquire_scope_stack(pid);
        // Two live handles need two `&mut` borrows; the scope stack is LIFO and
        // the heap outlives both (the other multi-scope tests do the same).
        let heap_ptr = &mut heap as *mut GcHeap;
        let outer = unsafe { (&mut *heap_ptr).new_scope(sid) };
        let kept = outer
            .alloc_local(Node {
                next: None,
                value: 0,
            })
            .unwrap()
            .as_raw()
            .node_ptr();

        let inner = unsafe { (&mut *heap_ptr).new_scope(sid) };
        let heads: Vec<NonNull<GcHead>> = (0..4)
            .map(|i| {
                inner
                    .alloc_local(Node {
                        next: None,
                        value: i,
                    })
                    .unwrap()
                    .as_raw()
                    .node_ptr()
            })
            .collect();

        // Promote the middle node: the source cache swaps its last node in and
        // the target cache gains it at the tail.
        take_scans();
        assert!(inner.promote_node_to(heads[1], &outer));
        assert_eq!(
            take_scans(),
            if HINT { 0 } else { 1 },
            "promotion scanned the source cache",
        );
        assert_eq!(
            unsafe { heads[1].as_ref().cache_slot },
            SlotHint::of(1),
            "the promoted node did not get the target's tail slot",
        );
        assert_eq!(
            unsafe { heads[3].as_ref().cache_slot },
            SlotHint::of(1),
            "the node swapped into the freed slot kept a stale hint",
        );
        assert_eq!(
            unsafe { kept.as_ref().cache_slot },
            SlotHint::of(0),
            "the first local of the target scope moved",
        );

        // And the promoted node is now removable from the target (a scan only
        // where the target has no room for a hint).
        take_scans();
        assert!(outer.remove_node(heads[1]));
        assert_eq!(take_scans(), if HINT { 0 } else { 1 });
    }

    /// Not a correctness gate: the per-removal cost with the slot hint versus
    /// with the hint destroyed (the linear scan this replaced). Run with
    /// `cargo test --lib hint_vs_scan_removal_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "micro-benchmark; run with --ignored --nocapture"]
    fn hint_vs_scan_removal_cost() {
        use std::time::Instant;

        let n = 20_000;
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let pid = new_partition(&mut heap);
        let sid = heap.acquire_scope_stack(pid);
        let ctx = heap.new_scope(sid);

        let alloc = |ctx: &GcScope<'_>, i: usize| {
            ctx.alloc_local(Node {
                next: None,
                value: i as i32,
            })
            .unwrap()
            .as_raw()
            .node_ptr()
        };

        let nodes: Vec<NonNull<GcHead>> = (0..n).map(|i| alloc(&ctx, i)).collect();
        let t = Instant::now();
        for &node in nodes.iter().rev() {
            assert!(ctx.remove_node(node));
        }
        let hinted = t.elapsed();

        // Same work, hint destroyed first: what `remove_node` used to do.
        let nodes: Vec<NonNull<GcHead>> = (0..n).map(|i| alloc(&ctx, i)).collect();
        let t = Instant::now();
        for &node in nodes.iter().rev() {
            unsafe { node.as_ptr().as_mut().unwrap().cache_slot = SlotHint::NONE };
            assert!(ctx.remove_node(node));
        }
        let scanned = t.elapsed();

        println!(
            "n={n}: hinted {:.1}ns/op, scanned {:.1}ns/op ({:.0}x)",
            hinted.as_nanos() as f64 / n as f64,
            scanned.as_nanos() as f64 / n as f64,
            scanned.as_secs_f64() / hinted.as_secs_f64(),
        );
    }
}
