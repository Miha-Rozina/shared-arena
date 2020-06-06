
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering::*};
use std::cell::UnsafeCell;
use std::sync::{Arc, Weak};

use std::ptr::NonNull;
use std::alloc::{alloc, dealloc, Layout};

use static_assertions::const_assert;

use crate::cache_line::CacheAligned;

// // https://stackoverflow.com/a/53646925
// const fn max(a: usize, b: usize) -> usize {
//     [a, b][(a < b) as usize]
// }

// const ALIGN_BLOCK: usize = max(128, 64);

pub const BITFIELD_WIDTH: usize = std::mem::size_of::<AtomicUsize>() * 8;
pub const BLOCK_PER_PAGE: usize = BITFIELD_WIDTH - 1;
pub const MASK_ARENA_BIT: usize = 1 << (BITFIELD_WIDTH - 1);

pub type Bitfield = AtomicUsize;

const_assert!(std::mem::size_of::<Bitfield>() == BITFIELD_WIDTH / 8);

// We make the struct repr(C) to ensure that the pointer to the inner
// value remains at offset 0. This is to avoid any pointer arithmetic
// when dereferencing it
#[repr(C)]
pub struct Block<T> {
    /// Inner value
    pub value: UnsafeCell<T>,
    /// Number of references to this block
    pub counter: AtomicUsize,
    /// Information about its page.
    /// It's a tagged pointer on 64 bits architectures.
    /// Contains:
    ///   - Pointer to page
    ///   - Index in page
    ///   - PageKind
    /// Read only and initialized on Page creation
    /// Doesn't need to be atomic
    page: PageTaggedPtr,
}

impl<T> Block<T> {
    pub(crate) fn drop_block(block: NonNull<Block<T>>) {
        let block_ref = unsafe { block.as_ref() };

        match block_ref.page.page_kind() {
            PageKind::PageSharedArena => {
                let page_ptr = block_ref.page.page_ptr::<Page<T>>();
                Page::<T>::drop_block(page_ptr, block);
            }
            _ => {
                unimplemented!()
            }
        }
    }
}

#[derive(Copy, Clone)]
struct PageTaggedPtr {
    pub data: usize
}

impl std::fmt::Debug for PageTaggedPtr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageTaggedPtr")
         .field("data    ", &format!("{:064b}", self.data))
         .field("page_ptr", &format!("{:064b}", self.page_ptr::<usize>().as_ptr() as usize))
         .field("page_kind", &self.page_kind())
         .field("page_index_block", &format!("{:08b} ({})", self.index_block(), self.index_block()))
         .finish()
    }
}

impl PageTaggedPtr {
    fn new(page_ptr: usize, index: usize, kind: PageKind) -> PageTaggedPtr {
        let kind: usize = kind.into();
        // Index is 6 bits at most
        // Kind is 1 bit
        let kind = kind << 6;
        // Tag is 7 bits
        let tag = kind | index;

        PageTaggedPtr {
            data: (page_ptr & !(0b1111111 << 57)) | (tag << 57)
        }
    }

    fn page_ptr<T>(self) -> NonNull<T> {
        let ptr = ((self.data << 7) as isize >> 7) as *mut T;

        NonNull::new(ptr).unwrap()
    }

    fn page_kind(self) -> PageKind {
        PageKind::from(self)
    }

    fn index_block(self) -> usize {
        (self.data >> 57) & 0b111111
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PageKind {
    PageSharedArena = 0,
    PageArena = 1
}

impl From<PageTaggedPtr> for PageKind {
    fn from(source: PageTaggedPtr) -> Self {
        if (source.data >> 63) == 0 {
            PageKind::PageSharedArena
        } else {
            PageKind::PageArena
        }
    }
}

impl Into<usize> for PageKind {
    fn into(self) -> usize {
        match self {
            PageKind::PageSharedArena => 0,
            PageKind::PageArena => 1
        }
    }
}


pub struct Page<T> {
    /// Bitfield representing free and non-free blocks.
    /// - 1 = free
    /// - 0 = non-free
    /// The most significant bit is dedicated to the arena and is
    /// used to determine when to deallocate the Page.
    /// With this bit reserved, we used BITFIELD_WIDTH - 1 bits for
    /// the blocks.
    /// Note that the bit for the arena is inversed:
    /// - 1 = Page is still referenced from an arena
    /// - 0 = The Page isn't referenced in an arena
    /// It is inversed so that Bitfield::trailing_zeros doesn't
    /// count that bit
    pub bitfield: CacheAligned<Bitfield>,
    /// Array of Block
    pub blocks: [Block<T>; BLOCK_PER_PAGE],
    pub arena_pending_list: Weak<AtomicPtr<Page<T>>>,
    pub next_free: AtomicPtr<Page<T>>,
    pub next: AtomicPtr<Page<T>>,
    pub in_free_list: AtomicBool,
}

impl<T> std::fmt::Debug for Page<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
         .field("next_free", &self.next_free.load(Relaxed))
         .field("next", &self.next.load(Relaxed))
         .finish()
    }
}

fn deallocate_page<T>(page: *mut Page<T>) {
    let layout = Layout::new::<Page<T>>();
    unsafe {
        dealloc(page as *mut Page<T> as *mut u8, layout);
    }
}

impl<T> Page<T> {
    fn allocate() -> NonNull<Page<T>> {
        let layout = Layout::new::<Page<T>>();
        unsafe {
            let page = alloc(layout) as *const Page<T>;
            NonNull::from(&*page)
        }
    }

    fn new(
        arena_pending_list: Weak<AtomicPtr<Page<T>>>,
        next: *mut Page<T>
    ) -> NonNull<Page<T>>
    {
        let mut page_ptr = Self::allocate();
        let page_copy = page_ptr;

        let page = unsafe { page_ptr.as_mut() };

        // Initialize the page
        // Don't invoke any Drop here, the allocated page is uninitialized

        // We fill the bitfield with ones
        page.bitfield.store(!0, Relaxed);
        page.next_free = AtomicPtr::new(next);
        page.next = AtomicPtr::new(next);
        page.in_free_list = AtomicBool::new(true);

        let pending_ptr = &mut page.arena_pending_list as *mut Weak<AtomicPtr<Page<T>>>;
        unsafe {
            pending_ptr.write(arena_pending_list);
        }

        // initialize the blocks
        for (index, block) in page.blocks.iter_mut().enumerate() {
            block.page = PageTaggedPtr::new(page_copy.as_ptr() as usize, index, PageKind::PageSharedArena);
            block.counter = AtomicUsize::new(0);
        }

        page_ptr
    }

    /// Make a new list of Page
    ///
    /// Returns the first and last Page in the list
    pub fn make_list(
        npages: usize,
        arena_pending_list: &Arc<AtomicPtr<Page<T>>>
    ) -> (NonNull<Page<T>>, NonNull<Page<T>>)
    {
        let arena_pending_list = Arc::downgrade(arena_pending_list);

        let last = Page::<T>::new(arena_pending_list.clone(), std::ptr::null_mut());
        let mut previous = last;

        for _ in 0..npages - 1 {
            let page = Page::<T>::new(arena_pending_list.clone(), previous.as_ptr());
            previous = page;
        }

        (previous, last)
    }

    /// Search for a free [`Block`] in the [`Page`] and mark it as non-free
    ///
    /// If there is no free block, it returns None
    pub fn acquire_free_block(&self) -> Option<NonNull<Block<T>>> {
        loop {
            let bitfield = self.bitfield.load(Relaxed);

            let index_free = bitfield.trailing_zeros() as usize;

            if index_free == BLOCK_PER_PAGE {
                return None;
            }

            let bit = 1 << index_free;

            let previous_bitfield = self.bitfield.fetch_and(!bit, AcqRel);

            // We check that the bit was still set in previous_bitfield.
            // If the bit is zero, it means another thread took it.
            if previous_bitfield & bit != 0 {
                return self.blocks.get(index_free).map(NonNull::from);
            }
        }
    }

    pub(super) fn drop_block(mut page: NonNull<Page<T>>, block: NonNull<Block<T>>) {
        let page_ptr = page.as_ptr();
        let page = unsafe { page.as_mut() };
        let block = unsafe { block.as_ref() };

        unsafe {
            // Drop the inner value
            std::ptr::drop_in_place(block.value.get());
        }

        // Put our page in pending_free_list of the arena, if necessary
        if !page.in_free_list.load(Relaxed) {
            // Another thread might have changed self.in_free_list
            // We could use compare_exchange here but swap is faster
            // 'lock cmpxchg' vs 'xchg' on x86
            // For self reference:
            // https://gpuopen.com/gdc-presentations/2019/gdc-2019-s2-amd-ryzen-processor-software-optimization.pdf
            if !page.in_free_list.swap(true, Acquire) {
                if let Some(arena_pending_list) = page.arena_pending_list.upgrade() {
                    loop {
                        let current = arena_pending_list.load(Relaxed);
                        page.next_free.store(current, Relaxed);

                        if arena_pending_list.compare_exchange(
                            current, page_ptr, AcqRel, Relaxed
                        ).is_ok() {
                            break;
                        }
                    }
                }
            }
        }

        let bit = 1 << block.page.index_block();

        // We set our bit to mark the block as free.
        // fetch_add is faster than fetch_or (xadd vs cmpxchg), and
        // we're sure to be the only thread to set this bit.
        let old_bitfield = page.bitfield.fetch_add(bit, AcqRel);

        let new_bitfield = old_bitfield | bit;

        // The bit dedicated to the Arena is inversed (1 for used, 0 for free)
        if !new_bitfield == MASK_ARENA_BIT {
            // We were the last block/arena referencing this page
            // Deallocate it
            deallocate_page(page_ptr);
        }
    }
}

pub(super) fn drop_page<T>(page: *mut Page<T>) {
    // We clear the bit dedicated to the arena
    let old_bitfield = {
        let page = unsafe { page.as_ref().unwrap() };
        page.bitfield.fetch_sub(MASK_ARENA_BIT, AcqRel)
    };

    if !old_bitfield == 0 {
        // No one is referencing this page anymore (neither Arena, ArenaBox or ArenaArc)
        deallocate_page(page);
    }
}

impl<T> Drop for Page<T> {
    fn drop(&mut self) {
        panic!("PAGE");
    }
}

#[cfg(test)]
mod tests {
    use super::{PageKind, PageTaggedPtr};

    #[test]
    fn page_tagged_ptr() {
        for index_block in 0..64 {
            let tagged_ptr = PageTaggedPtr::new(!0, index_block, PageKind::PageSharedArena);
            let ptr = tagged_ptr.page_ptr::<usize>().as_ptr();
            assert_eq!(ptr, !0 as *mut _, "{:064b}", ptr as usize);
            assert_eq!(tagged_ptr.page_kind(), PageKind::PageSharedArena);
            assert_eq!(tagged_ptr.index_block(), index_block);

            let tagged_ptr = PageTaggedPtr::new(!0, index_block, PageKind::PageArena);
            let ptr = tagged_ptr.page_ptr::<usize>().as_ptr();
            assert_eq!(ptr, !0 as *mut _, "{:064b}", ptr as usize);
            assert_eq!(tagged_ptr.page_kind(), PageKind::PageArena);
            assert_eq!(tagged_ptr.index_block(), index_block);

            let tagged_ptr = PageTaggedPtr::new(16, index_block, PageKind::PageSharedArena);
            let ptr = tagged_ptr.page_ptr::<usize>().as_ptr();
            assert_eq!(ptr, 16 as *mut _, "{:064b}", ptr as usize);
            assert_eq!(tagged_ptr.page_kind(), PageKind::PageSharedArena);
            assert_eq!(tagged_ptr.index_block(), index_block);

            let tagged_ptr = PageTaggedPtr::new(16, index_block, PageKind::PageArena);
            let ptr = tagged_ptr.page_ptr::<usize>().as_ptr();
            assert_eq!(ptr, 16 as *mut _, "{:064b}", ptr as usize);
            assert_eq!(tagged_ptr.page_kind(), PageKind::PageArena);
            assert_eq!(tagged_ptr.index_block(), index_block);
        }
    }
}
