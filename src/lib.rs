//! The arena, a fast but limited type of allocator.
//!
//! **A fast (but limited) allocation arena for values of a single type.**
//!
//! Allocated objects are destroyed all at once, when the arena itself is
//! destroyed. There is no deallocation of individual objects while the arena
//! itself is still alive. The flipside is that allocation is fast: typically
//! just a vector push.
//!
//! There is also a method `into_vec()` to recover ownership of allocated
//! objects when the arena is no longer required, instead of destroying
//! everything.
//!
//! ## Example
//!
//! ```
//! use typed_arena::Arena;
//!
//! struct Monster {
//!     level: u32,
//! }
//!
//! let monsters = Arena::new();
//!
//! let vegeta = monsters.alloc(Monster { level: 9001 });
//! assert!(vegeta.level > 9000);
//! ```
//!
//! ## Safe Cycles
//!
//! All allocated objects get the same lifetime, so you can safely create cycles
//! between them. This can be useful for certain data structures, such as graphs
//! and trees with parent pointers.
//!
//! ```
//! use std::cell::Cell;
//! use typed_arena::Arena;
//!
//! struct CycleParticipant<'a> {
//!     other: Cell<Option<&'a CycleParticipant<'a>>>,
//! }
//!
//! let arena = Arena::new();
//!
//! let a = arena.alloc(CycleParticipant { other: Cell::new(None) });
//! let b = arena.alloc(CycleParticipant { other: Cell::new(None) });
//!
//! a.other.set(Some(b));
//! b.other.set(Some(a));
//! ```

// Potential optimizations:
// 1) add and stabilize a method for in-place reallocation of vecs.
// 2) add and stabilize placement new.
// 3) use an iterator. This may add far too much unsafe code.

#![deny(missing_docs)]
#![cfg_attr(not(any(feature = "std", test)), no_std)]
#![cfg_attr(not(feature = "std"), feature(alloc))]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(any(feature = "std", test))]
extern crate core;

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use core::cell::RefCell;
use core::cmp;
use core::iter;
use core::mem;
use core::slice;

#[cfg(test)]
mod test;

// Initial size in bytes.
const INITIAL_SIZE: usize = 1024;
// Minimum capacity. Must be larger than 0.
const MIN_CAPACITY: usize = 1;

/// An arena of objects of type `T`.
///
/// ## Example
///
/// ```
/// use typed_arena::Arena;
///
/// struct Monster {
///     level: u32,
/// }
///
/// let monsters = Arena::new();
///
/// let vegeta = monsters.alloc(Monster { level: 9001 });
/// assert!(vegeta.level > 9000);
/// ```
pub struct Arena<T> {
    chunks: RefCell<ChunkList<T>>,
}

struct ChunkList<T> {
    current: Vec<T>,
    rest: Vec<Vec<T>>,
}

impl<T> Arena<T> {
    /// Construct a new arena.
    ///
    /// ## Example
    ///
    /// ```
    /// use typed_arena::Arena;
    ///
    /// let arena = Arena::new();
    /// # arena.alloc(1);
    /// ```
    pub fn new() -> Arena<T> {
        let size = cmp::max(1, mem::size_of::<T>());
        Arena::with_capacity(INITIAL_SIZE / size)
    }

    /// Construct a new arena with capacity for `n` values pre-allocated.
    ///
    /// ## Example
    ///
    /// ```
    /// use typed_arena::Arena;
    ///
    /// let arena = Arena::with_capacity(1337);
    /// # arena.alloc(1);
    /// ```
    pub fn with_capacity(n: usize) -> Arena<T> {
        let n = cmp::max(MIN_CAPACITY, n);
        Arena {
            chunks: RefCell::new(ChunkList {
                current: Vec::with_capacity(n),
                rest: Vec::new(),
            }),
        }
    }

    /// Allocates a value in the arena, and returns a mutable reference
    /// to that value.
    ///
    /// ## Example
    ///
    /// ```
    /// use typed_arena::Arena;
    ///
    /// let arena = Arena::new();
    /// let x = arena.alloc(42);
    /// assert_eq!(*x, 42);
    /// ```
    #[inline]
    pub fn alloc(&self, value: T) -> &mut T {
        self.alloc_fast_path(value)
            .unwrap_or_else(|value| self.alloc_slow_path(value))
    }

    #[inline]
    fn alloc_fast_path(&self, value: T) -> Result<&mut T, T> {
        let mut chunks = self.chunks.borrow_mut();
        let len = chunks.current.len();
        if len < chunks.current.capacity() {
            chunks.current.push(value);
            // Avoid going through `Vec::deref_mut`, which overlaps
            // other references we have already handed out!
            debug_assert!(len < chunks.current.len()); // bounds check
            Ok(unsafe { &mut *chunks.current.as_mut_ptr().add(len) })
        } else {
            Err(value)
        }
    }

    fn alloc_slow_path(&self, value: T) -> &mut T {
        &mut self.alloc_extend(iter::once(value))[0]
    }

    /// Uses the contents of an iterator to allocate values in the arena.
    /// Returns a mutable slice that contains these values.
    ///
    /// ## Example
    ///
    /// ```
    /// use typed_arena::Arena;
    ///
    /// let arena = Arena::new();
    /// let abc = arena.alloc_extend("abcdefg".chars().take(3));
    /// assert_eq!(abc, ['a', 'b', 'c']);
    /// ```
    pub fn alloc_extend<I>(&self, iterable: I) -> &mut [T]
    where
        I: IntoIterator<Item = T>,
    {
        let mut iter = iterable.into_iter();

        let mut chunks = self.chunks.borrow_mut();

        let iter_min_len = iter.size_hint().0;
        let mut next_item_index;
        if chunks.current.len() + iter_min_len > chunks.current.capacity() {
            chunks.reserve(iter_min_len);
            chunks.current.extend(iter);
            next_item_index = 0;
        } else {
            next_item_index = chunks.current.len();
            let mut i = 0;
            while let Some(elem) = iter.next() {
                if chunks.current.len() == chunks.current.capacity() {
                    // The iterator was larger than we could fit into the current chunk.
                    let chunks = &mut *chunks;
                    // Create a new chunk into which we can freely push the entire iterator into
                    chunks.reserve(i + 1);
                    let previous_chunk = chunks.rest.last_mut().unwrap();
                    let previous_chunk_len = previous_chunk.len();
                    // Move any elements we put into the previous chunk into this new chunk
                    chunks
                        .current
                        .extend(previous_chunk.drain(previous_chunk_len - i..));
                    chunks.current.push(elem);
                    // And the remaining elements in the iterator
                    chunks.current.extend(iter);
                    next_item_index = 0;
                    break;
                } else {
                    chunks.current.push(elem);
                }
                i += 1;
            }
        }
        let new_slice_ref = {
            let new_slice_ref = &mut chunks.current[next_item_index..];

            // Extend the lifetime from that of `chunks_borrow` to that of `self`.
            // This is OK because we’re careful to never move items
            // by never pushing to inner `Vec`s beyond their initial capacity.
            // The returned reference is unique (`&mut`):
            // the `Arena` never gives away references to existing items.
            unsafe { mem::transmute::<&mut [T], &mut [T]>(new_slice_ref) }
        };

        new_slice_ref
    }

    /// Allocates space for a given number of values, but doesn't initialize it.
    ///
    /// ## Unsafety and Undefined Behavior
    ///
    /// The same caveats that apply to
    /// [`std::mem::uninitialized`](https://doc.rust-lang.org/nightly/std/mem/fn.uninitialized.html)
    /// apply here:
    ///
    /// > **This is incredibly dangerous and should not be done lightly. Deeply
    /// consider initializing your memory with a default value instead.**
    ///
    /// In particular, it is easy to trigger undefined behavior by allocating
    /// uninitialized values, failing to properly initialize them, and then the
    /// `Arena` will attempt to drop them when it is dropped. Initializing an
    /// uninitialized value is trickier than it might seem: a normal assignment
    /// to a field will attempt to drop the old, uninitialized value, which
    /// almost certainly also triggers undefined behavior. You must also
    /// consider all the places where your code might "unexpectedly" drop values
    /// earlier than it "should" because of unwinding during panics.
    pub unsafe fn alloc_uninitialized(&self, num: usize) -> *mut [T] {
        let mut chunks = self.chunks.borrow_mut();

        if chunks.current.len() + num > chunks.current.capacity() {
            chunks.reserve(num);
        }

        // At this point, the current chunk must have free capacity.
        let next_item_index = chunks.current.len();
        chunks.current.set_len(next_item_index + num);
        // Extend the lifetime...
        &mut chunks.current[next_item_index..] as *mut _
    }

    /// Returns unused space.
    ///
    /// *This unused space is still not considered "allocated".* Therefore, it
    /// won't be dropped unless there are further calls to `alloc`,
    /// `alloc_uninitialized`, or `alloc_extend` which is why the method is
    /// safe.
    pub fn uninitialized_array(&self) -> *mut [T] {
        let chunks = self.chunks.borrow();
        let len = chunks.current.capacity() - chunks.current.len();
        let next_item_index = chunks.current.len();
        let slice = &chunks.current[next_item_index..];
        unsafe { slice::from_raw_parts_mut(slice.as_ptr() as *mut T, len) as *mut _ }
    }

    /// Convert this `Arena` into a `Vec<T>`.
    ///
    /// Items in the resulting `Vec<T>` appear in the order that they were
    /// allocated in.
    ///
    /// ## Example
    ///
    /// ```
    /// use typed_arena::Arena;
    ///
    /// let arena = Arena::new();
    ///
    /// arena.alloc("a");
    /// arena.alloc("b");
    /// arena.alloc("c");
    ///
    /// let easy_as_123 = arena.into_vec();
    ///
    /// assert_eq!(easy_as_123, vec!["a", "b", "c"]);
    /// ```
    pub fn into_vec(self) -> Vec<T> {
        let mut chunks = self.chunks.into_inner();
        // keep order of allocation in the resulting Vec
        let n = chunks
            .rest
            .iter()
            .fold(chunks.current.len(), |a, v| a + v.len());
        let mut result = Vec::with_capacity(n);
        for mut vec in chunks.rest {
            result.append(&mut vec);
        }
        result.append(&mut chunks.current);
        result
    }

    /// Returns an iterator that allows modifying each value.
    ///
    /// Items are yielded in the order that they were allocated.
    ///
    /// ## Example
    ///
    /// ```
    /// use typed_arena::Arena;
    ///
    /// #[derive(Debug, PartialEq, Eq)]
    /// struct Point { x: i32, y: i32 };
    ///
    /// let mut arena = Arena::new();
    ///
    /// arena.alloc(Point { x: 0, y: 0 });
    /// arena.alloc(Point { x: 1, y: 1 });
    ///
    /// for point in arena.iter_mut() {
    ///     point.x += 10;
    /// }
    ///
    /// let points = arena.into_vec();
    ///
    /// assert_eq!(points, vec![Point { x: 10, y: 0 }, Point { x: 11, y: 1 }]);
    ///
    /// ```
    ///
    /// ## Immutable Iteration
    ///
    /// Note that there is no corresponding `iter` method. Access to the arena's contents
    /// requries mutable access to the arena itself.
    ///
    /// ```compile_fail
    /// use typed_arena::Arena;
    ///
    /// let mut arena = Arena::new();
    /// let x = arena.alloc(1);
    ///
    /// // borrow error!
    /// for i in arena.iter_mut() {
    ///     println!("i: {}", i);
    /// }
    ///
    /// // borrow error!
    /// *x = 2;
    /// ```
    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<T> {
        let chunks = self.chunks.get_mut();
        let position = if chunks.rest.len() > 0 {
            let index = 0;
            let inner_iter = chunks.rest[index].iter_mut();
            // Extend the lifetime of the individual elements to that of the arena.
            // This is OK because we borrow the arena mutably to prevent new allocations
            // and we take care here to never move items inside the arena while the
            // iterator is alive.
            let inner_iter = unsafe { mem::transmute(inner_iter) };
            IterMutState::ChunkListRest { index, inner_iter }
        } else {
            // Extend the lifetime of the individual elements to that of the arena.
            let iter = unsafe { mem::transmute(chunks.current.iter_mut()) };
            IterMutState::ChunkListCurrent { iter }
        };
        IterMut {
            chunks,
            state: position,
        }
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ChunkList<T> {
    #[inline(never)]
    #[cold]
    fn reserve(&mut self, additional: usize) {
        let double_cap = self
            .current
            .capacity()
            .checked_mul(2)
            .expect("capacity overflow");
        let required_cap = additional
            .checked_next_power_of_two()
            .expect("capacity overflow");
        let new_capacity = cmp::max(double_cap, required_cap);
        let chunk = mem::replace(&mut self.current, Vec::with_capacity(new_capacity));
        self.rest.push(chunk);
    }
}

enum IterMutState<'a, T> {
    ChunkListRest {
        index: usize,
        inner_iter: slice::IterMut<'a, T>,
    },
    ChunkListCurrent {
        iter: slice::IterMut<'a, T>,
    },
}

/// Mutable arena iterator.
///
/// This struct is created by the [`iter_mut`](struct.Arena.html#method.iter_mut) method on [Arenas](struct.Arena.html).
pub struct IterMut<'a, T: 'a> {
    chunks: &'a mut ChunkList<T>,
    state: IterMutState<'a, T>,
}

impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = &'a mut T;
    fn next(&mut self) -> Option<&'a mut T> {
        loop {
            self.state = match self.state {
                IterMutState::ChunkListRest {
                    mut index,
                    ref mut inner_iter,
                } => {
                    match inner_iter.next() {
                        Some(item) => return Some(item),
                        None => {
                            index += 1;
                            if index < self.chunks.rest.len() {
                                let inner_iter = self.chunks.rest[index].iter_mut();
                                // Extend the lifetime of the individual elements to that of the arena.
                                let inner_iter = unsafe { mem::transmute(inner_iter) };
                                IterMutState::ChunkListRest { index, inner_iter }
                            } else {
                                let iter = self.chunks.current.iter_mut();
                                // Extend the lifetime of the individual elements to that of the arena.
                                let iter = unsafe { mem::transmute(iter) };
                                IterMutState::ChunkListCurrent { iter }
                            }
                        }
                    }
                }
                IterMutState::ChunkListCurrent { ref mut iter } => return iter.next(),
            };
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let current_len = self.chunks.current.len();
        let current_cap = self.chunks.current.capacity();
        if self.chunks.rest.is_empty() {
            (current_len, Some(current_len))
        } else {
            let rest_len = self.chunks.rest.len();
            let last_chunk_len = self
                .chunks
                .rest
                .last()
                .map(|chunk| chunk.len())
                .unwrap_or(0);

            let min = current_len + last_chunk_len;
            let max = min + (rest_len * current_cap / rest_len);

            (min, Some(max))
        }
    }
}
