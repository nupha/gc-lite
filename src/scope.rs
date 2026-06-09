use {
    core::ptr::NonNull,
    std::{cell::RefCell, marker::PhantomData, num::NonZeroU8, ops::DerefMut},
};

use smallvec::SmallVec;

use crate::{
    heap::GcHeap,
    helpers::GcError,
    node::{Gc, GcHead, GcNode, GcNodeFlag},
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
    depth: NonZeroU8,
    cache: RefCell<SmallVec<[NonNull<GcHead>; 8]>>,
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
            !partition_id.is_null(),
            "GcScopeState partition_id must not be null"
        );
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
            depth: NonZeroU8::new(depth).unwrap(),
            cache: RefCell::new(SmallVec::new()),
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
    pub fn depth(&self) -> u8 {
        self.depth.get()
    }

    pub fn count(&self) -> usize {
        self.cache.borrow().len()
    }

    /// get parent scope, and its level.
    pub fn parent(&self) -> Option<(&GcScopeState<'_>, u8)> {
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

        self.cache.borrow_mut().push(node);
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
    /// # Safety
    ///
    /// The caller must ensure the node is still alive and valid.
    /// This method does NOT check whether the node is the current promote node;
    /// the caller (e.g., `ScopedContext::throw`) is responsible for ensuring
    /// the node is not the promote target.
    pub fn remove_node(&self, mut node: NonNull<GcHead>) -> bool {
        #[cfg(debug_assertions)]
        unsafe {
            node.as_ref().debug_assert_node_valid_simple();
        }
        let mut cache = self.cache.borrow_mut();
        if let Some(pos) = cache.iter().position(|&n| n == node) {
            cache.swap_remove(pos);
            unsafe {
                node.as_mut().remove_flag(GcNodeFlag::LOCAL);
            }
            true
        } else {
            false
        }
    }

    pub fn contains(&self, node: NonNull<GcHead>) -> bool {
        self.cache.borrow().contains(&node)
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
    pub fn promote_node_to(&self, node: NonNull<GcHead>, target: &GcScopeState<'_>) -> bool {
        let mut cache = self.cache.borrow_mut();

        if let Some(pos) = cache.iter().position(|&n| n == node) {
            debug_assert!(
                unsafe { node.as_ref().is_local() },
                "promote_node_to: node is not LOCAL",
            );
            cache.swap_remove(pos);
            drop(cache);
            target.cache.borrow_mut().push(node);
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
        let lst = std::mem::take(self.cache.borrow_mut().deref_mut());
        for mut n in lst {
            unsafe {
                n.as_mut().remove_flag(crate::node::GcNodeFlag::LOCAL);
            }
        }
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
    index: u8,
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
                stack.list.len() as u8 - 1,
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
    pub fn scope_max_depth(&self, stack_id: GcScopeStackId) -> u8 {
        self.scope_stacks[stack_id.0 as usize].list.len() as _
    }

    /// get scope state specified by (stack_id, depth).
    /// where `depth` is 1-based (1 means index #0)
    pub fn scope(&self, stack_id: GcScopeStackId, depth: u8) -> Option<&GcScopeState<'_>> {
        if depth > 0 {
            self.scope_stacks[stack_id.0 as usize]
                .list
                .get(depth as usize - 1)
        } else {
            None
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

        let depth = stack.list.len() as u8 + 1;
        let state = GcScopeState {
            heap,
            stack_id,
            partition_id: stack.partition.unwrap(),
            depth: NonZeroU8::new(depth).unwrap(),
            cache: RefCell::new(SmallVec::new()),
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
        let partition_id = heap.create_partition();
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

    #[test]
    fn test_gc_context_alloc_protects_and_unprotects_on_drop() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();
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
            let removed = ctx
                .heap_mut()
                .sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
            assert_eq!(removed, 0);
            ctx.clear();
        }

        unsafe {
            assert!(!head.as_ref().is_local());
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after > 0);
    }

    #[test]
    fn test_gc_context_add_sets_local_and_clears_on_commit() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();
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
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after > 0);
    }

    #[test]
    fn test_gc_context_add_on_local_node_returns_false() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();
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
        let partition_id = heap.create_partition();

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
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(removed_after, 0);
    }

    #[test]
    fn test_gc_context_alloc_multiple_nodes() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();
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
            let removed = ctx
                .heap_mut()
                .sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
            assert_eq!(removed, 0);
            ctx.clear();
        }

        unsafe {
            assert!(!head1.as_ref().is_local());
            assert!(!head2.as_ref().is_local());
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after >= 2);
    }

    #[test]
    fn test_gc_context_reset_unprotects_and_clears_cache() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();
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
            let removed_before = ctx
                .heap_mut()
                .sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
            assert!(removed_before > 0);
        }
    }

    #[test]
    fn test_gc_context_level_for_heap_scopes() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();
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
        let partition_id = heap.create_partition();
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
        let partition_id = heap.create_partition();
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
        let partition_id = heap.create_partition();
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
}
