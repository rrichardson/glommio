use std::ops::Deref;

use super::IouBuf;
use smallvec::SmallVec;

/// A Vec of IouBufs
/// This mostly exists to serve the `writev` and multishot read event use-cases
#[derive(Debug, Clone)]
pub struct IouBufVec {
    bufs: SmallVec<[IouBuf; 4]>,
    io_vecs: SmallVec<[libc::iovec; 4]>,
}

impl IouBufVec {
    /// Push an IouBuf to the end of this vec
    pub fn push(&mut self, buf: IouBuf) {
        let sl: &[u8] = buf.deref();
        let ptr = sl.as_ptr() as *mut u8;
        let len = sl.len();
        self.bufs.push(buf);
        self.io_vecs.push(libc::iovec {
            iov_base: ptr as *mut libc::c_void,
            iov_len: len as libc::size_t,
        })
    }

    /// Return the collective length of all buffers in this vec
    pub fn len(&self) -> usize {
        self.bufs.iter().map(|b| b.len()).sum()
    }

    /// Return a bool representing the presence of data in the vecs
    pub fn is_empty(&self) -> bool {
        self.bufs.len() > 0 && self.len() > 0
    }

    /// "remove" from the front of the vec the amount processed.
    /// This includes popping completely consumed bufs from the
    /// front of the vec, and pushing the slice offset up for the remaining
    pub fn consume(&mut self, mut amt: usize) {
        while let Some(buf) = self.bufs.first().cloned() {
            if amt >= buf.len() {
                self.bufs.remove(0);
                amt -= buf.len();
            } else {
                self.bufs[0] = buf.slice(amt..);
                return;
            }
        }
    }
}

impl Extend<IouBuf> for IouBufVec {
    fn extend<T: IntoIterator<Item = IouBuf>>(&mut self, iter: T) {
        for elem in iter {
            self.push(elem);
        }
    }
}

impl Default for IouBufVec {
    fn default() -> IouBufVec {
        IouBufVec {
            bufs: SmallVec::new(),
            io_vecs: SmallVec::new(),
        }
    }
}
