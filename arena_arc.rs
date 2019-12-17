
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::ptr::NonNull;

use super::page::{IndexInPage, Page, Block};

/// A reference-counting pointer to `T` in the arena
///
/// The type `ArenaArc<T>` provides shared ownership of a value of type `T`,
/// in the [`Arena`]/[`SharedArena`].
/// Invoking [`Clone`] on `ArenaArc` produces a new `ArenaArc`
/// instance, which points to the same value, while increasing a
/// reference count.
///
/// When the last `ArenaArc` pointer to a given value is dropped,
/// the pointed-to value is also dropped and its dedicated memory
/// in the arena is marked as available for future allocation.
///
/// Shared mutable references in Rust is not allowed, if you need to
/// mutate through an `AtomicArc`, use a Mutex, RwLock or one of
/// the atomic types.
///
/// If you don't need to share the value, you should use [`ArenaBox`].
///
/// ## Cloning references
///
/// Creating a new reference from an existing reference counted pointer
/// is done using the `Clone` trait implemented for AtomicArc<T>
///
/// ## `Deref` behavior
///
/// `AtomicArc<T>` automatically dereferences to `T`, so you can call
/// `T`'s methods on a value of type `AtomicArc<T>`.
///
/// ```
/// use shared_arena::{ArenaArc, SharedArena};
///
/// let arena = shared_arena::new();
/// let my_vec: ArenaArc<Vec<u8>> = arena::alloc_arc(Vec::new());
///
/// assert!(my_vec.len() == 0);
/// ```
///
/// [`Arc`]: https://doc.rust-lang.org/std/sync/struct.Arc.html
/// [`Send`]: https://doc.rust-lang.org/std/marker/trait.Send.html
/// [`Sync`]: https://doc.rust-lang.org/std/marker/trait.Sync.html
/// [`deref`]: https://doc.rust-lang.org/std/ops/trait.Deref.html
/// [`Arena`]: ./struct.Arena.html
/// [`SharedArena`]: ./struct.SharedArena.html
/// [`ArenaBox`]: ./struct.ArenaBox.html
/// [`Clone`]: https://doc.rust-lang.org/std/clone/trait.Clone.html#tymethod.clone
///
pub struct ArenaArc<T> {
    page: Arc<Page<T>>,
    block: NonNull<Block<T>>,
}

unsafe impl<T: Send> Send for ArenaArc<T> {}
unsafe impl<T: Send + Sync> Sync for ArenaArc<T> {}

impl<T: std::fmt::Debug> std::fmt::Debug for ArenaArc<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

impl<T> ArenaArc<T> {
    pub fn new(page: Arc<Page<T>>, index_in_page: IndexInPage) -> ArenaArc<T> {
        let block = &page.nodes[index_in_page.0];

        let counter = block.counter.load(Ordering::Relaxed);
        assert!(counter == 0, "PoolArc: Counter not zero");

        block.counter.store(1, Ordering::Relaxed);
        let block = NonNull::from(block);

        ArenaArc { block, page }
    }
}

impl<T> Clone for ArenaArc<T> {
    /// Make a clone of the ArenaArc pointer.
    ///
    /// This increase the reference counter.
    #[inline]
    fn clone(&self) -> ArenaArc<T> {
        let block = unsafe { self.block.as_ref() };

        let old = block.counter.fetch_add(1, Ordering::Relaxed);

        assert!(old < isize::max_value() as usize);

        ArenaArc {
            page: self.page.clone(),
            block: self.block
        }
    }
}

impl<T> std::ops::Deref for ArenaArc<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.block.as_ref().value.get() }
    }
}

impl<T> std::ops::DerefMut for ArenaArc<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.block.as_ref().value.get() }
    }
}

pub(super) fn drop_block_in_arena<T>(page: &Page<T>, block: &Block<T>) {
    unsafe {
        // Drop the inner value
        std::ptr::drop_in_place(block.value.get());
    }

    let index_in_page = block.index_in_page;
    let bit = index_in_page % 8;

    let bitfield_ref = &page.bitfield[index_in_page / 8];

    let mut bitfield = bitfield_ref.load(Ordering::Relaxed);

    // We set our bit to mark the block as free
    let mut new_bitfield = bitfield | (1 << bit);

    while let Err(x) = bitfield_ref.compare_exchange_weak(
        bitfield, new_bitfield, Ordering::SeqCst, Ordering::Relaxed
    ) {
        bitfield = x;
        new_bitfield = bitfield | (1 << bit);
    }
}

/// Drop the ArenaArc<T> and decrement its reference counter
///
/// If it is the last reference to that value, the value is
/// also dropped
impl<T> Drop for ArenaArc<T> {
    fn drop(&mut self) {
        let (page, block) = unsafe {
            (self.page.as_ref(), self.block.as_ref())
        };

        // We decrement the reference counter
        let count = block.counter.fetch_sub(1, Ordering::AcqRel);

        // We were the last reference
        if count == 1 {
            drop_block_in_arena(page, block);
        };
    }
}
