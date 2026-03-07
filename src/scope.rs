use std::{num::NonZeroU8, ops::DerefMut};

use {core::ptr::NonNull, std::cell::RefCell, std::marker::PhantomData};

use smallvec::SmallVec;

use crate::{
    heap::GcHeap,
    helpers::GcError,
    node::{GcHead, GcNode, GcRef},
    partition::GcPartitionId,
};

#[derive(Debug)]
pub struct GcScopeState<'heap> {
    heap: NonNull<GcHeap>,
    partition_id: GcPartitionId,
    depth: NonZeroU8,
    cache: RefCell<SmallVec<[NonNull<GcHead>; 8]>>,
    promote: RefCell<Option<(NonNull<GcHead>, bool)>>,
    _marker: PhantomData<&'heap mut GcHeap>,
}

impl<'heap> Drop for GcScopeState<'heap> {
    fn drop(&mut self) {
        self.flush();
    }
}

impl<'heap> GcScopeState<'heap> {
    pub fn new(heap: &'heap mut GcHeap, partition_id: GcPartitionId) -> Self {
        debug_assert!(!partition_id.is_null());
        let depth = heap.scope_stack.len() + 1;

        Self {
            heap: NonNull::from_ref(heap),
            partition_id,
            depth: NonZeroU8::new(depth as u8).unwrap(),
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

            self.heap().scope_stack.get(parent_index as usize).map(|s| {
                (
                    unsafe { std::mem::transmute::<&GcScopeState<'static>, &GcScopeState<'_>>(s) },
                    d - 1,
                )
            })
        } else {
            None
        }
    }

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
            node.as_mut().dbg_scope_depth = self.depth();
        }

        unsafe {
            node.as_mut().insert_flag(crate::node::GcNodeFlag::LOCAL);

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

    pub fn get_promote(&self) -> Option<NonNull<GcHead>> {
        self.promote.borrow().map(|(p, _)| p)
    }

    /// Set a node to be promoted.
    ///
    /// Promote means when the scope is dropped, the node will be added to upper scope.
    pub fn set_promote(&self, node: Option<NonNull<GcHead>>) {
        debug_assert_eq!(
            self.depth(),
            self.heap().scope_max_depth(),
            "only inner-most scope can set_promote"
        );

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

    /// clear and unprotect cached nodes.
    /// this behaves like to drop current scope, and start a new scope.
    pub fn flush(&self) {
        let promote: Option<NonNull<GcHead>> = self.promote.borrow_mut().take().map(|(p, _)| p);

        if let Some(mut p) = promote {
            let (up, _lev) = self.parent().unwrap();
            // put to upper scope cache
            up.cache.borrow_mut().push(p);

            #[cfg(debug_assertions)]
            unsafe {
                p.as_mut().dbg_scope_depth = _lev as _;
            }
        }

        let lst = std::mem::take(self.cache.borrow_mut().deref_mut());
        for mut n in lst.into_iter().filter(|&p| promote.is_none_or(|x| x != p)) {
            unsafe {
                n.as_mut().remove_flag(crate::node::GcNodeFlag::LOCAL);

                #[cfg(debug_assertions)]
                {
                    n.as_mut().dbg_scope_depth = 0;
                }
            }
        }
    }

    /// promote node
    #[deprecated]
    pub fn promote(&self, node: NonNull<GcHead>) -> bool {
        let heap = unsafe { &mut *self.heap.as_ptr() };
        let self_ptr = self as *const Self as *mut ();

        if heap.scope_stack.len() > 1
            && let Some((idx, _)) = heap
                .scope_stack
                .iter()
                .enumerate()
                .rev()
                .find(|(_, c)| *c as *const Self as *const () == self_ptr)
            && idx != 0
        {
            if let Some(i) = { self.cache.borrow().iter().position(|&h| h == node) } {
                self.cache.borrow_mut().swap_remove(i);

                #[cfg(debug_assertions)]
                unsafe {
                    let mut n = node;
                    n.as_mut().dbg_scope_depth = idx as _;
                }

                heap.scope_stack[idx - 1].cache.borrow_mut().push(node);
                true
            } else {
                false
            }
        } else {
            false
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

pub struct GcScope<'heap> {
    heap: NonNull<GcHeap>,
    index: u8,
    _marker: PhantomData<&'heap mut GcHeap>,
}

impl<'heap> GcScope<'heap> {
    fn heap_raw(&self) -> &GcHeap {
        unsafe { self.heap.as_ref() }
    }

    fn heap_raw_mut(&mut self) -> &mut GcHeap {
        unsafe { self.heap.as_mut() }
    }

    fn inner_static(&self) -> &GcScopeState<'static> {
        let heap = self.heap_raw();
        &heap.scope_stack[self.index as usize]
    }

    fn inner_static_mut(&mut self) -> &mut GcScopeState<'static> {
        let index = self.index as usize;
        let heap = self.heap_raw_mut();
        &mut heap.scope_stack[index]
    }

    fn inner(&self) -> &GcScopeState<'heap> {
        unsafe {
            std::mem::transmute::<&GcScopeState<'static>, &GcScopeState<'heap>>(self.inner_static())
        }
    }

    fn inner_mut(&mut self) -> &mut GcScopeState<'heap> {
        unsafe {
            std::mem::transmute::<&mut GcScopeState<'static>, &mut GcScopeState<'heap>>(
                self.inner_static_mut(),
            )
        }
    }

    pub fn new(heap: &'heap mut GcHeap, partition_id: GcPartitionId) -> Self {
        heap.new_scope(partition_id)
    }

    pub fn new_scope<'child>(&'child mut self) -> GcScope<'child>
    where
        'heap: 'child,
    {
        let partition_id = self.partition_id();
        self.heap_raw_mut().new_scope(partition_id)
    }

    pub fn with_new_scope<R>(&self, f: impl FnOnce(GcScope<'_>) -> R) -> R {
        let partition_id = self.partition_id();
        let heap = unsafe { &mut *self.heap.as_ptr() };
        let scope = heap.new_scope(partition_id);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(scope)));

        match result {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e),
        }
    }

    #[inline(always)]
    pub fn partition_id(&self) -> GcPartitionId {
        self.inner().partition_id()
    }

    #[inline(always)]
    pub fn heap(&self) -> &GcHeap {
        self.inner().heap()
    }

    #[inline(always)]
    pub fn heap_mut(&mut self) -> &mut GcHeap {
        self.inner_mut().heap_mut()
    }

    #[inline(always)]
    pub fn depth(&self) -> u8 {
        self.inner().depth()
    }

    #[inline(always)]
    pub fn count(&self) -> usize {
        self.inner().count()
    }

    pub fn alloc_root<T: GcNode>(&self, payload: T) -> Result<GcRef<T>, (GcError, T)> {
        self.inner().alloc_root(payload)
    }

    pub fn alloc_local<T: GcNode>(&self, payload: T) -> Result<GcRef<T>, (GcError, T)> {
        self.inner().alloc_local(payload)
    }

    pub fn add_non_local(&self, node: NonNull<GcHead>) -> bool {
        self.inner().add_non_local(node)
    }

    pub fn get_promote(&self) -> Option<NonNull<GcHead>> {
        self.inner().get_promote()
    }

    pub fn set_promote(&self, node: Option<NonNull<GcHead>>) {
        self.inner().set_promote(node)
    }

    pub fn flush(&self) {
        self.inner().flush()
    }

    #[deprecated]
    pub fn promote(&self, node: NonNull<GcHead>) -> bool {
        self.inner().promote(node)
    }

    pub fn contains(&self, node: NonNull<GcHead>) -> bool {
        self.inner().contains(node)
    }
}

impl<'heap> std::ops::Deref for GcScope<'heap> {
    type Target = GcScopeState<'heap>;

    fn deref(&self) -> &Self::Target {
        self.inner()
    }
}

impl<'heap> std::ops::DerefMut for GcScope<'heap> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner_mut()
    }
}

impl<'heap> Drop for GcScope<'heap> {
    fn drop(&mut self) {
        unsafe {
            let heap = self.heap.as_mut();
            debug_assert_eq!(heap.scope_stack.len() as u8 - 1, self.index);
            if let Some(ctx) = heap.pop_gc_scope() {
                drop(ctx);
            }
        }
    }
}

impl GcHeap {
    #[inline(always)]
    pub fn scope_max_depth(&self) -> u8 {
        self.scope_stack.len() as _
    }

    /// get scope by depth (peek, without modifying stack)
    pub fn scope(&self, depth: u8) -> Option<&GcScopeState<'_>> {
        if depth == 0 {
            return None;
        }

        self.scope_stack.get(depth as usize - 1)
    }

    pub(crate) fn push_gc_scope(&mut self, partition_id: GcPartitionId) -> &GcScopeState<'_> {
        let ctx = GcScopeState::new(self, partition_id);
        // SAFETY: It is safe because the GcHeap owns the GcScope, and we ensure that
        // the GcScope does not outlive the GcHeap.
        let static_ctx =
            unsafe { std::mem::transmute::<GcScopeState<'_>, GcScopeState<'static>>(ctx) };
        self.scope_stack.push(static_ctx);

        self.scope_stack.last().unwrap()
    }

    #[inline(always)]
    pub(crate) fn pop_gc_scope(&mut self) -> Option<GcScopeState<'_>> {
        self.scope_stack.pop()
    }

    #[inline]
    pub fn current_scope(&self) -> Option<&GcScopeState<'_>> {
        let s = self.scope_stack.last();
        // SAFETY: It is safe because the GcHeap owns the GcScope, and we ensure that
        // the GcScope does not outlive the GcHeap.
        unsafe {
            std::mem::transmute::<Option<&GcScopeState<'static>>, Option<&GcScopeState<'_>>>(s)
        }
    }

    #[inline]
    pub fn with_current_scope<R>(&mut self, f: impl FnOnce(&mut GcScopeState) -> R) -> Option<R> {
        self.scope_stack.last_mut().map(f)
    }

    pub fn new_scope<'s>(&'s mut self, partition_id: GcPartitionId) -> GcScope<'s> {
        self.push_gc_scope(partition_id);
        let index = self.scope_stack.len() as u8 - 1;
        let heap_ptr = NonNull::from(self);
        GcScope {
            heap: heap_ptr,
            index,
            _marker: PhantomData,
        }
    }

    pub fn with_new_scope<R>(
        &mut self,
        partition_id: GcPartitionId,
        f: impl FnOnce(GcScope<'_>) -> R,
    ) -> R {
        let scope = self.new_scope(partition_id);
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
        let partition_id = heap.create_partition(4096);

        let head;

        {
            let ctx = GcScope::new(&mut heap, partition_id);
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
        let partition_id = heap.create_partition(4096);

        let head;
        {
            let mut ctx = GcScope::new(&mut heap, partition_id);
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
        let partition_id = heap.create_partition(4096);

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
            let mut ctx = GcScope::new(&mut heap, partition_id);
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
        let partition_id = heap.create_partition(4096);

        let ctx = GcScope::new(&mut heap, partition_id);
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
        let partition_id = heap.create_partition(4096);

        let head;

        {
            let ctx = GcScope::new(&mut heap, partition_id);
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
        let partition_id = heap.create_partition(4096);

        let head1;
        let head2;

        {
            let mut ctx = GcScope::new(&mut heap, partition_id);
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
        let partition_id = heap.create_partition(4096);

        let head;

        {
            let mut ctx = GcScope::new(&mut heap, partition_id);
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
        let partition_id = heap.create_partition(4096);

        assert_eq!(heap.scope_max_depth(), 0);

        heap.push_gc_scope(partition_id);
        heap.push_gc_scope(partition_id);
        heap.push_gc_scope(partition_id);

        assert_eq!(heap.scope_max_depth(), 3);

        for level in 1..=3 {
            let ctx = heap.scope(level).unwrap();
            assert_eq!(ctx.depth(), level);
        }

        heap.pop_gc_scope();
        assert_eq!(heap.scope_max_depth(), 2);
        for level in 1..=2 {
            let ctx = heap.scope(level).unwrap();
            assert_eq!(ctx.depth(), level);
        }

        heap.pop_gc_scope();
        assert_eq!(heap.scope_max_depth(), 1);
        let ctx = heap.scope(1).unwrap();
        assert_eq!(ctx.depth(), 1);
    }

    #[test]
    fn test_gc_context_parent_mut_returns_parent_and_level() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        heap.push_gc_scope(partition_id);
        heap.push_gc_scope(partition_id);
        heap.push_gc_scope(partition_id);

        heap.with_current_scope(|ctx| {
            assert_eq!(ctx.depth(), 3);
            let (parent, parent_level) = ctx.parent().unwrap();
            assert_eq!(parent_level, 2);
            assert_eq!(parent.depth(), 2);
        });

        heap.pop_gc_scope();

        heap.with_current_scope(|ctx| {
            assert_eq!(ctx.depth(), 2);
            let (parent, parent_level) = ctx.parent().unwrap();
            assert_eq!(parent_level, 1);
            assert_eq!(parent.depth(), 1);
        });

        heap.pop_gc_scope();

        heap.with_current_scope(|ctx| {
            assert_eq!(ctx.depth(), 1);
            assert!(ctx.parent().is_none());
        });
    }

    #[test]
    fn test_scope_promote_moves_node_to_parent_scope() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        heap.push_gc_scope(partition_id);
        heap.push_gc_scope(partition_id);

        let head;

        {
            let ctx = heap.scope_stack.last_mut().unwrap();
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

            let promoted = ctx.promote(head);
            assert!(promoted);

            unsafe {
                assert!(heap.scope_stack[0].contains(head));
                assert!(head.as_ref().is_local());
            }
        }

        {
            let ctx = heap.pop_gc_scope().unwrap();
            drop(ctx);
        }

        unsafe {
            assert!(head.as_ref().is_local());
        }

        {
            let ctx = heap.pop_gc_scope().unwrap();
            drop(ctx);
        }

        unsafe {
            assert!(!head.as_ref().is_local());
        }
    }

    #[test]
    fn test_set_promote_moves_node_to_parent_scope_and_keeps_protection() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        heap.push_gc_scope(partition_id);
        heap.push_gc_scope(partition_id);

        let promoted_head;
        let other_head;

        {
            let ctx = heap.scope_stack.last_mut().unwrap();
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

        heap.with_current_scope(|ctx| ctx.flush());

        unsafe {
            assert!(promoted_head.as_ref().is_local());
            assert!(!other_head.as_ref().is_local());
        }

        {
            let parent_ctx = heap.scope_stack.first().unwrap();
            assert!(parent_ctx.contains(promoted_head));
        }

        {
            let child_ctx = heap.scope_stack.last().unwrap();
            assert!(!child_ctx.contains(promoted_head));
        }

        {
            let parent_ctx = heap.scope_stack.first_mut().unwrap();
            parent_ctx.flush();
        }

        unsafe {
            assert!(!promoted_head.as_ref().is_local());
        }
    }

    #[test]
    fn test_scope_promote_in_top_level_scope_returns_false() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        heap.push_gc_scope(partition_id);

        let head;

        {
            let ctx = heap.scope_stack.last_mut().unwrap();
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

            let promoted = ctx.promote(head);
            assert!(!promoted);

            unsafe {
                assert!(head.as_ref().is_local());
            }
        }

        {
            let ctx = heap.pop_gc_scope().unwrap();
            drop(ctx);
        }

        unsafe {
            assert!(!head.as_ref().is_local());
        }
    }

    #[test]
    fn test_set_promote_in_top_level_scope_is_noop() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        heap.push_gc_scope(partition_id);

        let head;

        {
            let ctx = heap.scope_stack.last_mut().unwrap();
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
        let partition_id = heap.create_partition(4096);

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

        heap.push_gc_scope(partition_id);
        heap.push_gc_scope(partition_id);

        {
            let ctx = heap.scope_stack.last_mut().unwrap();
            ctx.set_promote(Some(head));

            unsafe {
                assert!(head.as_ref().is_local());
            }

            ctx.set_promote(None);
            ctx.flush();
        }

        {
            let parent_ctx = heap.scope_stack.first().unwrap();
            assert!(!parent_ctx.contains(head));
        }

        unsafe {
            assert!(!head.as_ref().is_local());
        }
    }
}
