//! `IouBuf` is a `Clone`, `Send` `Sync` reference counted buffer that supports sub-slicing to make
//! it convenient to reference serialized structs contained within the buffer.
//! `IouBuf` is meant to work with `BufferPool` which is a thread-local free-list/recycler of
//! backing-buffers of multiple sizes.
//!
//! The IouBuf and the BufferPool are intended to work with `io_uring`. Therefore, every buffer
//! that they allocate are guaranteed to be aligned to page (4kb) boundaries.
//!
//! # Example
//!
//! ```rust
//! /// Allocates a buffer into the thread-local buffer pool
//! /// Presumably the actual allocated buffer will be some larger power of two, like 4kb
//! use prelude::IouBuf;
//! let buf = IouBuf::alloc(444).unwrap();
//! let mut subset = buf.into_slice(..5);
//! {
//!     let mut writable = subset.get_mut().unwrap();
//!     writable.fill(42);
//! }
//! /// Return the scope back to the
//! let root = subset.into_root();
//! let mut subset = root.into_slice(5..10);
//! {
//!     let mut writable = subset.get_mut().unwrap();
//!     writable.fill(84);
//! }
//! let new_subset = subset.root().slice(0..10);
//! assert_eq!(&*new_subset, &[42,42,42,42,42,84,84,84,84,84]);
//!
//! ```
//!
//! [`io_uring`]: https://en.wikipedia.org/wiki/Io_uring
#![allow(clippy::len_without_is_empty)]
use std::{
    borrow::Borrow,
    fmt::Debug,
    hash::{Hash, Hasher},
    io::{Cursor, Write},
    mem::ManuallyDrop,
    ops::{Deref, Range},
    ptr::NonNull,
    sync::atomic::Ordering,
};

use bytes::{Buf, Bytes};

use super::iou_buf_pool::{AllocError, BufferHandle, BUFPOOL};

/// `IouBuf` is a `Clone`, `Send` `Sync` reference counted buffer that supports sub-slicing to make
/// it convenient to reference serialized structs contained within the buffer.
/// `IouBuf` is meant to work with `BufferPool` which is a thread-local free-list/recycler of
/// backing-buffers of multiple sizes.
#[derive(Eq)]
pub struct IouBuf {
    inner: NonNull<BufferHandle>,
    slice: NonNull<[u8]>,
}

impl IouBuf {
    /// Create a fresh IouBuf with a reset BufferHandle backed by the Thread-Local Buffer Pool
    pub fn alloc(size: usize) -> Result<Self, AllocError> {
        if size == 0 {
            Ok(Self::empty())
        } else {
            BUFPOOL.with(|bp| {
                let hnd = bp.next_buffer(size)?;
                unsafe { hnd.as_ref().refcount.store(1, Ordering::Release) };
                Ok(Self::from_handle(hnd, size))
            })
        }
    }

    /// Create a fresh IouBuf with a reset BufferHandle explicitly zeroed out and backed by the Thread-Local Buffer Pool
    pub fn alloc_zeroed(size: usize) -> Result<Self, AllocError> {
        let this = Self::alloc(size)?;
        // Safety: we just allocated this so there can be no other references
        unsafe {
            std::ptr::write_bytes(this.inner.as_ref().buf.as_mut_ptr(), 0u8, this.capacity())
        };
        Ok(this)
    }

    /// Construct a specialized form of IouBuf that is empty
    /// It will require an allocation if it is going to change into a non-empty IouBuf
    pub fn empty() -> Self {
        let hnd = unsafe {
            let buf = BufferHandle::empty();
            buf.as_ref().refcount.store(1, Ordering::Release);
            buf
        };
        Self::from_handle(hnd, 0)
    }

    /// Returns true if this buffer is empty
    pub fn is_empty(&self) -> bool {
        self.slice.len() == 0
    }

    /// Create a new instance of IouBuf from an already "allocated" BufferHandle or IouBuf
    /// This is typically used in the `clone` operation
    fn from_handle(handle: NonNull<BufferHandle>, len: usize) -> Self {
        let buf = unsafe { handle.as_ref().buf.as_ref() };
        let slice = unsafe { NonNull::new_unchecked(&buf[..len] as *const [u8] as *mut [u8]) };
        IouBuf {
            inner: handle,
            slice,
        }
    }

    /// Allocate a new IouBuf and copy the contents of this slice into it
    pub fn deep_clone(&self) -> Result<Self, AllocError> {
        let size = self.slice.len();
        let mut buf = BUFPOOL.with(|bp| {
            let hnd = bp.next_buffer(size)?;
            unsafe { hnd.as_ref().refcount.store(1, Ordering::Release) };
            Ok::<IouBuf, AllocError>(IouBuf::from_handle(hnd, size))
        })?;
        unsafe { buf.slice.as_mut().copy_from_slice(self.slice.as_ref()) };
        Ok(buf)
    }

    /// Copy contents of other into this buffer and size it to that slice
    pub fn from_slice(src: &[u8]) -> Result<Self, AllocError> {
        BUFPOOL.with(|bp| {
            let len = src.len();
            let mut hnd = bp.next_buffer(len)?;
            unsafe { hnd.as_ref().refcount.store(1, Ordering::Release) };
            let buf = unsafe { hnd.as_mut().buf.as_mut() };
            buf[0..len].copy_from_slice(src);
            let slice = unsafe { NonNull::new_unchecked(&buf[..len] as *const [u8] as *mut [u8]) };
            Ok(IouBuf { inner: hnd, slice })
        })
    }

    /// Reset this buffer's slice to point to the entirety of the backing buffer
    pub fn into_root(self) -> Self {
        let this = ManuallyDrop::new(self);
        let slice = unsafe { this.inner.as_ref().buf };
        let inner = this.inner;
        IouBuf { inner, slice }
    }

    /// Create a clone of this IouBuf, but reset the slice to point back to the entire buffer
    pub fn root(&self) -> Self {
        let slice = unsafe {
            self.inner.as_ref().increment();
            self.inner.as_ref().buf
        };
        IouBuf {
            inner: self.inner,
            slice,
        }
    }

    /// Get the length of the slice pointed to by this IouBuf (which may be smaller than than the
    /// backing buffer)
    pub fn len(&self) -> usize {
        self.deref().len()
    }

    /// Get the length of the underlying buffer.
    pub fn capacity(&self) -> usize {
        unsafe { self.inner.as_ref().buf.len() }
    }

    /// Construct a new IouBuf of size `size` filling the contents with `value`
    pub fn fill(value: u8, size: usize) -> Result<Self, AllocError> {
        let mut buf = Self::alloc(size)?;
        {
            //unwrap is safe because we are sure there are no other copies of this buf
            let bref = unsafe { IouBuf::get_mut_unchecked(&mut buf) };
            // funny enough this is the most efficient approach, as this pattern is detected by the compiler
            // and changed to memset if needed
            for i in &mut bref[..] {
                *i = value
            }
        }
        Ok(buf)
    }

    /// Forces the length to a specific amount
    ///
    /// # Safety
    ///
    /// This panics if the length is greater than the capacity of the inner buf.  
    #[inline]
    pub unsafe fn resize(&mut self, newlen: usize) {
        if newlen > self.capacity() {
            panic!(
                "New length of {} would be greater than capacity: {}",
                newlen,
                self.capacity()
            );
        }
        if !self.is_unique() {
            panic!("This buffer is not unique, so it can't be resized");
        }
        let slice = unsafe {
            let ptr = self.slice.as_mut().as_mut_ptr();
            let sl = std::slice::from_raw_parts(ptr, newlen);
            NonNull::new_unchecked(sl as *const [u8] as *mut [u8])
        };
        self.slice = slice;
    }
    /// Return this buffer (make no copies) but restrict the buffer's effective slice
    /// to that supplied by the `range` parameter.
    pub fn into_slice<R>(self, range: R) -> IouBuf
    where
        R: std::slice::SliceIndex<[u8], Output = [u8]>,
    {
        let this = ManuallyDrop::new(self);
        let buf: &[u8] = this.deref();
        // I consider a cast from const to mut in order to do new_unchecked much safer than using
        // NonNull::from which is happy to do horrible things
        let slice = unsafe { NonNull::new_unchecked(&buf[range] as *const [u8] as *mut [u8]) };
        IouBuf {
            inner: this.inner,
            slice,
        }
    }

    /// Return this buffer (make no copies) but restrict the buffer's effective slice
    /// to that supplied by the `ptr_range` parameter.
    /// If the range provided is not a subset, return None
    pub fn into_slice_from_ptr_range(self, ptr_range: Range<*const u8>) -> Option<IouBuf> {
        let buf_range = self.deref().as_ptr_range();
        if buf_range.contains(&ptr_range.start) && buf_range.contains(&ptr_range.end) {
            Some(unsafe { self.into_slice_from_ptr_range_unchecked(ptr_range) })
        } else {
            None
        }
    }

    /// Unsafe variant of `into_slice_from_ptr_range` which assumes the range is a subset
    ///
    /// # Safety
    ///
    /// Caller must be certain the `ptr_range` is a subset of the current slice
    #[inline]
    pub unsafe fn into_slice_from_ptr_range_unchecked(self, ptr_range: Range<*const u8>) -> IouBuf {
        let this = ManuallyDrop::new(self);
        let buf: &[u8] = this.deref();
        // I consider a cast from const to mut in order to do new_unchecked much safer than using
        // NonNull::from which is happy to do horrible things
        let slice = unsafe {
            let buf_range_start = buf.as_ptr();
            let start = ptr_range.start.sub_ptr(buf_range_start);
            let end = ptr_range.end.sub_ptr(buf_range_start);
            NonNull::new_unchecked(&buf[start..end] as *const [u8] as *mut [u8])
        };
        IouBuf {
            inner: this.inner,
            slice,
        }
    }

    /// Return a clone of this buffer, with the slice pointing to a subset of the backing buffer
    /// supplied by the `range` parameter
    ///
    pub fn slice<R>(&self, range: R) -> IouBuf
    where
        R: std::slice::SliceIndex<[u8], Output = [u8]>,
    {
        let buf: &[u8] = self.deref();
        // I consider a cast from const to mut in order to do new_unchecked much safer than using
        // NonNull::from which is happy to do horrible things
        let slice = unsafe {
            self.inner.as_ref().increment();
            NonNull::new_unchecked(&buf[range] as *const [u8] as *mut [u8])
        };
        IouBuf {
            inner: self.inner,
            slice,
        }
    }

    /// Create a slice of the current `IouBuf` slice utilizing the supplied closure
    pub fn slice_fn<F>(&self, closure: F) -> IouBuf
    where
        F: FnOnce(&[u8]) -> &[u8],
    {
        // I consider a cast from const to mut in order to do new_unchecked much safer than using
        // NonNull::from which is happy to do horrible things
        let slice =
            unsafe { NonNull::new_unchecked(closure(self.deref()) as *const [u8] as *mut [u8]) };
        let next = self.clone();
        IouBuf {
            inner: next.inner,
            slice,
        }
    }

    /// Determine whether this is the unique reference (including weak refs) to
    /// the underlying data.
    #[inline]
    fn is_unique(&self) -> bool {
        let count = self.inner().refcount.load(Ordering::Acquire);
        count == 1
    }

    /// Get a ref to the inner BufferHandle
    #[inline]
    fn inner(&self) -> &BufferHandle {
        unsafe { self.inner.as_ref() }
    }

    /// Returns a mutable reference into the given `IouBuf`, if there are
    /// no other `IouBuf` pointers to the same allocation.
    #[inline]
    pub fn get_mut(&mut self) -> Option<&mut [u8]> {
        if self.is_unique() {
            // This unsafety is ok because we're guaranteed that the pointer
            // returned is the *only* pointer that will ever be returned to T. Our
            // reference count is guaranteed to be 1 at this point, and we required
            // the IouBuf itself to be `mut`, so we're returning the only possible
            // reference to the inner data.
            unsafe { Some(Self::get_mut_unchecked(self)) }
        } else {
            None
        }
    }

    /// Returns a mutable reference into the given `IouBuf`,
    /// without any check.
    /// # Safety
    /// Be sure that there are no references holding onto this allocation before calling this
    /// ```
    #[inline]
    pub unsafe fn get_mut_unchecked(this: &mut Self) -> &mut [u8] {
        this.slice.as_mut()
    }

    /// Interpret this slice as a str
    pub fn as_str(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(unsafe { self.slice.as_ref() })
    }

    #[inline]
    /// Append a slice into the end of this "active" slice
    ///
    /// As long as:
    /// 1. This Ioubuf is unique (its refcount is 1)
    /// 2. There is room in the backing buffer in the BufferHandle
    ///
    /// This will coopy the src slice to the end of this slice, or as much as it can
    /// and then return the amount appended
    ///
    /// NOTE: This could overwrite previously written data if the slice member
    /// has a range that is smaller than the containing (inner) slice. But since
    /// this operation can only be performed on a unique IouBuf, we assume you know what
    /// you're doing.
    pub fn append_slice(&mut self, src: &[u8]) -> Result<usize, AllocError> {
        if self.is_unique() {
            unsafe {
                if self.inner().buf.as_ref().len() < self.len() + src.len() {
                    return Err(AllocError::InvalidBufferSize(src.len()));
                }
                let buf = self.inner.as_mut().buf.as_mut();
                let slice = self.slice.as_ref();
                let slice_offset = slice.as_ptr() as usize - buf.as_ref().as_ptr() as usize;
                let slice_offset_end = slice_offset + slice.len();
                let remaining = buf.len() - slice_offset_end;
                let amt = std::cmp::min(src.len(), remaining);
                buf[slice_offset_end..(slice_offset_end + amt)].copy_from_slice(&src[..amt]);
                let newslice = NonNull::new_unchecked(
                    &buf[slice_offset..(slice_offset_end + amt)] as *const [u8] as *mut [u8],
                );
                self.slice = newslice;
                Ok(amt)
            }
        } else {
            Err(AllocError::NotUnique)
        }
    }

    /// Convert this `IouBuf` into an `IouBufCursor` so that it can implement the `Buf` trait
    #[inline]
    pub fn into_cursor(self) -> IouBufCursor {
        IouBufCursor::new(self)
    }
}

// Safety: in safe code, you cannot get a mutable reference to the slice unless you hold the unique reference
unsafe impl Send for IouBuf {}
unsafe impl Sync for IouBuf {}

/// Convert Bytes to IouBuf
impl From<Bytes> for IouBuf {
    /// `Bytes` has a couple different states, and we can't know for sure which is which
    /// on the outside.  If it's in its "normal" refcounted mode, it is basically an Arc<Vec>
    /// which means it will convert to a Vec without alloc/copy.
    /// IouBuf can also convert from Vec to IouBuf without alloc/copy, so using these high-level
    /// conversion primitives gives us a high probability of 0 alloc/copy without any risk
    /// of catastrophic failure
    fn from(b: Bytes) -> Self {
        let v: Vec<u8> = b.into();
        v.into()
    }
}

impl From<Vec<u8>> for IouBuf {
    fn from(v: Vec<u8>) -> Self {
        let len = v.len();
        let inner = unsafe { BufferHandle::from_vec(v) };
        let slice = unsafe {
            NonNull::new_unchecked(
                (&(inner.as_ref()).buf.as_ref()[..len] as *const [u8]).cast_mut(),
            )
        };
        IouBuf { inner, slice }
    }
}

impl Hash for IouBuf {
    fn hash<H: Hasher>(&self, state: &mut H) {
        unsafe {
            let sl = self.slice.as_ref();
            sl.hash(state)
        }
    }
}

impl TryFrom<&[u8]> for IouBuf {
    type Error = AllocError;
    fn try_from(v: &[u8]) -> Result<IouBuf, Self::Error> {
        let buf = IouBuf::alloc(v.len())?;
        let mut sl = buf.into_slice(0..v.len());
        let bref = sl.get_mut().unwrap();
        bref.copy_from_slice(v);
        Ok(sl)
    }
}

impl Write for IouBuf {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        match self.append_slice(buf) {
            Ok(amt) => Ok(amt),
            Err(e) => Err(e.into()),
        }
    }

    fn flush(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

impl Borrow<[u8]> for IouBuf {
    fn borrow(&self) -> &[u8] {
        self.deref()
    }
}

impl Clone for IouBuf {
    #[inline]
    fn clone(&self) -> Self {
        unsafe { self.inner.as_ref().increment() };
        IouBuf {
            inner: self.inner,
            slice: self.slice,
        }
    }
}

impl Drop for IouBuf {
    /// Drops the `IouBuf`.
    ///
    /// This will decrement the reference count within the BufferHandle. If the reference
    /// count is 0, then recycle the `BufferHandle`
    ///
    #[inline]
    fn drop(&mut self) {
        BufferHandle::decrement(self.inner);
    }
}

impl Deref for IouBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { self.slice.as_ref() }
    }
}

impl PartialEq<IouBuf> for IouBuf {
    fn eq(&self, other: &IouBuf) -> bool {
        other.deref() == self.deref()
    }
}

impl Debug for IouBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("Point").field("len", &self.len()).finish()
    }
}

/*
fn subslice_offset(outer: &[u8], inner: &[u8]) -> Option<usize> {
    let outer_ptr = outer.as_ptr() as usize;
    let inner_ptr = inner.as_ptr() as usize;
    if inner_ptr < outer_ptr || inner_ptr > outer_ptr.wrapping_add(outer.len()) {
        None
    } else {
        Some(inner_ptr.wrapping_sub(outer_ptr))
    }
}
*/

/// Wraps an ['IouBuf'] in a ['Cursor'] in order to make it easily implement ['Buf']
///
/// This is a newtype for a traditional std::io::Cursor that has been wrapped around an IouBuf
/// in order to make this usable for things which take `Buf`.
#[derive(Debug)]
pub struct IouBufCursor {
    /// The Cursor
    pub cur: Cursor<IouBuf>,
}

impl IouBufCursor {
    /// Constructs an `IouBufCursor` from an `IouBuf`
    pub fn new(buf: IouBuf) -> Self {
        Self {
            cur: Cursor::new(buf),
        }
    }

    /// Return the amount of data remaining in this cursor
    pub fn remaining(&self) -> usize {
        let len = self.cur.get_ref().as_ref().len();
        let pos = self.cur.position();

        if pos >= len as u64 {
            return 0;
        }

        len - pos as usize
    }
}

impl Buf for IouBufCursor {
    fn remaining(&self) -> usize {
        self.remaining()
    }

    fn chunk(&self) -> &[u8] {
        let len = self.cur.get_ref().as_ref().len();
        let pos = self.cur.position();

        if pos >= len as u64 {
            return &[];
        }

        &self.cur.get_ref().as_ref()[pos as usize..]
    }

    fn advance(&mut self, cnt: usize) {
        let pos = (self.cur.position() as usize)
            .checked_add(cnt)
            .expect("overflow");

        assert!(pos <= self.cur.get_ref().as_ref().len());
        self.cur.set_position(pos as u64);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn buf_basics() {
        let buf = IouBuf::alloc(444).unwrap();
        let mut subset = buf.into_slice(..5);
        {
            let writable = subset.get_mut().unwrap();
            writable.fill(42);
        }
        // Return the scope back to the root
        let root = subset.into_root();
        let mut subset = root.into_slice(5..10);
        {
            let writable = subset.get_mut().unwrap();
            writable.fill(84);
        }
        let new_subset = subset.root().slice(0..10);
        assert_eq!(&*new_subset, &[42, 42, 42, 42, 42, 84, 84, 84, 84, 84]);
    }

    #[test]
    fn vec_vec_buffs() {
        let mut v: Vec<Vec<IouBuf>> = vec![];
        for _ in 0..10 {
            let v1 = (0..4).map(|_| IouBuf::alloc(444).unwrap()).collect();
            v.push(v1);
        }
        for i in v.iter() {
            for j in i.iter() {
                assert_eq!(j.len(), 444);
                assert_eq!(j.capacity(), 4096); // nearest power of 2 slabs
            }
        }
    }

    #[test]
    fn vec_vec_from_slice() {
        let mut v: Vec<Vec<IouBuf>> = vec![];
        for _ in 0..10 {
            let v1 = (0..4)
                .map(|_| IouBuf::try_from(&b"Wat Wat Wat"[..]).unwrap())
                .collect();
            v.push(v1);
        }
        for i in v.iter() {
            for j in i.iter() {
                assert_eq!(j.len(), 11);
                assert_eq!(j.capacity(), 4096);
            }
        }
    }

    #[test]
    fn vec_vec_from_vec() {
        let mut v: Vec<Vec<IouBuf>> = vec![];
        for _ in 0..10 {
            let v1 = (0..4)
                .map(|_| {
                    let vc = b"Wat Wat Wat".to_vec();
                    IouBuf::from(vc)
                })
                .collect();
            v.push(v1);
        }
        for i in v.iter() {
            for j in i.iter() {
                assert_eq!(j.len(), 11);
                assert_eq!(j.capacity(), 11);
            }
        }
    }

    #[test]
    fn vec_vec_chunks() {
        let strip_count = 4;
        let chunk_size = 4096;
        let strip_size = 4096 * 4;

        let mut strips = Vec::with_capacity(strip_count);
        for _strip_id in 0..strip_count {
            let stride = strip_size / chunk_size;
            let mut strip = Vec::with_capacity(stride as usize);
            let mut buf_pattern: u8 = 65u8;
            for chunk_num in 0..stride {
                let reserve = (chunk_size - 3) as usize;
                let buf = if chunk_num == 0 {
                    vec![vec![91u8; 3], vec![buf_pattern; reserve]]
                        .into_iter()
                        .flatten()
                        .collect()
                } else if chunk_num == stride - 1 {
                    vec![vec![buf_pattern; reserve], vec![93u8; 3]]
                        .into_iter()
                        .flatten()
                        .collect()
                } else {
                    vec![buf_pattern; chunk_size as usize]
                };
                strip.push(IouBuf::from(buf));
                buf_pattern += 1;
                buf_pattern %= 90u8;
            }
            strips.push(strip);
        }

        for i in strips.clone().iter() {
            for j in i.iter() {
                //println!("ioubuf len = {}", j.len());
                assert_eq!(j.len(), 4096);
            }
        }
    }

    #[test]
    fn bytes_buf_roundtrip() {
        let buf = IouBuf::alloc(444).unwrap();
        let mut subset = buf.into_slice(..5);
        {
            let writable = subset.get_mut().unwrap();
            writable.fill(42);
        }
        // Return the scope back to the root
        let root = subset.into_root();
        let mut subset = root.into_slice(5..10);
        {
            let writable = subset.get_mut().unwrap();
            writable.fill(84);
        }
        let new_subset = subset.root().slice(0..10);
        assert_eq!(&*new_subset, &[42, 42, 42, 42, 42, 84, 84, 84, 84, 84]);
    }

    #[test]
    fn buf_is_cursor() {
        fn read_buf<B: Buf>(mut buf: B) -> bool {
            assert_eq!(b'h', buf.get_u8());
            assert_eq!(b'e', buf.get_u8());
            assert_eq!(b'l', buf.get_u8());

            let mut rest = [0; 8];
            buf.copy_to_slice(&mut rest);

            assert_eq!(&rest[..], &b"lo world"[..]);
            true
        }

        let iou = IouBuf::try_from(&b"hello world"[..]).unwrap();
        let cur = iou.into_cursor();
        read_buf(cur);
    }

    #[test]
    fn append_buf() {
        let mut buf1 = IouBuf::from_slice(b"1234").unwrap();
        buf1.append_slice(b"567").unwrap();
        let d = IouBuf::from_slice(b"1234567").unwrap();
        assert_eq!(buf1, d);

        let mut buf3 = {
            let tmpbuf = IouBuf::from_slice(b"0123456789").unwrap();
            tmpbuf.slice(2..7)
        };
        buf3.append_slice(b"abc").unwrap();
        let d = IouBuf::from_slice(b"23456abc").unwrap();
        println!("{}", buf3.as_str().unwrap());
        assert_eq!(buf3, d);
    }

    #[test]
    fn write_buf() {
        use std::io::{BufWriter, Write};
        let buf1 = IouBuf::from_slice(b"").unwrap();
        let mut stream = BufWriter::new(buf1);
        let chars: Vec<u8> = (0..10).map(|i| i + 48).collect();
        for i in 0..chars.len() {
            stream.write_all(&chars[i..(i + 1)]).unwrap();
        }
        stream.flush().unwrap();
        let buf1 = stream.into_inner().unwrap();
        assert_eq!(buf1.as_str().unwrap(), "0123456789");
    }

    #[test]
    fn empty() {
        let buf1 = IouBuf::alloc(0).unwrap();
        let buf2 = IouBuf::empty();
        unsafe {
            assert_eq!(
                buf1.inner.as_ref().buf.as_ref().as_ptr(),
                buf2.inner.as_ref().buf.as_ref().as_ptr()
            );
        }
        assert_eq!(buf1.len(), 0);
        assert_eq!(buf2.capacity(), 0);
    }
}
