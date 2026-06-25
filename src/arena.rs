// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright (c) 2025-2026 John Ray <996351336@qq.com>

use core::{alloc::Layout, cell::Cell, ptr::NonNull};

/// Minimum free slot size (16 bytes = two `usize` values)
const MIN_ALLOC_SIZE: usize = 16;

/// Arena capacity (64 KB)
pub const ARENA_CAPACITY: usize = 64 * 1024;
/// Allocations larger than this skip the arena and go directly to system malloc
pub const MAX_ARENA_ALLOC: usize = 16 * 1024;

/// A free hole before sorting & merging during sweep
pub(crate) struct Hole {
    pub offset: usize,
    pub size: usize,
}

/// Bump-pointer arena + free-list hybrid allocator
#[derive(Debug)]
pub struct GcArena {
    base: NonNull<u8>,
    capacity: usize,
    /// Current bump frontier offset
    bump: Cell<usize>,
    /// Head of the free-list (reclaimed dead node space)
    free_head: Cell<Option<NonNull<u8>>>,
    /// Number of live nodes currently in this arena
    live_count: Cell<usize>,
}

impl GcArena {
    /// Create a new arena, allocating `capacity` bytes from the system allocator.
    pub fn new(capacity: usize) -> Option<Self> {
        let layout = Layout::from_size_align(capacity, 16).ok()?;
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            return None;
        }
        Some(Self {
            base: NonNull::new(ptr)?,
            capacity,
            bump: Cell::new(0),
            free_head: Cell::new(None),
            live_count: Cell::new(0),
        })
    }

    /// Main allocation path: free-list → bump → `None`.
    pub fn alloc(&self, layout: Layout) -> Option<NonNull<u8>> {
        let needed = layout.size().max(MIN_ALLOC_SIZE);
        // 1. Try free-list first
        if let Some(ptr) = self.alloc_from_free_list(needed) {
            self.live_count.set(self.live_count.get() + 1);
            return Some(ptr);
        }
        // 2. Bump allocation
        let align = layout.align();
        let offset = (self.bump.get() + align - 1) & !(align - 1);
        if offset + needed > self.capacity {
            return None; // arena full
        }
        self.bump.set(offset + needed);
        self.live_count.set(self.live_count.get() + 1);
        let ptr = unsafe { self.base.as_ptr().add(offset) };
        Some(unsafe { NonNull::new_unchecked(ptr) })
    }

    /// First-fit free-list search.
    fn alloc_from_free_list(&self, needed: usize) -> Option<NonNull<u8>> {
        let mut prev: Option<NonNull<u8>> = None;
        let mut curr = self.free_head.get();
        while let Some(entry) = curr {
            let size = unsafe { read_free_size(entry) };
            if size >= needed {
                // Hit: remove from the list
                let next = unsafe { read_free_next(entry) };
                if let Some(p) = prev {
                    unsafe { write_free_next(p, next) };
                } else {
                    self.free_head.set(next);
                }
                return Some(entry);
            }
            prev = curr;
            curr = unsafe { read_free_next(entry) };
        }
        None
    }

    /// Reclaim a dead node into the hole list (called by sweep).
    ///
    /// Holes are sorted, merged and rebuilt into the free-list in a single
    /// batch at the end of sweep via [`finish_sweep`].
    pub(crate) fn collect_hole(&self, holes: &mut Vec<Hole>, node: NonNull<u8>, node_size: usize) {
        // The arena always allocates at least MIN_ALLOC_SIZE bytes, so
        // the effective size used for bump accounting must be rounded up.
        let actual_size = node_size.max(MIN_ALLOC_SIZE);

        // Frontier merge optimisation: if the dead node is right at the
        // bump frontier, just rewind the bump pointer instead of entering
        // the free-list.
        let node_end = node.as_ptr() as usize + actual_size;
        let bump_end = self.base.as_ptr() as usize + self.bump.get();
        if node_end == bump_end {
            // ★ Perfect rewind
            self.bump
                .set(node.as_ptr() as usize - self.base.as_ptr() as usize);
            self.live_count.set(self.live_count.get().saturating_sub(1));
            return;
        }

        // Record the hole for batch processing in finish_sweep
        let offset = node.as_ptr() as usize - self.base.as_ptr() as usize;
        holes.push(Hole {
            offset,
            size: actual_size,
        });
        self.live_count.set(self.live_count.get().saturating_sub(1));
    }

    /// Called at the end of sweep: sort, merge adjacent holes and rebuild the free-list.
    pub(crate) fn finish_sweep(&self, holes: &mut Vec<Hole>) {
        if self.live_count.get() == 0 {
            // All dead → full reset, no free-list needed
            self.bump.set(0);
            self.free_head.set(None);
            holes.clear();
            return;
        }

        // 1. Sort by address
        holes.sort_by_key(|h| h.offset);

        // 2. Merge adjacent holes
        let mut merged: Vec<Hole> = Vec::with_capacity(holes.len());
        let mut iter = holes.drain(..);
        if let Some(mut cur) = iter.next() {
            for next in iter {
                if cur.offset + cur.size == next.offset {
                    // Adjacent → merge
                    cur.size += next.size;
                } else {
                    merged.push(cur);
                    cur = next;
                }
            }
            merged.push(cur);
        }

        // 3. Rebuild the free-list (reverse order for correct head-insertion)
        self.free_head.set(None);
        for hole in merged.into_iter().rev() {
            let ptr = unsafe { NonNull::new_unchecked(self.base.as_ptr().add(hole.offset)) };
            unsafe { write_free_size(ptr, hole.size) };
            unsafe { write_free_next(ptr, self.free_head.get()) };
            self.free_head.set(Some(ptr));
        }
    }
}

impl Drop for GcArena {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.capacity, 16).unwrap();
        unsafe { std::alloc::dealloc(self.base.as_ptr(), layout) };
    }
}

// ── FreeEntry read / write (re-uses dead GcHead space) ──────────────────
//
// Memory layout of a dead node (first 16 bytes):
//   offset 0: size (8 bytes, reuses the attrs + partition area)
//   offset 8: next  (8 bytes, reuses the first 8 bytes of weak_id)

unsafe fn read_free_size(entry: NonNull<u8>) -> usize {
    unsafe { *(entry.as_ptr().cast::<usize>()) }
}

unsafe fn write_free_size(entry: NonNull<u8>, size: usize) {
    unsafe {
        *(entry.as_ptr().cast::<usize>()) = size;
    }
}

unsafe fn read_free_next(entry: NonNull<u8>) -> Option<NonNull<u8>> {
    unsafe { *(entry.as_ptr().add(8).cast::<Option<NonNull<u8>>>()) }
}

unsafe fn write_free_next(entry: NonNull<u8>, next: Option<NonNull<u8>>) {
    unsafe {
        *(entry.as_ptr().add(8).cast::<Option<NonNull<u8>>>()) = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: allocate a single u64 (8 bytes) and return its offset from base.
    fn alloc_one(arena: &GcArena) -> Option<NonNull<u8>> {
        arena.alloc(Layout::new::<u64>())
    }

    /// Helper: read a u64 at the given pointer.
    unsafe fn read_u64(p: NonNull<u8>) -> u64 {
        unsafe { *(p.as_ptr().cast::<u64>()) }
    }

    /// Helper: write a u64 at the given pointer.
    unsafe fn write_u64(p: NonNull<u8>, val: u64) {
        unsafe {
            *(p.as_ptr().cast::<u64>()) = val;
        }
    }

    // ── Bump allocation basics ──────────────────────────────────────────

    #[test]
    fn test_arena_new_and_bump() {
        let arena = GcArena::new(1024).unwrap();
        let p1 = alloc_one(&arena).unwrap();
        let p2 = alloc_one(&arena).unwrap();
        // Pointers must be distinct and within arena bounds
        assert_ne!(p1, p2);
        let base = arena.base.as_ptr() as usize;
        assert!((p1.as_ptr() as usize) >= base);
        assert!((p2.as_ptr() as usize) < (base + 1024));
    }

    #[test]
    fn test_arena_exhaustion() {
        let arena = GcArena::new(64).unwrap();
        // Each alloc uses at least MIN_ALLOC_SIZE (16) bytes.
        // 64 / 16 = 4 allocations fit exactly.
        assert!(alloc_one(&arena).is_some());
        assert!(alloc_one(&arena).is_some());
        assert!(alloc_one(&arena).is_some());
        assert!(alloc_one(&arena).is_some());
        // The 5th should fail
        assert!(alloc_one(&arena).is_none());
    }

    #[test]
    fn test_bump_allocs_are_sequential_non_overlapping() {
        let arena = GcArena::new(256).unwrap();
        let p1 = alloc_one(&arena).unwrap();
        let p2 = alloc_one(&arena).unwrap();
        let p3 = alloc_one(&arena).unwrap();
        // Bump pointers should be strictly increasing
        assert!(p1.as_ptr() < p2.as_ptr());
        assert!(p2.as_ptr() < p3.as_ptr());
    }

    // ── Free-list: collect holes and rebuild ───────────────────────────

    #[test]
    fn test_free_list_reuses_hole() {
        let arena = GcArena::new(256).unwrap();
        let p1 = alloc_one(&arena).unwrap();
        let p2 = alloc_one(&arena).unwrap();
        let _p3 = alloc_one(&arena).unwrap();

        // "Sweep" p2: collect it as a hole
        let mut holes = Vec::new();
        arena.collect_hole(&mut holes, p2, 8);

        // finish_sweep should rebuild the free-list
        arena.finish_sweep(&mut holes);
        assert!(holes.is_empty());

        // Now allocate again — it should come from the free-list,
        // specifically from p2's old location.
        let p_new = alloc_one(&arena).unwrap();
        assert_eq!(p_new, p2, "free-list should reuse the hole at p2's address");
    }

    #[test]
    fn test_free_list_two_adjacent_holes_merge() {
        let arena = GcArena::new(256).unwrap();
        let p1 = alloc_one(&arena).unwrap();
        let p2 = alloc_one(&arena).unwrap();
        let p3 = alloc_one(&arena).unwrap();
        let _p4 = alloc_one(&arena).unwrap();

        // Collect p2 and p3 (adjacent)
        let mut holes = Vec::new();
        arena.collect_hole(&mut holes, p2, 8);
        arena.collect_hole(&mut holes, p3, 8);
        assert_eq!(holes.len(), 2);

        arena.finish_sweep(&mut holes);

        // The two adjacent 8-byte holes should be merged into one 16-byte hole.
        // Allocating a 16-byte object should reuse the merged hole.
        let big = arena
            .alloc(Layout::from_size_align(16, 8).unwrap())
            .unwrap();
        assert_eq!(big, p2, "merged hole should cover p2..p3+8");
    }

    #[test]
    fn test_free_list_three_adjacent_holes_merge() {
        let arena = GcArena::new(256).unwrap();
        let p1 = alloc_one(&arena).unwrap();
        let p2 = alloc_one(&arena).unwrap();
        let p3 = alloc_one(&arena).unwrap();
        let p4 = alloc_one(&arena).unwrap();
        let _p5 = alloc_one(&arena).unwrap();

        // Collect p2, p3, p4 (all adjacent)
        let mut holes = Vec::new();
        arena.collect_hole(&mut holes, p2, 8);
        arena.collect_hole(&mut holes, p3, 8);
        arena.collect_hole(&mut holes, p4, 8);
        arena.finish_sweep(&mut holes);

        // Allocate a 24-byte slot — should land on the merged triple hole
        let big = arena
            .alloc(Layout::from_size_align(24, 8).unwrap())
            .unwrap();
        assert_eq!(big, p2, "triple-merged hole should start at p2");
    }

    // ── Frontier rewind optimisation ───────────────────────────────────

    #[test]
    fn test_frontier_rewind_last_node() {
        let arena = GcArena::new(256).unwrap();
        let p1 = alloc_one(&arena).unwrap();
        let p2 = alloc_one(&arena).unwrap();
        let p3 = alloc_one(&arena).unwrap();

        // Collect p3 (the last allocated node — right at bump frontier)
        let mut holes = Vec::new();
        arena.collect_hole(&mut holes, p3, 8);
        // collect_hole should have recognised the frontier match and
        // rewound bump instead of pushing to holes.
        assert!(
            holes.is_empty(),
            "frontier rewind should not produce a hole"
        );

        // Bump should be back to what it was after p2
        let p_new = alloc_one(&arena).unwrap();
        assert_eq!(p_new, p3, "frontier rewind should allow reuse of p3's slot");
    }

    #[test]
    fn test_frontier_rewind_chain() {
        let arena = GcArena::new(256).unwrap();
        let p1 = alloc_one(&arena).unwrap();
        let p2 = alloc_one(&arena).unwrap();
        let p3 = alloc_one(&arena).unwrap();
        let p4 = alloc_one(&arena).unwrap();

        // Collect p3 and p4 in reverse order (p4 is at frontier)
        let mut holes = Vec::new();
        arena.collect_hole(&mut holes, p4, 8); // frontier → rewind
        arena.collect_hole(&mut holes, p3, 8); // now at new frontier → rewind
        assert!(holes.is_empty(), "both should use frontier rewind");

        // Bump back to after p2, allocate should reuse p3
        let p_new = alloc_one(&arena).unwrap();
        assert_eq!(p_new, p3, "frontier rewind chain works");
    }

    // ── Full reset ─────────────────────────────────────────────────────

    #[test]
    fn test_arena_full_reset() {
        let arena = GcArena::new(256).unwrap();
        let p1 = alloc_one(&arena).unwrap();
        let p2 = alloc_one(&arena).unwrap();

        // Collect both — live_count becomes 0
        let mut holes = Vec::new();
        arena.collect_hole(&mut holes, p1, 8);
        arena.collect_hole(&mut holes, p2, 8);
        assert_eq!(arena.live_count.get(), 0);

        // finish_sweep should reset the arena entirely
        arena.finish_sweep(&mut holes);
        assert_eq!(arena.bump.get(), 0);
        assert!(arena.free_head.get().is_none());

        // Now allocations should start from the beginning (base)
        let p_new1 = alloc_one(&arena).unwrap();
        assert_eq!(p_new1, p1, "full reset reuses from base");
    }

    // ── Mixed: frontier rewind + holes ─────────────────────────────────

    #[test]
    fn test_mixed_frontier_and_holes() {
        let arena = GcArena::new(256).unwrap();
        let p1 = alloc_one(&arena).unwrap();
        let p2 = alloc_one(&arena).unwrap();
        let p3 = alloc_one(&arena).unwrap();
        let p4 = alloc_one(&arena).unwrap();
        let p5 = alloc_one(&arena).unwrap();

        // p1, p3, p5 are dead (interleaved with live p2, p4)
        // p5 is at the frontier → rewound
        // p1 and p3 need to be recorded as holes
        let mut holes = Vec::new();
        arena.collect_hole(&mut holes, p5, 8); // frontier rewind
        arena.collect_hole(&mut holes, p3, 8); // hole
        arena.collect_hole(&mut holes, p1, 8); // hole
        // p5 was frontier → no hole; p1 and p3 are holes
        assert_eq!(holes.len(), 2);

        arena.finish_sweep(&mut holes);
        assert!(holes.is_empty());

        // Allocate 2 objects — they should come from free-list (holes)
        let n1 = alloc_one(&arena).unwrap();
        // free_head → p1 (lowest addr) → p3 (highest addr) → None
        // First alloc pops the head → p1
        assert_eq!(n1, p1, "first free-list alloc should reuse p1");

        let n2 = alloc_one(&arena).unwrap();
        assert_eq!(n2, p3, "second free-list alloc should reuse p3");

        // Now empty free-list, next alloc bumps after p4
        let n3 = alloc_one(&arena).unwrap();
        assert!(
            (n3.as_ptr() > p4.as_ptr()),
            "bump after p4 after free-list exhausted"
        );
    }

    // ── Large object bypass ────────────────────────────────────────────

    #[test]
    fn test_large_allocs_return_none() {
        // Arena of 1024 bytes; an allocation larger than that should fail
        let arena = GcArena::new(1024).unwrap();
        let large = arena.alloc(Layout::from_size_align(2048, 16).unwrap());
        assert!(
            large.is_none(),
            "alloc larger than capacity should return None"
        );
    }

    // ── Data integrity after arena reuse ───────────────────────────────

    #[test]
    fn test_data_integrity_after_free_list_reuse() {
        let arena = GcArena::new(256).unwrap();
        unsafe {
            let p1 = alloc_one(&arena).unwrap();
            write_u64(p1, 0xDEAD);
            let p2 = alloc_one(&arena).unwrap();
            write_u64(p2, 0xBEEF);

            assert_eq!(read_u64(p1), 0xDEAD);
            assert_eq!(read_u64(p2), 0xBEEF);

            // Collect p1
            let mut holes = Vec::new();
            arena.collect_hole(&mut holes, p1, 8);
            arena.finish_sweep(&mut holes);

            // Reuse p1's slot — write new data
            let p_new = alloc_one(&arena).unwrap();
            assert_eq!(p_new, p1);
            write_u64(p_new, 0xCAFE);

            // p2 should still have its data
            assert_eq!(read_u64(p2), 0xBEEF);
            // New data should be readable
            assert_eq!(read_u64(p_new), 0xCAFE);
        }
    }
}
