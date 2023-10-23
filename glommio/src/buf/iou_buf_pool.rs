use std::{
    alloc::{Layout, LayoutError},
    cell::RefCell,
    io,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use flume::{Receiver, Sender, TryRecvError};
use memmap2::MmapMut;
use thiserror::Error;
use ubyte::ByteUnit;

// TODO: - make most of these configuration settings
const PAGE_ALIGN: ByteUnit = ByteUnit::Kibibyte(4);
const SMALLEST_BUFFER_SIZE: ByteUnit = PAGE_ALIGN; // 4KiB
pub(crate) const LARGEST_BUFFER_SIZE: ByteUnit = ByteUnit::Kibibyte(128);
const SMALLEST_BUFFER_OFFSET: u32 = SMALLEST_BUFFER_SIZE.as_u64().ilog2();
const DEFAULT_SLAB_CAP: usize = 128;
const MAX_REFCOUNT: usize = (isize::MAX) as usize;
static EMPTY_BUF: &[u8] = b"";

thread_local! {
    /// Buffer pool
    pub static BUFPOOL: BufferPool = BufferPool::init(DEFAULT_SLAB_CAP);
}

type HandlePtr = NonNull<BufferHandle>;

/// A wrapper for a collection of BufferSlabs of the same size
/// This unifies the Send/Receive queue for that size
struct SlabList {
    buffers: Receiver<HandlePtr>,
    slabs: Vec<BufferSlab>,
    recycler: Sender<HandlePtr>,
    size: usize,
}

impl SlabList {
    pub(crate) fn new(sz: usize, capacity: usize) -> Result<Self, AllocError> {
        let (recycle_tx, recycle_rx) = flume::unbounded();
        let slabs = vec![unsafe { BufferSlab::new(capacity, sz, recycle_tx.clone())? }];
        Ok(SlabList {
            buffers: recycle_rx,
            recycler: recycle_tx,
            slabs,
            size: sz,
        })
    }

    fn append_slab(&mut self) -> Result<(), AllocError> {
        let cap = self.slabs[0].count;
        self.slabs
            .push(unsafe { BufferSlab::new(cap, self.size, self.recycler.clone())? });
        Ok(())
    }

    /// Find the appropriate BufferPool based on the size and use it to allocate
    /// or retrieve an IouBuf from its free-list
    pub(crate) fn next(&mut self) -> Result<HandlePtr, AllocError> {
        match self.buffers.try_recv() {
            Ok(buf) => Ok(buf),
            Err(e) => Err(AllocError::from(e)),
        }
    }
}

/// A BufferPool is a collection of BufferSlabs of different sizes
pub struct BufferPool {
    slablists: Vec<RefCell<SlabList>>,
}

impl BufferPool {
    #[inline]
    fn calc_slab_index(sz: usize) -> usize {
        sz.next_power_of_two()
            .ilog2()
            .saturating_sub(SMALLEST_BUFFER_OFFSET) as usize
    }

    /// "Allocates" a buffer by fetching the next buffer from the freelist.
    /// This attempts to pull a free buffer from the respective freelist, if that fails
    /// it has the SlabList for that size allocate a new slab of buffers
    #[inline]
    pub(crate) fn next_buffer(&self, sz: usize) -> Result<HandlePtr, AllocError> {
        let idx = Self::calc_slab_index(sz);
        let hnd = if let Some(slablist) = self.slablists.get(idx) {
            let res = slablist.borrow_mut().next();
            match res {
                Ok(hnd) => Ok(hnd),
                // TODO: Handle the `TryRecvError::Empty` and `Disconnected` errors separately
                // `Disconnected` means something really weird has happened
                Err(_) => {
                    // TODO:  Cap this so we can't just append_slab forever
                    slablist.borrow_mut().append_slab()?;
                    slablist
                        .borrow_mut()
                        .buffers
                        .try_recv()
                        .map_err(AllocError::from)
                }
            }
        } else {
            //TODO: Before production, remove the panic and just return an error
            // AllocError::InvalidBufSize
            let bytes = LARGEST_BUFFER_SIZE.as_u64();
            panic!("Invalid buffer size: {sz}  -- index: {idx} -- largest: {bytes}");
            //Err(AllocError::InvalidBufferSize(sz))
        }?;
        Ok(hnd)
    }

    // Initialize the static slabs with their respectively sized buffers
    // Note that we want this to fail loudly if something goes wrong
    // because this fn is only run at startup
    fn init(capacity: usize) -> Self {
        let smallest_buffer_size = SMALLEST_BUFFER_SIZE.as_u64() as usize;
        let slab_count =
            (LARGEST_BUFFER_SIZE.as_u64().ilog2() - smallest_buffer_size.ilog2()) as usize;
        let mut slablists = Vec::with_capacity(slab_count);
        for shift in 0..=slab_count {
            let size = smallest_buffer_size << shift;
            let slabs = RefCell::new(SlabList::new(size, capacity).unwrap());
            slablists.push(slabs);
        }
        BufferPool { slablists }
    }
}

/// A recycling pool of buffers, and a place for the buffer's handles
///
/// This pool is designed to work with io_uring's buffer pre-allocator
/// and registration, so that it can hand ownership over to io_uring and
/// get buffers back
///
/// A BufferSlab contains both the allocated buffers, and the handles which are used
/// similarly to `ArcInner` to act as the refcount for a shared resource
///
/// The physical memory layout of a BufferSlab root is as follows:
///
/// ```ignore
/// | aligned size_of::<BufferHandle>() * count | padding to align to 4096 | buffer_size * count |
/// ```
#[allow(dead_code)]
struct BufferSlab {
    size: usize,
    count: usize,
    mmap: MmapMut,
}

impl BufferSlab {
    /// Construct a new Buffer Slab given a count and desired buffer size
    ///
    /// This mmaps a new anonymous blob of memory in order to accomodate the
    /// demand. It also creates its own dedicated recycling queue.
    pub unsafe fn new(
        count: usize,
        size: usize,
        recycle_tx: Sender<HandlePtr>,
    ) -> Result<Self, AllocError> {
        let page_align = PAGE_ALIGN.as_u64() as usize;
        // ensure that the requested buffer size is a power of 2 within range
        let adjusted_buf_size = size
            .checked_next_power_of_two()
            .ok_or_else(|| AllocError::InvalidBufferSize(size))?;
        if adjusted_buf_size > LARGEST_BUFFER_SIZE {
            return Err(AllocError::InvalidBufferSize(size));
        }
        let adjusted_buf_size =
            std::cmp::max(adjusted_buf_size, SMALLEST_BUFFER_SIZE.as_u64() as usize);
        // Create a layout for the buffers, aligning to the page_alignment
        let buf_layout = Layout::from_size_align(adjusted_buf_size, page_align).unwrap(); //.map_err(AllocError::from);
        let buf_total = buf_layout
            .size()
            .checked_mul(count)
            .ok_or_else(|| AllocError::InvalidBufferSize(count))?;

        // Calculate the layout of a slice of BufferHandles
        let handles_layout = Layout::array::<BufferHandle>(count).map_err(AllocError::from)?;
        let padding = handles_layout.padding_needed_for(page_align);
        let total = buf_total + padding + handles_layout.size();
        let mut buf = MmapMut::map_anon(total).map_err(AllocError::from)?;
        let buf_ptr = buf.as_mut_ptr();
        std::ptr::write_bytes(buf_ptr, 0, total);
        // Find the start of the buffers
        let buffers_start: *mut u8 = buf_ptr.add(padding + handles_layout.size());

        // Initialize all of the BufferHandles to point to their respective buffers
        let handles =
            std::slice::from_raw_parts_mut::<'_, BufferHandle>(buf.as_mut_ptr().cast(), count);

        for (idx, h) in handles.iter_mut().enumerate() {
            let buffer = NonNull::new_unchecked(buffers_start.add(idx * adjusted_buf_size));

            *h = BufferHandle {
                buf: NonNull::slice_from_raw_parts(buffer, adjusted_buf_size),
                refcount: AtomicUsize::new(0),
                recycler: Some(recycle_tx.clone()),
            };
            // This unwrap should never fail, but if it does, we want it to be loud
            recycle_tx
                .send(NonNull::new_unchecked(h as *mut BufferHandle))
                .unwrap();
        }

        Ok(BufferSlab {
            count,
            size: adjusted_buf_size,
            mmap: buf,
        })
    }
}

/// A Handle to a buffer from a `BufferSlab` which is `Send + Sync`
///
/// These know how to recycle the buffer that they manage back into the correct slab
pub(crate) struct BufferHandle {
    pub(crate) refcount: AtomicUsize,
    pub(crate) buf: NonNull<[u8]>,
    pub(crate) recycler: Option<Sender<HandlePtr>>,
}

impl BufferHandle {
    pub(crate) unsafe fn from_vec(v: Vec<u8>) -> NonNull<Self> {
        let mut v = std::mem::ManuallyDrop::new(v);
        let buf = v.as_mut_ptr();
        let cap = v.capacity();
        let boxhnd = Box::new(BufferHandle {
            refcount: AtomicUsize::new(1),
            buf: unsafe { NonNull::new_unchecked(std::ptr::slice_from_raw_parts_mut(buf, cap)) },
            recycler: None,
        });
        NonNull::new_unchecked(Box::into_raw(boxhnd))
    }

    pub(crate) unsafe fn empty() -> NonNull<Self> {
        let boxhnd = Box::new(BufferHandle {
            refcount: AtomicUsize::new(1),
            buf: unsafe {
                NonNull::new_unchecked(std::ptr::slice_from_raw_parts_mut(
                    EMPTY_BUF.as_ptr() as *mut u8,
                    0,
                ))
            },
            recycler: None,
        });
        NonNull::new_unchecked(Box::into_raw(boxhnd))
    }

    pub(crate) unsafe fn free(this: NonNull<BufferHandle>) {
        let me = unsafe { this.as_ref() };
        if let Some(recycler) = me.recycler.as_ref() {
            let _ = recycler.send(this);
        } else {
            // Is this the empty buf special case?
            if me.buf.as_ref().as_ptr() == EMPTY_BUF.as_ptr() {
                return;
            }
            // if this fails, we're going to have a bad time anyways, so might as well unwrap
            let layout = std::alloc::Layout::array::<u8>(me.buf.len()).unwrap();
            std::alloc::dealloc(me.buf.as_ptr().cast(), layout);
        }
    }

    pub(crate) fn increment(&self) {
        // Using a relaxed ordering is alright here, as knowledge of the
        // original reference prevents other threads from erroneously deleting
        // the object.
        //
        // As explained in the [Boost documentation][1], Increasing the
        // reference counter can always be done with memory_order_relaxed: New
        // references to an object can only be formed from an existing
        // reference, and passing an existing reference from one thread to
        // another must already provide any required synchronization.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        let old_size = self.refcount.fetch_add(1, Ordering::Relaxed);
        //println!("Bumped refcount to {}", old_size + 1);
        // However we need to guard against massive refcounts in case someone is `mem::forget`ing
        // Arcs. If we don't do this the count can overflow and users will use-after free. This
        // branch will never be taken in any realistic program. We abort because such a program is
        // incredibly degenerate, and we don't care to support it.
        //
        // This check is not 100% water-proof: we error when the refcount grows beyond `isize::MAX`.
        // But we do that check *after* having done the increment, so there is a chance here that
        // the worst already happened and we actually do overflow the `usize` counter. However, that
        // requires the counter to grow from `isize::MAX` to `usize::MAX` between the increment
        // above and the `abort` below, which seems exceedingly unlikely.
        if old_size > MAX_REFCOUNT {
            panic!("Exceeded MAX_REFCOUNT in IouBuf");
        }
    }

    pub(crate) fn decrement(this: NonNull<BufferHandle>) {
        let me = unsafe { this.as_ref() };
        let old_count = me.refcount.fetch_sub(1, Ordering::Release);
        if old_count != 1 {
            //println!("Dropped refcount to {}", old_count - 1);
            return;
        }
        //println!("old_count is 1.  Recycling buffer");
        // This fence is needed to prevent reordering of use of the data and
        // deletion of the data.  Because it is marked `Release`, the decreasing
        // of the reference count synchronizes with this `Acquire` fence. This
        // means that use of the data happens before decreasing the reference
        // count, which happens before this fence, which happens before the
        // deletion of the data.
        //
        // As explained in the [Boost documentation][1],
        //
        // > It is important to enforce any possible access to the object in one
        // > thread (through an existing reference) to *happen before* deleting
        // > the object in a different thread. This is achieved by a "release"
        // > operation after dropping a reference (any access to the object
        // > through this reference must obviously happened before), and an
        // > "acquire" operation before deleting the object.
        //
        // In particular, while the contents of an Arc are usually immutable, it's
        // possible to have interior writes to something like a Mutex<T>. Since a
        // Mutex is not acquired when it is deleted, we can't rely on its
        // synchronization logic to make writes in thread A visible to a destructor
        // running in thread B.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        // [2]: (https://github.com/rust-lang/rust/pull/41714)
        me.refcount.load(Ordering::Acquire);

        //println!("Freeing BufferHandle");
        unsafe {
            BufferHandle::free(this);
        }
    }
}

#[derive(Debug, Error)]
/// Error returned by allocation operations of the `BufferPool` and `IouBuf`
pub enum AllocError {
    #[error("Allocation failed")]
    /// Allocation Failed
    AllocFailed(#[from] std::io::Error),
    #[error("Maximum pool limited reached")]
    /// This allocation would exceed the specified maximum number of buffers for that size
    MaxPoolLimitReached,
    #[error("Invalid Buffer Size: {0}")]
    /// This allocation would exceed the maximum buffer size specification
    InvalidBufferSize(usize),
    ///
    #[error("Invalid Layout: {0}")]
    LayoutError(#[from] LayoutError),
    ///
    #[error("Fatal Error: {0}")]
    FatalError(&'static str),
    ///
    #[error("Out of Buffers")]
    OutOfBuffers,
    /// The requested IouBuf operations requires it to be unique, but it wasn't
    #[error("IouBuf operation requires uniqueness, but the refcount is > 1")]
    NotUnique,
    /// General Error
    #[error("Error : {0}")]
    General(String),
}

impl From<AllocError> for io::Error {
    fn from(err: AllocError) -> io::Error {
        let msg = err.to_string();
        match err {
            AllocError::AllocFailed(e) => io::Error::new(e.kind(), msg),
            AllocError::MaxPoolLimitReached => io::Error::new(io::ErrorKind::OutOfMemory, msg),
            AllocError::LayoutError(_) => io::Error::new(io::ErrorKind::Other, msg),
            AllocError::InvalidBufferSize(_) => io::Error::new(io::ErrorKind::Other, msg),
            AllocError::FatalError(_) => io::Error::new(io::ErrorKind::Other, msg),
            AllocError::OutOfBuffers => io::Error::new(io::ErrorKind::OutOfMemory, msg),
            AllocError::NotUnique => {
                std::io::Error::new(std::io::ErrorKind::AddrInUse, "IouBuf was not unique")
            }
            AllocError::General(_) => io::Error::new(io::ErrorKind::Other, msg),
        }
    }
}

impl From<TryRecvError> for AllocError {
    fn from(err: TryRecvError) -> Self {
        match err {
            TryRecvError::Empty => AllocError::OutOfBuffers,
            TryRecvError::Disconnected => {
                AllocError::FatalError("BufferPool recycling queue failed. Sender(s) Disconnected")
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn return_queue_works() {
        let mut slablist = SlabList::new(1000, 16).unwrap();
        let mut v = Vec::<NonNull<BufferHandle>>::new();
        while let Ok(hnd) = slablist.next() {
            v.push(hnd);
        }
        assert_eq!(v.len(), 16);
        v.drain(..).for_each(|mut hnd| unsafe {
            hnd.as_mut().recycler.as_mut().unwrap().send(hnd).unwrap();
        });
        assert_eq!(v.len(), 0);
        while let Ok(hnd) = slablist.next() {
            v.push(hnd);
        }
        assert_eq!(v.len(), 16);
    }
}
