/// Originally from the Monoio Project : https://github.com/bytedance/monoio   MIT License https://github.com/bytedance/monoio/blob/master/LICENSE-MIT
use std::future::Future;

use crate::buf::{IouBuf, IouBufVec};
use crate::io::{AsyncWriteRent, BufResult, BufVecResult};
/// AsyncWriteRentExt
pub trait AsyncWriteRentExt {
    /// The future of Result<size, buffer>
    type WriteExactFuture<'a>: Future<Output = BufResult<usize>>
    where
        Self: 'a;

    /// Write all
    fn write_all(&mut self, buf: IouBuf) -> Self::WriteExactFuture<'_>;

    /// The future of Result<size, buffer>
    type WriteVectoredExactFuture<'a>: Future<Output = BufVecResult<usize>>
    where
        Self: 'a;

    /// Write all
    fn writev_all(&mut self, bufs: IouBufVec) -> Self::WriteVectoredExactFuture<'_>;
}

impl<A> AsyncWriteRentExt for A
where
    A: AsyncWriteRent + ?Sized,
{
    type WriteExactFuture<'a> = impl Future<Output = BufResult<usize>> + 'a where A: 'a;

    fn write_all(&mut self, orig_buf: IouBuf) -> Self::WriteExactFuture<'_> {
        async move {
            let len = orig_buf.len();
            let mut written = 0;
            let mut buf = orig_buf.clone();
            while written < len {
                let buf_slice = buf.slice(written..);
                let (result, buf_slice) = self.write(buf_slice).await;
                buf = buf_slice;
                match result {
                    Ok(0) => {
                        return (
                            Err(std::io::Error::new(
                                std::io::ErrorKind::WriteZero,
                                "failed to write whole buffer",
                            )),
                            buf,
                        )
                    }
                    Ok(n) => written += n,
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return (Err(e), orig_buf),
                }
            }
            (Ok(written), buf)
        }
    }

    type WriteVectoredExactFuture<'a> = impl Future<Output = BufVecResult<usize>> + 'a where A: 'a;

    fn writev_all(&mut self, orig_bufs: IouBufVec) -> Self::WriteVectoredExactFuture<'_> {
        let len = orig_bufs.len();
        let mut written = 0;
        let mut bufs = orig_bufs.clone();
        async move {
            while written < len {
                let (res, bufs_) = self.writev(bufs).await;
                bufs = bufs_;
                match res {
                    Ok(0) => {
                        return (
                            Err(std::io::Error::new(
                                std::io::ErrorKind::WriteZero,
                                "failed to write whole buffer",
                            )),
                            bufs,
                        )
                    }
                    Ok(n) => {
                        written += n;
                        bufs.consume(n);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return (Err(e), bufs),
                }
            }
            (Ok(written), orig_bufs)
        }
    }
}
