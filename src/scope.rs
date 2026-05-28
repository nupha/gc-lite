use std::{num::NonZeroU8, ops::DerefMut};

use {core::ptr::NonNull, std::cell::RefCell, std::marker::PhantomData};

use smallvec::SmallVec;

use crate::{
    heap::GcHeap,
    helpers::GcError,
    node::{GcHead, GcNode, GcNodeFlag, GcRef},
    partition::GcPartitionId,
};

#[derive(Debug)]
pub struct GcScopeState<'s> {
    heap: NonNull<GcHeap>,
    partition_id: GcPartitionId,
    stack_id: u16,
    depth: NonZeroU8,
    cache: RefCell<SmallVec<[NonNull<GcHead>; 8]>>,
    promote: RefCell<Option<(NonNull<GcHead>, bool)>>,
    _marker: PhantomData<&'s mut GcHeap>,
}

impl<'s> Drop for GcScopeState<'s> {
    fn drop(&mut self) {
        self.flush();
    }
}

impl<'s> GcScopeState<'s> {
    pub fn new(heap: &'s mut GcHeap, stack_id: u16, partition_id: GcPartitionId) -> Self {
        debug_assert!(!partition_id.is_null());
        debug_assert!((stack_id as usize) < heap.scope_stacks.len());

        let depth = heap.scope_max_depth(stack_id) + 1;
        Self {
            heap: NonNull::from_ref(heap),
            stack_id,
            partition_id,
            depth: NonZeroU8::new(depth).unwrap(),
            cache: RefCell::new(SmallVec::new()),
            promote: RefCell::new(None),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub fn partition_id(&self) -> GcPartitionId {
        self.partition_id
    }

    #[inline(always)]
    pub fn stack_id(&self) -> u16 {
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

            self.heap().scope_stacks[self.stack_id as usize]
                .scopes
                .get(parent_index as usize)
                .map(|s| {
                    (
                        unsafe {
                            std::mem::transmute::<&GcScopeState<'static>, &GcScopeState<'_>>(s)
                        },
                        d - 1,
                    )
                })
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn alloc_root<T: GcNode>(&self, payload: T) -> Result<GcRef<T>, (GcError, T)> {
        unsafe { (*self.heap.as_ptr()).alloc_root_raw(self.partition_id, payload) }
    }

    /// alloc a local node in scope.
    pub fn alloc_local<T: GcNode>(&self, payload: T) -> Result<GcRef<T>, (GcError, T)> {
        let r = unsafe { (*self.heap.as_ptr()).alloc_raw(self.partition_id, payload)? };
        self.add_node(r.head_ptr);
        Ok(r)
    }

    fn add_node(&self, mut node: NonNull<GcHead>) {
        #[cfg(debug_assertions)]
        unsafe {
            debug_assert!(!node.as_ref().is_root_or_local());
        }

        unsafe {
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

    pub fn get_promote(&self) -> Option<NonNull<GcHead>> {
        self.promote.borrow().map(|(p, _)| p)
    }

    /// Set a node to be promoted.
    ///
    /// Promote means when the scope is dropped, the node will be added to upper scope.
    pub fn set_promote(&self, node: Option<NonNull<GcHead>>) {
        #[cfg(debug_assertions)]
        {
            debug_assert_eq!(
                self.depth(),
                self.heap().scope_max_depth(self.stack_id),
                "only inner-most scope can set_promote"
            );

            if let Some(n) = node {
                unsafe {
                    n.as_ref().debug_assert_node_valid(self.heap());
                }
            }
        }

        // Note: scope depth==1 indicates the top scope, which don't promote any node
        if self.depth() == 1 {
            debug_assert!(
                self.promote.borrow().is_none(),
                "top scope don't promote node"
            );
            return;
        }

        // check current promote node
        if let Some((mut p, was_non_local)) = self.promote.borrow_mut().take()
            && was_non_local
        {
            unsafe {
                p.as_mut().remove_flag(crate::node::GcNodeFlag::LOCAL);

                let mut lst = self.cache.borrow_mut();
                if let Some(i) = lst.iter().position(|&n| n == p) {
                    lst.swap_remove(i);
                }
            }
        }

        if let Some(n) = node {
            let h = unsafe { n.as_ref() };
            if h.is_root() {
                return; // root node don't need to be promoted
            }

            let is_non_local = if h.is_local() {
                if !self
                    .cache
                    .borrow()
                    .iter()
                    .any(|p| std::ptr::eq(n.as_ptr(), p.as_ptr()))
                {
                    // node is already in some other scope, can't promote by this
                    return;
                }
                false
            } else {
                self.add_node(n);
                true
            };

            *self.promote.borrow_mut() = Some((n, is_non_local));
        }
    }

    /// clear and unprotect locals nodes in this scope; promote the result node to upper scope.
    pub fn flush(&self) {
        let promote: Option<NonNull<GcHead>> = self.promote.borrow_mut().take().map(|(p, _)| p);

        if let Some(p) = promote {
            let (up, _) = self.parent().unwrap();
            // put to upper scope cache
            up.cache.borrow_mut().push(p);
        }

        let lst = std::mem::take(self.cache.borrow_mut().deref_mut());
        for mut n in lst.into_iter().filter(|&p| promote.is_none_or(|x| x != p)) {
            unsafe {
                n.as_mut().remove_flag(crate::node::GcNodeFlag::LOCAL);
            }
        }
    }

    pub fn contains(&self, node: NonNull<GcHead>) -> bool {
        self.cache.borrow().contains(&node)
    }

    /// # Safety
    ///
    /// 仅供 `GcHeap::drop` 在销毁事务栈时调用，用于跳过对
    /// `nodes` 中节点的保护计数更新与根集合维护逻辑。
    /// 调用方必须保证这些节点即将被整体释放，不再通过 GC 访问。
    pub(super) unsafe fn abort(&self) {
        self.promote.borrow_mut().take();

        let nodes = std::mem::take(self.cache.borrow_mut().deref_mut());
        for mut n in nodes {
            unsafe {
                n.as_mut().remove_flag(crate::node::GcNodeFlag::LOCAL);
            }
        }
    }
}

#[derive(Debug)]
pub struct GcScope<'s> {
    heap: NonNull<GcHeap>,
    stack_id: u16,
    index: u8,
    _marker: PhantomData<&'s mut GcHeap>,
}

impl<'s> Drop for GcScope<'s> {
    fn drop(&mut self) {
        unsafe {
            let heap = self.heap.as_mut();
            let stack = &heap.scope_stacks[self.stack_id as usize];
            debug_assert_eq!(stack.scopes.len() as u8 - 1, self.index);
            heap.pop_gc_scope(self.stack_id);
        }
    }
}

impl<'s> std::ops::Deref for GcScope<'s> {
    type Target = GcScopeState<'s>;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.state()
    }
}

impl<'s> std::ops::DerefMut for GcScope<'s> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.state_mut()
    }
}

impl<'s> GcScope<'s> {
    #[inline(always)]
    pub fn stack_id(&self) -> u16 {
        self.stack_id
    }

    #[inline(always)]
    fn state(&self) -> &GcScopeState<'s> {
        unsafe {
            let stack = &self.heap.as_ref().scope_stacks[self.stack_id as usize];
            debug_assert!((self.index as usize) < stack.scopes.len());

            std::mem::transmute::<&GcScopeState<'_>, &GcScopeState<'s>>(
                stack.scopes.get(self.index as usize).unwrap_unchecked(),
            )
        }
    }

    #[inline(always)]
    fn state_mut(&mut self) -> &mut GcScopeState<'s> {
        unsafe {
            let stack = &mut self.heap.as_mut().scope_stacks[self.stack_id as usize];
            debug_assert!((self.index as usize) < stack.scopes.len());

            std::mem::transmute::<&mut GcScopeState<'_>, &mut GcScopeState<'s>>(
                stack.scopes.get_mut(self.index as usize).unwrap_unchecked(),
            )
        }
    }

    #[inline(always)]
    pub fn new(heap: &'s mut GcHeap, stack_id: u16, partition_id: GcPartitionId) -> Self {
        heap.new_scope(stack_id, partition_id)
    }

    pub fn with_new_scope<R>(&self, f: impl FnOnce(GcScope<'_>) -> R) -> R {
        let partition_id = self.partition_id();
        let heap = unsafe { &mut *self.heap.as_ptr() };
        let scope = heap.new_scope(self.stack_id, partition_id);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(scope)));

        match result {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e),
        }
    }
}

impl GcHeap {
    pub fn acquire_scope_stack(&mut self) -> u16 {
        for (id, stack) in self.scope_stacks.iter_mut().enumerate().skip(1) {
            if stack.free {
                debug_assert!(stack.scopes.is_empty());
                stack.free = false;
                return id as u16;
            }
        }

        let id = u16::try_from(self.scope_stacks.len()).expect("too many scope stacks");
        self.scope_stacks.push(crate::heap::ScopeStack {
            scopes: Vec::with_capacity(16),
            free: false,
        });
        id
    }

    pub fn release_scope_stack(&mut self, stack_id: u16) {
        if stack_id == 0 {
            debug_assert!(false, "default scope stack cannot be released");
            return;
        }

        let stack = &mut self.scope_stacks[stack_id as usize];
        debug_assert!(stack.scopes.is_empty());
        stack.free = true;
    }

    #[inline(always)]
    pub fn scope_max_depth(&self, stack_id: u16) -> u8 {
        self.scope_stacks[stack_id as usize].scopes.len() as _
    }

    /// get scope by depth (peek, without modifying stack), `depth` is 1-based, where 1 means index 0
    pub fn scope(&self, stack_id: u16, depth: u8) -> Option<&GcScopeState<'_>> {
        if depth > 0 {
            self.scope_stacks[stack_id as usize]
                .scopes
                .get(depth as usize - 1)
        } else {
            None
        }
    }

    pub(crate) fn push_gc_scope(
        &mut self,
        stack_id: u16,
        partition_id: GcPartitionId,
    ) -> &GcScopeState<'_> {
        let depth = self.scope_max_depth(stack_id) + 1;
        let ctx = GcScopeState {
            heap: NonNull::from_ref(self),
            stack_id,
            partition_id,
            depth: NonZeroU8::new(depth).unwrap(),
            cache: RefCell::new(SmallVec::new()),
            promote: RefCell::new(None),
            _marker: PhantomData,
        };
        // SAFETY: It is safe because the GcHeap owns the GcScope, and we ensure that
        // the GcScope does not outlive the GcHeap.
        let static_ctx =
            unsafe { std::mem::transmute::<GcScopeState<'_>, GcScopeState<'static>>(ctx) };
        let stack = &mut self.scope_stacks[stack_id as usize];
        debug_assert!(!stack.free, "scope stack {stack_id} is not acquired");
        stack.scopes.push(static_ctx);

        stack.scopes.last().unwrap()
    }

    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) fn pop_gc_scope(&mut self, stack_id: u16) -> Option<GcScopeState<'_>> {
        self.scope_stacks[stack_id as usize].scopes.pop()
    }

    #[inline]
    pub fn current_scope(&self, stack_id: u16) -> Option<&GcScopeState<'_>> {
        let s = self.scope_stacks[stack_id as usize].scopes.last();
        // SAFETY: It is safe because the GcHeap owns the GcScope, and we ensure that
        // the GcScope does not outlive the GcHeap.
        unsafe {
            std::mem::transmute::<Option<&GcScopeState<'static>>, Option<&GcScopeState<'_>>>(s)
        }
    }

    #[inline]
    pub fn with_current_scope<R>(
        &mut self,
        stack_id: u16,
        f: impl FnOnce(&mut GcScopeState) -> R,
    ) -> Option<R> {
        self.scope_stacks[stack_id as usize]
            .scopes
            .last_mut()
            .map(f)
    }

    #[inline]
    pub fn new_scope<'s>(&'s mut self, stack_id: u16, partition_id: GcPartitionId) -> GcScope<'s> {
        self.push_gc_scope(stack_id, partition_id);
        let index = self.scope_stacks[stack_id as usize].scopes.len() as u8 - 1;
        GcScope {
            heap: NonNull::from(self),
            stack_id,
            index,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn with_new_scope<R>(
        &mut self,
        stack_id: u16,
        partition_id: GcPartitionId,
        f: impl FnOnce(GcScope<'_>) -> R,
    ) -> R {
        let scope = self.new_scope(stack_id, partition_id);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(scope)));

        match result {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{GcTraceCtx, trace::GcTrace};

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

        let head;

        {
            let ctx = GcScope::new(&mut heap, 0, partition_id);
            let node: GcRef<Node> = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.head_ptr;

            unsafe {
                assert!(head.as_ref().is_local());
            }

            ctx.flush();

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

        let head;
        {
            let mut ctx = GcScope::new(&mut heap, 0, partition_id);
            let node: GcRef<Node> = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.head_ptr;

            unsafe {
                assert!(head.as_ref().is_local());
            }

            while !ctx.heap_mut().mark(partition_id, 64) {}
            let removed = ctx
                .heap_mut()
                .sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
            assert_eq!(removed, 0);
            ctx.flush();
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
            let ctx = GcScope::new(&mut heap, 0, partition_id);
            let added = ctx.add_non_local(head);
            assert!(added);

            unsafe {
                assert!(head.as_ref().is_local());
            }

            ctx.flush();

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

        let ctx = GcScope::new(&mut heap, 0, partition_id);
        let node: GcRef<Node> = ctx
            .alloc_local(Node {
                next: None,
                value: 1,
            })
            .unwrap();

        let head = node.head_ptr;

        unsafe {
            assert!(head.as_ref().is_local());
        }
        let added = ctx.add_non_local(head);
        assert!(!added);

        unsafe {
            assert!(head.as_ref().is_local());
        }

        ctx.flush();

        unsafe {
            assert!(!head.as_ref().is_local());
        }
    }

    #[test]
    fn test_gc_context_alloc_root_creates_root_without_protection() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let head;

        {
            let ctx = GcScope::new(&mut heap, 0, partition_id);
            let node: GcRef<Node> = ctx
                .alloc_root(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.head_ptr;

            unsafe {
                assert!(node.is_root());
                assert!(head.as_ref().is_root());
                assert!(!head.as_ref().is_local());
            }
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert_eq!(removed_after, 0);
    }

    #[test]
    fn test_gc_context_alloc_multiple_nodes() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        let head1;
        let head2;

        {
            let mut ctx = GcScope::new(&mut heap, 0, partition_id);
            let n1: GcRef<Node> = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();
            let n2: GcRef<Node> = ctx
                .alloc_local(Node {
                    next: None,
                    value: 2,
                })
                .unwrap();

            head1 = n1.head_ptr;
            head2 = n2.head_ptr;

            unsafe {
                assert!(head1.as_ref().is_local());
                assert!(head2.as_ref().is_local());
            }

            while !ctx.heap_mut().mark(partition_id, 64) {}
            let removed = ctx
                .heap_mut()
                .sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
            assert_eq!(removed, 0);
            ctx.flush();
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

        let head;

        {
            let mut ctx = GcScope::new(&mut heap, 0, partition_id);
            let node: GcRef<Node> = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.head_ptr;

            unsafe {
                assert!(head.as_ref().is_local());
            }

            ctx.flush();

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

        assert_eq!(heap.scope_max_depth(0), 0);

        heap.push_gc_scope(0, partition_id);
        heap.push_gc_scope(0, partition_id);
        heap.push_gc_scope(0, partition_id);

        assert_eq!(heap.scope_max_depth(0), 3);

        for level in 1..=3 {
            let ctx = heap.scope(0, level).unwrap();
            assert_eq!(ctx.depth(), level);
        }

        heap.pop_gc_scope(0);
        assert_eq!(heap.scope_max_depth(0), 2);
        for level in 1..=2 {
            let ctx = heap.scope(0, level).unwrap();
            assert_eq!(ctx.depth(), level);
        }

        heap.pop_gc_scope(0);
        assert_eq!(heap.scope_max_depth(0), 1);
        let ctx = heap.scope(0, 1).unwrap();
        assert_eq!(ctx.depth(), 1);
    }

    #[test]
    fn test_gc_context_parent_mut_returns_parent_and_level() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        heap.push_gc_scope(0, partition_id);
        heap.push_gc_scope(0, partition_id);
        heap.push_gc_scope(0, partition_id);

        heap.with_current_scope(0, |ctx| {
            assert_eq!(ctx.depth(), 3);
            let (parent, parent_level) = ctx.parent().unwrap();
            assert_eq!(parent_level, 2);
            assert_eq!(parent.depth(), 2);
        });

        heap.pop_gc_scope(0);

        heap.with_current_scope(0, |ctx| {
            assert_eq!(ctx.depth(), 2);
            let (parent, parent_level) = ctx.parent().unwrap();
            assert_eq!(parent_level, 1);
            assert_eq!(parent.depth(), 1);
        });

        heap.pop_gc_scope(0);

        heap.with_current_scope(0, |ctx| {
            assert_eq!(ctx.depth(), 1);
            assert!(ctx.parent().is_none());
        });
    }

    #[test]
    fn test_set_promote_moves_node_to_parent_scope_and_keeps_protection() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        heap.push_gc_scope(0, partition_id);
        heap.push_gc_scope(0, partition_id);

        let promoted_head;
        let other_head;

        {
            let ctx = heap.scope_stacks[0].scopes.last_mut().unwrap();
            let promoted: GcRef<Node> = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();
            let other: GcRef<Node> = ctx
                .alloc_local(Node {
                    next: None,
                    value: 2,
                })
                .unwrap();

            promoted_head = promoted.head_ptr;
            other_head = other.head_ptr;

            unsafe {
                assert!(promoted_head.as_ref().is_local());
                assert!(other_head.as_ref().is_local());
            }

            ctx.set_promote(Some(promoted_head));
        }

        heap.with_current_scope(0, |ctx| ctx.flush());

        unsafe {
            assert!(promoted_head.as_ref().is_local());
            assert!(!other_head.as_ref().is_local());
        }

        {
            let parent_ctx = heap.scope_stacks[0].scopes.first().unwrap();
            assert!(parent_ctx.contains(promoted_head));
        }

        {
            let child_ctx = heap.scope_stacks[0].scopes.last().unwrap();
            assert!(!child_ctx.contains(promoted_head));
        }

        {
            let parent_ctx = heap.scope_stacks[0].scopes.first_mut().unwrap();
            parent_ctx.flush();
        }

        unsafe {
            assert!(!promoted_head.as_ref().is_local());
        }
    }

    #[test]
    fn test_set_promote_in_top_level_scope_is_noop() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

        heap.push_gc_scope(0, partition_id);

        let head;

        {
            let ctx = heap.scope_stacks[0].scopes.last_mut().unwrap();
            let node: GcRef<Node> = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.head_ptr;

            unsafe {
                assert!(head.as_ref().is_local());
            }

            ctx.set_promote(Some(head));
            ctx.flush();
        }

        unsafe {
            assert!(!head.as_ref().is_local());
        }
    }

    #[test]
    fn test_set_promote_reset_on_non_local_restores_state() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();

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

        heap.push_gc_scope(0, partition_id);
        heap.push_gc_scope(0, partition_id);

        {
            let ctx = heap.scope_stacks[0].scopes.last_mut().unwrap();
            ctx.set_promote(Some(head));

            unsafe {
                assert!(head.as_ref().is_local());
            }

            ctx.set_promote(None);
            ctx.flush();
        }

        {
            let parent_ctx = heap.scope_stacks[0].scopes.first().unwrap();
            assert!(!parent_ctx.contains(head));
        }

        unsafe {
            assert!(!head.as_ref().is_local());
        }
    }

    #[test]
    fn test_multi_scope_stacks_are_independent() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();
        let stack1 = heap.acquire_scope_stack();
        let stack2 = heap.acquire_scope_stack();

        assert_ne!(stack1, stack2);
        assert_eq!(heap.scope_max_depth(stack1), 0);
        assert_eq!(heap.scope_max_depth(stack2), 0);

        heap.push_gc_scope(stack1, partition_id);
        heap.push_gc_scope(stack2, partition_id);
        heap.push_gc_scope(stack1, partition_id);

        assert_eq!(heap.scope_max_depth(stack1), 2);
        assert_eq!(heap.scope_max_depth(stack2), 1);
        assert_eq!(heap.current_scope(stack1).unwrap().depth(), 2);
        assert_eq!(heap.current_scope(stack2).unwrap().depth(), 1);

        heap.with_current_scope(stack1, |ctx| {
            let (parent, parent_level) = ctx.parent().unwrap();
            assert_eq!(parent_level, 1);
            assert_eq!(parent.depth(), 1);
            assert_eq!(parent.stack_id(), stack1);
        });

        heap.pop_gc_scope(stack1);
        heap.pop_gc_scope(stack2);
        heap.pop_gc_scope(stack1);

        heap.release_scope_stack(stack1);
        heap.release_scope_stack(stack2);

        let reused = heap.acquire_scope_stack();
        assert_eq!(reused, stack1);
    }

    #[test]
    fn test_gc_scope_drop_is_lifo_per_stack() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition();
        let stack1 = heap.acquire_scope_stack();
        let stack2 = heap.acquire_scope_stack();
        let heap_ptr = &mut heap as *mut GcHeap;

        unsafe {
            let scope1 = (&mut *heap_ptr).new_scope(stack1, partition_id);
            let scope2 = (&mut *heap_ptr).new_scope(stack2, partition_id);

            drop(scope1);
            drop(scope2);
        }

        assert_eq!(heap.scope_max_depth(stack1), 0);
        assert_eq!(heap.scope_max_depth(stack2), 0);
    }
}
