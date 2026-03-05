use std::{num::NonZeroU8, ops::DerefMut};

use {core::ptr::NonNull, std::cell::RefCell, std::marker::PhantomData};

use smallvec::SmallVec;

use crate::{
    heap::GcHeap,
    helpers::GcError,
    node::{GcHead, GcLocal, GcNode, GcRef},
    partition::GcPartitionId,
};

#[derive(Debug)]
pub struct GcScope<'heap> {
    heap: NonNull<GcHeap>,
    partition_id: GcPartitionId,
    depth: NonZeroU8,
    cache: RefCell<SmallVec<[NonNull<GcHead>; 8]>>,
    promote: RefCell<Option<(NonNull<GcHead>, bool)>>,
    _marker: PhantomData<&'heap mut GcHeap>,
}

impl<'heap> Drop for GcScope<'heap> {
    fn drop(&mut self) {
        self.flush();
    }
}

impl<'heap> GcScope<'heap> {
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
    pub fn parent(&self) -> Option<(&GcScope<'_>, u8)> {
        let d = self.depth();
        if d > 1 {
            let parent_index = d - 2;

            self.heap().scope_stack.get(parent_index as usize).map(|s| {
                (
                    unsafe { std::mem::transmute::<&GcScope<'static>, &GcScope<'_>>(s) },
                    d - 1,
                )
            })
        } else {
            None
        }
    }

    pub fn alloc<T: GcNode>(&self, payload: T) -> Result<GcRef<T>, (GcError, T)> {
        unsafe {
            let r = (*self.heap.as_ptr()).alloc_raw(self.partition_id, payload)?;
            let mut head = r.head_ptr;

            #[cfg(debug_assertions)]
            {
                let h = head.as_ref();
                debug_assert!(
                    !h.contains_flag(crate::node::GcNodeFlag::LOCAL),
                    "node already in GcScope: {h:p}"
                );
            }

            head.as_mut().insert_flag(crate::node::GcNodeFlag::LOCAL);

            (*self.heap.as_ptr()).do_protect_node(head);
            self.cache.borrow_mut().push(head);

            Ok(r)
        }
    }

    pub fn alloc_root<T: GcNode>(&self, payload: T) -> Result<GcRef<T>, (GcError, T)> {
        unsafe { (*self.heap.as_ptr()).alloc_root_raw(self.partition_id, payload) }
    }

    #[deprecated]
    pub fn alloc_local<T: GcNode>(&self, payload: T) -> Result<GcLocal<T>, (GcError, T)> {
        //  unsafe { (*self.heap).alloc_local_raw(self.partition_id, payload) }
        let r = self.alloc(payload)?;
        Ok(GcLocal::new(unsafe { &mut *self.heap.as_ptr() }, r))
    }

    // if node is not local, then add it to `self` scope
    pub fn add_non_local(&self, mut node: NonNull<GcHead>) -> bool {
        unsafe {
            if node.as_ref().is_local() {
                return false;
            }

            #[cfg(debug_assertions)]
            {
                node.as_mut().dbg_scope_level = 0;
            }

            node.as_mut().insert_flag(crate::node::GcNodeFlag::LOCAL);
            (*self.heap.as_ptr()).do_protect_node(node);
        }

        self.cache.borrow_mut().push(node);
        true
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

        // check current promote value
        let prev = self.promote.borrow_mut().take();

        if let Some((mut p, was_non_scoped)) = prev
            && was_non_scoped
        {
            // avoid leak protect
            unsafe {
                p.as_mut().remove_flag(crate::node::GcNodeFlag::LOCAL);
                (*self.heap.as_ptr()).do_unprotect_node(p);
            }
        }

        if self.depth() == 1 {
            return; // has no upper scope, don't promote
        }

        if let Some(mut n) = node {
            let h = unsafe { n.as_ref() };

            if h.is_root() {
                return; // root node don't promote
            }

            let was_non_scope = if h.is_local() {
                if !self
                    .cache
                    .borrow()
                    .iter()
                    .any(|p| std::ptr::eq(n.as_ptr(), p.as_ptr()))
                {
                    // node is in other scope, don't promote
                    return;
                }
                false
            } else {
                // node is neither root nor local, protect it first
                unsafe {
                    n.as_mut().insert_flag(crate::node::GcNodeFlag::LOCAL);
                    (*self.heap.as_ptr()).do_protect_node(n);
                }
                true
            };

            *self.promote.borrow_mut() = Some((n, was_non_scope));
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
                p.as_mut().dbg_scope_level = _lev as _;
            }
        }

        // unprotect non-promoted nodes
        let lst = std::mem::take(self.cache.borrow_mut().deref_mut());
        for mut n in lst.into_iter().filter(|&p| promote.is_none_or(|x| x != p)) {
            unsafe {
                n.as_mut().remove_flag(crate::node::GcNodeFlag::LOCAL);
                (*self.heap.as_ptr()).do_unprotect_node(n);

                #[cfg(debug_assertions)]
                {
                    n.as_mut().dbg_scope_level = 0;
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
                    n.as_mut().dbg_scope_level = idx as _;
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

impl GcHeap {
    #[inline(always)]
    pub fn scope_max_depth(&self) -> u8 {
        self.scope_stack.len() as _
    }

    /// get scope by depth
    pub fn scope(&self, depth: u8) -> Option<&GcScope<'_>> {
        if depth > 0 {
            self.scope_stack.get(depth as usize - 1)
        } else {
            None
        }
    }

    pub fn push_gc_scope(&mut self, partition_id: GcPartitionId) -> &GcScope<'_> {
        let ctx = GcScope::new(self, partition_id);
        // SAFETY: It is safe because the GcHeap owns the GcScope, and we ensure that
        // the GcScope does not outlive the GcHeap.
        let static_ctx = unsafe { std::mem::transmute::<GcScope<'_>, GcScope<'static>>(ctx) };
        self.scope_stack.push(static_ctx);

        self.scope_stack.last().unwrap()
    }

    #[inline(always)]
    pub fn pop_gc_scope(&mut self) -> Option<GcScope<'_>> {
        self.scope_stack.pop()
    }

    #[inline]
    pub fn current_scope(&self) -> Option<&GcScope<'_>> {
        let s = self.scope_stack.last();
        // SAFETY: It is safe because the GcHeap owns the GcScope, and we ensure that
        // the GcScope does not outlive the GcHeap.
        unsafe { std::mem::transmute::<Option<&GcScope<'static>>, Option<&GcScope<'_>>>(s) }
    }

    #[inline]
    pub fn with_current_scope<R>(&mut self, f: impl FnOnce(&mut GcScope) -> R) -> Option<R> {
        self.scope_stack.last_mut().map(f)
    }

    pub fn with_new_scope<R>(
        &mut self,
        partition_id: GcPartitionId,
        f: impl FnOnce(&GcScope<'_>) -> R,
    ) -> R {
        self.push_gc_scope(partition_id);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f(self.scope_stack.last_mut().unwrap())
        }));
        self.pop_gc_scope();

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
                .alloc(Node {
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
                .alloc(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.head_ptr;

            unsafe {
                assert!(head.as_ref().is_local());
                assert_eq!(head.as_ref().protect_count(), 1);
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
            assert_eq!(head.as_ref().protect_count(), 0);
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
            assert_eq!(head.as_ref().protect_count(), 0);
        }

        {
            let mut ctx = GcScope::new(&mut heap, partition_id);
            let added = ctx.add_non_local(head);
            assert!(added);

            unsafe {
                assert!(head.as_ref().is_local());
                assert_eq!(head.as_ref().protect_count(), 1);
            }

            ctx.flush();

            unsafe {
                assert!(!head.as_ref().is_local());
                assert_eq!(head.as_ref().protect_count(), 0);
            }
        }

        unsafe {
            assert!(!head.as_ref().is_local());
            assert_eq!(head.as_ref().protect_count(), 0);
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
            .alloc(Node {
                next: None,
                value: 1,
            })
            .unwrap();

        let head = node.head_ptr;

        unsafe {
            assert!(head.as_ref().is_local());
            assert_eq!(head.as_ref().protect_count(), 1);
        }

        let added = ctx.add_non_local(head);
        assert!(!added);

        unsafe {
            assert!(head.as_ref().is_local());
            assert_eq!(head.as_ref().protect_count(), 1);
        }

        ctx.flush();

        unsafe {
            assert!(!head.as_ref().is_local());
            assert_eq!(head.as_ref().protect_count(), 0);
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
                assert_eq!(head.as_ref().protect_count(), 0);
            }
        }

        assert!(heap.get_roots(partition_id).any(|n| n == head));

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
                .alloc(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();
            let n2: GcRef<Node> = ctx
                .alloc(Node {
                    next: None,
                    value: 2,
                })
                .unwrap();

            head1 = n1.head_ptr;
            head2 = n2.head_ptr;

            unsafe {
                assert_eq!(head1.as_ref().protect_count(), 1);
                assert_eq!(head2.as_ref().protect_count(), 1);
            }

            while !ctx.heap_mut().mark(partition_id, 64) {}
            let removed = ctx
                .heap_mut()
                .sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
            assert_eq!(removed, 0);
            ctx.flush();
        }

        unsafe {
            assert_eq!(head1.as_ref().protect_count(), 0);
            assert_eq!(head2.as_ref().protect_count(), 0);
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after >= 2);
    }

    #[test]
    fn test_gc_context_alloc_local() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let head;

        {
            let mut ctx = GcScope::new(&mut heap, partition_id);
            let local: GcLocal<Node> = ctx
                .alloc_local(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = local.get().head_ptr;

            unsafe {
                assert_eq!(head.as_ref().protect_count(), 2);
            }

            drop(local);

            unsafe {
                assert_eq!(head.as_ref().protect_count(), 1);
            }
        }

        unsafe {
            assert_eq!(head.as_ref().protect_count(), 0);
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after > 0);
    }

    #[test]
    fn test_gc_context_reset_unprotects_and_clears_cache() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let head;

        {
            let mut ctx = GcScope::new(&mut heap, partition_id);
            let node: GcRef<Node> = ctx
                .alloc(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.head_ptr;

            unsafe {
                assert_eq!(head.as_ref().protect_count(), 1);
            }

            ctx.flush();

            unsafe {
                assert_eq!(head.as_ref().protect_count(), 0);
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
                .alloc(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.head_ptr;

            unsafe {
                assert!(head.as_ref().is_local());
                assert_eq!(head.as_ref().protect_count(), 1);
            }

            let promoted = ctx.promote(head);
            assert!(promoted);

            unsafe {
                assert!(heap.scope_stack[0].contains(head));
                assert!(head.as_ref().is_local());
                assert_eq!(head.as_ref().protect_count(), 1);
            }
        }

        {
            let ctx = heap.pop_gc_scope().unwrap();
            drop(ctx);
        }

        unsafe {
            assert_eq!(head.as_ref().protect_count(), 1);
        }

        {
            let ctx = heap.pop_gc_scope().unwrap();
            drop(ctx);
        }

        unsafe {
            assert!(!head.as_ref().is_local());
            assert_eq!(head.as_ref().protect_count(), 0);
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
                .alloc(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();
            let other: GcRef<Node> = ctx
                .alloc(Node {
                    next: None,
                    value: 2,
                })
                .unwrap();

            promoted_head = promoted.head_ptr;
            other_head = other.head_ptr;

            unsafe {
                assert!(promoted_head.as_ref().is_local());
                assert!(other_head.as_ref().is_local());
                assert_eq!(promoted_head.as_ref().protect_count(), 1);
                assert_eq!(other_head.as_ref().protect_count(), 1);
            }

            ctx.set_promote(Some(promoted_head));
        }

        heap.with_current_scope(|ctx| ctx.flush());

        unsafe {
            assert!(promoted_head.as_ref().is_local());
            assert_eq!(promoted_head.as_ref().protect_count(), 1);
            assert!(!other_head.as_ref().is_local());
            assert_eq!(other_head.as_ref().protect_count(), 0);
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
            assert_eq!(promoted_head.as_ref().protect_count(), 0);
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
                .alloc(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.head_ptr;

            unsafe {
                assert_eq!(head.as_ref().protect_count(), 1);
            }

            let promoted = ctx.promote(head);
            assert!(!promoted);

            unsafe {
                assert_eq!(head.as_ref().protect_count(), 1);
            }
        }

        {
            let ctx = heap.pop_gc_scope().unwrap();
            drop(ctx);
        }

        unsafe {
            assert_eq!(head.as_ref().protect_count(), 0);
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
                .alloc(Node {
                    next: None,
                    value: 1,
                })
                .unwrap();

            head = node.head_ptr;

            unsafe {
                assert!(head.as_ref().is_local());
                assert_eq!(head.as_ref().protect_count(), 1);
            }

            ctx.set_promote(Some(head));
            ctx.flush();
        }

        unsafe {
            assert!(!head.as_ref().is_local());
            assert_eq!(head.as_ref().protect_count(), 0);
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
            assert_eq!(head.as_ref().protect_count(), 0);
        }

        heap.push_gc_scope(partition_id);
        heap.push_gc_scope(partition_id);

        {
            let ctx = heap.scope_stack.last_mut().unwrap();
            ctx.set_promote(Some(head));

            unsafe {
                assert!(head.as_ref().is_local());
                assert_eq!(head.as_ref().protect_count(), 1);
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
            assert_eq!(head.as_ref().protect_count(), 0);
        }
    }
}
