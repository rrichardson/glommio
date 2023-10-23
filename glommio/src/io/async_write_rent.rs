/// Originally from the Monoio Project : https://github.com/bytedance/monoio   MIT License https://github.com/bytedance/monoio/blob/master/LICENSE-MIT
use std::future::Future;

use crate::buf::{IouBuf, IouBufVec};
use crate::io::{BufResult, BufVecResult};

/// AsyncWriteRent: async write with a ownership of a buffer
pub trait AsyncWriteRent {
    /// The future of write Result<size, buffer>
    type WriteFuture<'a>: Future<Output = BufResult<usize>>
    where
        Self: 'a;
    /// The future of writev Result<size, buffer>
    type WritevFuture<'a>: Future<Output = BufVecResult<usize>>
    where
        Self: 'a;

    /// The future of flush
    type FlushFuture<'a>: Future<Output = std::io::Result<()>>
    where
        Self: 'a;

    /// The future of shutdown
    type ShutdownFuture<'a>: Future<Output = std::io::Result<()>>
    where
        Self: 'a;

    /// Same as write(2)
    fn write(&mut self, buf: IouBuf) -> Self::WriteFuture<'_>;

    /// Same as writev(2)
    fn writev(&mut self, buf_vec: IouBufVec) -> Self::WritevFuture<'_>;

    /// Flush buffered data if needed
    fn flush(&mut self) -> Self::FlushFuture<'_>;

    /// Same as shutdown
    fn shutdown(&mut self) -> Self::ShutdownFuture<'_>;
}

/// AsyncWriteRentAt: async write with a ownership of a buffer and a position
pub trait AsyncWriteRentAt {
    /// The future of Result<size, buffer>
    type Future<'a, T>: Future<Output = BufResult<usize>>
    where
        Self: 'a,
        T: 'a;

    /// Write buf at given offset
    fn write_at(&self, buf: IouBuf, pos: usize) -> Self::Future<'_, IouBuf>;
}

impl<A: ?Sized + AsyncWriteRent> AsyncWriteRent for &mut A {
    type WriteFuture<'a> = A::WriteFuture<'a>
    where
        Self: 'a;

    type WritevFuture<'a> = A::WritevFuture<'a>
    where
        Self: 'a;

    type FlushFuture<'a> = A::FlushFuture<'a>
    where
        Self: 'a;

    type ShutdownFuture<'a> = A::ShutdownFuture<'a>
    where
        Self: 'a;

    #[inline]
    fn write(&mut self, buf: IouBuf) -> Self::WriteFuture<'_> {
        (**self).write(buf)
    }

    #[inline]
    fn writev(&mut self, buf_vec: IouBufVec) -> Self::WritevFuture<'_> {
        (**self).writev(buf_vec)
    }

    #[inline]
    fn flush(&mut self) -> Self::FlushFuture<'_> {
        (**self).flush()
    }

    #[inline]
    fn shutdown(&mut self) -> Self::ShutdownFuture<'_> {
        (**self).shutdown()
    }
}
