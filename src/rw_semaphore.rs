use super::{Mutex, MutexGuard};

use alloc::{collections::VecDeque, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::{
    future::Future,
    pin::Pin,
    result::Result,
    task::{Context, Poll, Waker},
};

use super::{pop_off, push_off};

const READER: usize = 1 << 2;
// const UPGRADED: usize = 1 << 1;
const WRITER: usize = 1;

type AcquireResult = Result<(), ()>;

pub(crate) struct RwSemaphore {
    permit: AtomicUsize,
    waiters: Mutex<VecDeque<Arc<Waiter>>>,
    _closed: bool,
}

impl RwSemaphore {
    pub fn new() -> Self {
        Self {
            permit: AtomicUsize::new(0),
            waiters: Mutex::new(VecDeque::new()),
            _closed: false,
        }
    }

    pub fn acquire_read(&self) -> AcquireFuture<'_> {
        AcquireFuture {
            semaphore: self,
            node: Arc::new(Waiter::new(AcquireType::Read)),
        }
    }

    pub fn acquire_write(&self) -> AcquireFuture<'_> {
        AcquireFuture {
            semaphore: self,
            node: Arc::new(Waiter::new(AcquireType::Write)),
        }
    }

    pub fn try_acquire_read(&self) -> AcquireResult {
        push_off();
        let value = self.permit.fetch_add(READER, Ordering::Acquire);
        if (value & WRITER) != 0 {
            self.permit.fetch_sub(READER, Ordering::Release);
            pop_off();
            Err(())
        } else {
            Ok(())
        }
    }

    pub fn try_acquire_write(&self) -> AcquireResult {
        push_off();
        if self
            .permit
            .compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Ok(())
        } else {
            pop_off();
            Err(())
        }
    }

    fn poll_acquire(&self, node: &Arc<Waiter>) -> AcquireResult {
        let mut waiters = self.waiters.lock();
        let req = node.req;
        let res = if req == AcquireType::Read {
            self.try_acquire_read()
        } else {
            self.try_acquire_write()
        };
        if res.is_err() {
            if node
                .queued
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                waiters.push_back(node.clone());
            }
        };
        res
    }

    pub fn release_read(&self) {
        let mut waiters = self.waiters.lock();
        let old = self.permit.fetch_sub(READER, Ordering::Release);
        if old == READER {
            Self::wake_next(&mut waiters);
        }
        pop_off();
    }

    pub fn release_write(&self) {
        let mut waiters = self.waiters.lock();
        self.permit.fetch_and(!WRITER, Ordering::Release);
        Self::wake_next(&mut waiters);
        pop_off();
    }

    fn wake_next(waiters: &mut MutexGuard<VecDeque<Arc<Waiter>>>) {
        if !waiters.is_empty() {
            let waiter = waiters.pop_front().unwrap();
            waiter.wake();
            if waiter.req == AcquireType::Read {
                waiters.retain(|waiter| {
                    if waiter.req == AcquireType::Read {
                        waiter.wake();
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }

    pub fn reader_count(&self) -> usize {
        let state = self.permit.load(Ordering::Relaxed);
        state / READER
    }

    pub fn writer_count(&self) -> usize {
        (self.permit.load(Ordering::Relaxed) & WRITER) / WRITER
    }

    pub fn get_permit(&self) -> usize {
        self.permit.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcquireType {
    Read = 0,
    Write,
}

pub(crate) struct AcquireFuture<'a> {
    semaphore: &'a RwSemaphore,
    node: Arc<Waiter>,
}

impl Future for AcquireFuture<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.node.waker.is_none() {
            let waiter = unsafe { Arc::<Waiter>::get_mut_unchecked(&mut self.node) };
            waiter.waker = Some(cx.waker().clone());
        }
        assert!(cx.waker().will_wake(self.node.waker.as_ref().unwrap()));
        match self.semaphore.poll_acquire(&self.node) {
            Ok(_) => Poll::Ready(()),
            Err(_) => Poll::Pending,
        }
    }
}

pub struct Waiter {
    req: AcquireType,
    waker: Option<Waker>,
    queued: AtomicBool,
}

impl Waiter {
    const fn new(req: AcquireType) -> Self {
        Self {
            req,
            waker: None,
            queued: AtomicBool::new(false),
        }
    }

    pub fn wake(&self) {
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        } else {
            panic!("waiter with None `waker` was enqueued");
        }
    }
}
