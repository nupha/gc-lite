use std::ops::DerefMut;

use {core::ptr::NonNull, std::cell::RefCell, std::marker::PhantomData};

use smallvec::SmallVec;

use crate::{
    heap::GcHeap,
    helpers::GcError,
    node::{GcHead, GcLocal, GcNode, GcRef},
    partition::GcPartitionId,
};

#[derive(Debug)]
pub struct GcContext<'heap> {
    heap: *mut GcHeap,
    partition_id: GcPartitionId,
    cache: RefCell<SmallVec<[NonNull<GcHead>; 8]>>,
    _marker: PhantomData<&'heap mut GcHeap>,
}

impl<'heap> Drop for GcContext<'heap> {
    fn drop(&mut self) {
        self.commit();
    }
}

impl<'heap> GcContext<'heap> {
    pub fn new(heap: &'heap mut GcHeap, partition_id: GcPartitionId) -> Self {
        debug_assert!(!partition_id.is_null());
        Self {
            heap: heap as *mut _,
            partition_id,
            cache: RefCell::new(SmallVec::new()),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub fn partition_id(&self) -> GcPartitionId {
        self.partition_id
    }

    #[inline(always)]
    pub fn heap(&self) -> &GcHeap {
        unsafe { &*self.heap }
    }

    #[inline(always)]
    pub fn heap_mut(&mut self) -> &mut GcHeap {
        unsafe { &mut *self.heap }
    }

    /// Allocate a node, and put it to scope cache to protect it.
    pub fn alloc<T: GcNode>(&mut self, payload: T) -> Result<GcRef<T>, (GcError, T)> {
        unsafe {
            let r = (*self.heap).alloc_raw(self.partition_id, payload)?;
            let head = r.head_ptr;
            (*self.heap).do_protect_node(head);
            self.cache.borrow_mut().push(head);
            Ok(r)
        }
    }

    pub fn alloc_root<T: GcNode>(&mut self, payload: T) -> Result<GcRef<T>, (GcError, T)> {
        unsafe { (*self.heap).alloc_root_raw(self.partition_id, payload) }
    }

    #[deprecated]
    pub fn alloc_local<T: GcNode>(&mut self, payload: T) -> Result<GcLocal<T>, (GcError, T)> {
        //  unsafe { (*self.heap).alloc_local_raw(self.partition_id, payload) }
        let r = self.alloc(payload)?;
        Ok(GcLocal::new(unsafe { &mut *self.heap }, r))
    }

    /// confirm and clear current cached nodes, restart to cache new nodes.
    /// this behaves like to drop current scope, and start a new scope.
    pub fn commit(&mut self) {
        let nodes = std::mem::take(self.cache.borrow_mut().deref_mut());
        for n in nodes {
            #[cfg(debug_assertions)]
            unsafe {
                let mut n = n;
                n.as_mut().dbg_scope_level = 0;
            }

            self.heap_mut().do_unprotect_node(n);
        }
    }

    /// promote node
    pub fn promote(&self, node: NonNull<GcHead>) -> bool {
        let heap = unsafe { &mut *self.heap };
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
        // #[cfg(debug_assertions)]
        // for n in self.cache.iter() {
        //     println!("[O.o]    abort scope node: {:?}", n);
        // }

        self.cache.borrow_mut().clear();
    }
}

impl GcHeap {
    pub fn scope_level(&self) -> usize {
        self.scope_stack.len()
    }

    pub fn push_gc_scope(&mut self, partition_id: GcPartitionId) {
        let ctx = GcContext::new(self, partition_id);
        // SAFETY: It is safe because the GcHeap owns the GcContext, and we ensure that
        // the GcContext does not outlive the GcHeap.
        let static_ctx = unsafe { std::mem::transmute::<GcContext<'_>, GcContext<'static>>(ctx) };
        self.scope_stack.push(static_ctx);
    }

    pub fn pop_gc_scope(&mut self) -> Option<GcContext<'_>> {
        self.scope_stack.pop()
    }

    pub fn current_gc_scope(&self) -> Option<&GcContext<'_>> {
        let s = self.scope_stack.last();
        // SAFETY: It is safe because the GcHeap owns the GcContext, and we ensure that
        // the GcContext does not outlive the GcHeap.
        unsafe { std::mem::transmute::<Option<&GcContext<'static>>, Option<&GcContext<'_>>>(s) }
    }

    pub fn with_current_scope<R>(&mut self, f: impl FnOnce(&mut GcContext) -> R) -> Option<R> {
        self.scope_stack.last_mut().map(f)
    }

    pub fn with_new_scope<R>(
        &mut self,
        partition_id: GcPartitionId,
        f: impl FnOnce(&mut GcContext<'_>) -> R,
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
    fn test_gc_context_alloc_protects_and_unprotects_on_drop() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let head;
        {
            let mut ctx = GcContext::new(&mut heap, partition_id);
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

            while !ctx.heap_mut().mark(partition_id, 64) {}
            let removed = ctx
                .heap_mut()
                .sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
            assert_eq!(removed, 0);
            ctx.commit();
        }

        unsafe {
            assert_eq!(head.as_ref().protect_count(), 0);
        }

        while !heap.mark(partition_id, 64) {}
        let removed_after = heap.sweep(partition_id, GcHeap::DUMMY_DISPOSE_CALLBACK);
        assert!(removed_after > 0);
    }

    #[test]
    fn test_gc_context_alloc_root_creates_root_without_protection() {
        let mut heap = GcHeap::new(&GC_TYPE_REGISTRY);
        let partition_id = heap.create_partition(4096);

        let head;

        {
            let mut ctx = GcContext::new(&mut heap, partition_id);
            let node: GcRef<Node> = unsafe {
                ctx.alloc_root(Node {
                    next: None,
                    value: 1,
                })
                .unwrap()
            };

            head = node.head_ptr;

            unsafe {
                assert!(node.is_root());
                assert!(head.as_ref().is_root());
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
            let mut ctx = GcContext::new(&mut heap, partition_id);
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
            ctx.commit();
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
            let mut ctx = GcContext::new(&mut heap, partition_id);
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
            let mut ctx = GcContext::new(&mut heap, partition_id);
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

            ctx.commit();

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
                assert_eq!(head.as_ref().protect_count(), 1);
            }

            let promoted = ctx.promote(head);
            assert!(promoted);

            unsafe {
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
            assert_eq!(head.as_ref().protect_count(), 0);
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
}
