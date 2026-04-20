//! Bounded pool of recycled `Vec<u8>` buffers.
//!
//! The hot per-request paths (frame reads, response serialization)
//! allocate fresh `Vec<u8>` storage on every call. Each buffer outlives
//! a single request, so a pool of recycled allocations turns the
//! steady-state cost of "ask the allocator for N bytes" into "pop a
//! Vec and resize." Buffers above `max_buffer_capacity` are dropped
//! instead of returned, so a single oversize request can't bloat the
//! pool's resident memory long-term.
//!
//! The pool itself is bounded too — once it holds `pool_capacity`
//! buffers, additional drops free their storage. That keeps a burst of
//! concurrent requests from inflating memory permanently.

use std::ops::{Deref, DerefMut};

use parking_lot::Mutex;

pub struct BufferPool {
    inner: Mutex<Vec<Vec<u8>>>,
    pool_capacity: usize,
    max_buffer_capacity: usize,
}

impl BufferPool {
    pub fn new(pool_capacity: usize, max_buffer_capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Vec::with_capacity(pool_capacity)),
            pool_capacity,
            max_buffer_capacity,
        }
    }

    /// Borrow a buffer from the pool. The buffer is empty (`len == 0`)
    /// but may have non-zero capacity from a previous user. Returned
    /// to the pool on drop.
    pub fn acquire(&self) -> PooledBuffer<'_> {
        let buf = self.inner.lock().pop().unwrap_or_default();
        PooledBuffer {
            pool: self,
            buf: Some(buf),
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }
}

pub struct PooledBuffer<'a> {
    pool: &'a BufferPool,
    // `Option` so `Drop` can take ownership of the buffer to push it
    // back into the pool. Always `Some` for the duration of the borrow.
    buf: Option<Vec<u8>>,
}

impl Deref for PooledBuffer<'_> {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        self.buf.as_ref().expect("buffer present until drop")
    }
}

impl DerefMut for PooledBuffer<'_> {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        self.buf.as_mut().expect("buffer present until drop")
    }
}

impl Drop for PooledBuffer<'_> {
    fn drop(&mut self) {
        let mut buf = self.buf.take().expect("buffer present until drop");
        // Don't keep oversize buffers around — a stray 16 MiB request
        // would otherwise bloat the pool indefinitely.
        if buf.capacity() > self.pool.max_buffer_capacity {
            return;
        }
        buf.clear();
        let mut inner = self.pool.inner.lock();
        if inner.len() < self.pool.pool_capacity {
            inner.push(buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_yields_empty_buffer() {
        let pool = BufferPool::new(4, 1024);
        let buf = pool.acquire();
        assert!(buf.is_empty());
    }

    #[test]
    fn drop_returns_buffer_to_pool() {
        let pool = BufferPool::new(4, 1024);
        {
            let mut buf = pool.acquire();
            buf.extend_from_slice(b"hello");
        }
        // After drop the pool holds one buffer.
        assert_eq!(pool.len(), 1);
        // Re-acquire: empty (cleared) but capacity reused.
        let buf = pool.acquire();
        assert!(buf.is_empty());
        assert!(buf.capacity() >= 5);
    }

    #[test]
    fn pool_caps_recycled_buffers() {
        let pool = BufferPool::new(2, 1024);
        let bufs: Vec<_> = (0..5).map(|_| pool.acquire()).collect();
        drop(bufs);
        // Only `pool_capacity` buffers are kept; the rest are freed.
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn oversize_buffers_are_dropped_not_recycled() {
        let pool = BufferPool::new(4, 16);
        {
            let mut buf = pool.acquire();
            buf.resize(32, 0); // exceeds max_buffer_capacity
        }
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn small_buffers_survive_recycling() {
        let pool = BufferPool::new(4, 1024);
        for _ in 0..10 {
            let mut buf = pool.acquire();
            buf.resize(64, 0);
        }
        // Steady state: pool holds at most one entry because each
        // acquire pops, fills, and drops in turn.
        assert!(pool.len() <= 4);
        assert!(pool.len() >= 1);
    }
}
