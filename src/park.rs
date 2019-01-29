use std::fmt;
use std::io::ErrorKind;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cancel::Cancel;
use coroutine_impl::{co_cancel_data, run_coroutine, CoroutineImpl, EventSource};
use scheduler::get_scheduler;
use sync::AtomicOption;
use timeout_list::TimeoutHandle;
use yield_now::{get_co_para, yield_now, yield_with};

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ParkError {
    Canceled,
    Timeout,
}

pub struct DropGuard<'a>(&'a Park);
pub struct Park {
    // the coroutine that waiting for this park instance
    wait_co: Arc<AtomicOption<CoroutineImpl>>,
    // when odd means the Park no need to block
    // the low bit used as flag, and higher bits used as flag to check the kernel delay drop
    state: AtomicUsize,
    // control how to deal with the cancellation
    check_cancel: AtomicBool,
    // if cancel happened
    is_canceled: AtomicBool,
    // timeout settings in ms, 0 is none (park forever)
    timeout: AtomicUsize,
    // timer handle, can be null
    timeout_handle: AtomicPtr<TimeoutHandle<Arc<AtomicOption<CoroutineImpl>>>>,
    // a flag if kernel is entered
    wait_kernel: AtomicBool,
}

// this is the park resource type (spmc style)
impl Park {
    pub fn new() -> Self {
        Park {
            wait_co: Arc::new(AtomicOption::none()),
            state: AtomicUsize::new(0),
            check_cancel: true.into(),
            is_canceled: AtomicBool::new(false),
            timeout: AtomicUsize::new(0),
            timeout_handle: AtomicPtr::new(ptr::null_mut()),
            wait_kernel: AtomicBool::new(false),
        }
    }

    // ignore cancel, if true, caller have to do the check instead
    pub fn ignore_cancel(&self, ignore: bool) {
        self.check_cancel.store(!ignore, Ordering::Relaxed);
    }

    // set the timeout duration of the parking
    #[inline]
    fn set_timeout(&self, dur: Option<Duration>) -> Option<Duration> {
        let timeout = match dur {
            None => 0,
            Some(d) => d.as_millis() as usize,
        };

        match self.timeout.swap(timeout, Ordering::Relaxed) {
            0 => None,
            d => Some(Duration::from_millis(d as u64)),
        }
    }

    #[inline]
    fn set_timeout_handle(
        &self,
        handle: Option<TimeoutHandle<Arc<AtomicOption<CoroutineImpl>>>>,
    ) -> Option<TimeoutHandle<Arc<AtomicOption<CoroutineImpl>>>> {
        let ptr = match handle {
            None => ptr::null_mut(),
            Some(h) => h.into_ptr(),
        };

        let old_ptr = self.timeout_handle.swap(ptr, Ordering::Relaxed);
        if old_ptr.is_null() {
            None
        } else {
            unsafe { Some(TimeoutHandle::from_ptr(ptr)) }
        }
    }

    // return true if need park the coroutine
    // when the state is true, we clear it and indicate not to block
    // when the state is false, means we need real park
    #[inline]
    fn check_park(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        if state & 1 == 0 {
            return true;
        }

        loop {
            match self.state.compare_exchange_weak(
                state,
                state - 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return false, // successfully consume the state, no need to block
                Err(x) => {
                    if x & 1 == 0 {
                        return true;
                    }
                    state = x;
                }
            }
        }
    }

    // unpark the underlying coroutine if any
    #[inline]
    pub fn unpark(&self) {
        let mut state = self.state.load(Ordering::Acquire);
        if state & 1 == 1 {
            // the state is already set do nothing here
            return;
        }

        loop {
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return self.wake_up(false),
                Err(x) => {
                    if x & 1 == 1 {
                        break; // already set, do nothing
                    }
                    state = x;
                }
            }
        }
    }

    // remove the timeout handle after return back to user space
    #[inline]
    fn remove_timeout_handle(&self) {
        if let Some(h) = self.set_timeout_handle(None) {
            if h.is_link() {
                get_scheduler().del_timer(h);
            }
            // when timeout the node is unlinked
            // just drop it to release memory
        }
    }

    #[inline]
    fn wake_up(&self, b_sync: bool) {
        if let Some(co) = self.wait_co.take_fast(Ordering::Acquire) {
            if b_sync {
                run_coroutine(co);
            } else {
                get_scheduler().schedule(co);
            }
        }
    }

    /// park current coroutine with specified timeout
    /// if timeout happens, return Err(ParkError::Timeout)
    /// if cancellation detected, return Err(ParkError::Canceled)
    pub fn park_timeout(&self, dur: Option<Duration>) -> Result<(), ParkError> {
        self.set_timeout(dur);

        // if the state is not set, need to wait
        if !self.check_park() {
            return Ok(());
        }

        // before a new yield wait the kernel done
        if self.wait_kernel.swap(false, Ordering::AcqRel) {
            while self.state.load(Ordering::Acquire) & 0x02 == 0x02 {
                yield_now();
            }
        } else {
            // should clear the generation
            self.state.fetch_and(!0x02, Ordering::Relaxed);
        }

        // what if the state is set before yield?
        // the subscribe would re-check it
        yield_with(self);
        // clear the trigger state
        self.check_park();
        // remove timer handle
        self.remove_timeout_handle();

        // let _gen = self.state.load(Ordering::Acquire);
        // println!("unparked gen={}, self={:p}", gen, self);

        // after return back, we should check if it's timeout or canceled
        if self.is_canceled.load(Ordering::Relaxed) {
            return Err(ParkError::Canceled);
        }

        if let Some(err) = get_co_para() {
            match err.kind() {
                ErrorKind::TimedOut => return Err(ParkError::Timeout),
                ErrorKind::Other => return Err(ParkError::Canceled),
                _ => unreachable!("unexpected return error kind"),
            }
        }

        Ok(())
    }

    fn delay_drop(&self) -> DropGuard {
        self.wait_kernel.store(true, Ordering::Release);
        DropGuard(self)
    }
}

impl<'a> Drop for DropGuard<'a> {
    fn drop(&mut self) {
        // we would inc the state by 2 in kernel if all is done
        self.0.state.fetch_add(0x02, Ordering::Release);
    }
}

impl Drop for Park {
    fn drop(&mut self) {
        // wait the kernel finish
        if !self.wait_kernel.load(Ordering::Acquire) {
            return;
        }

        while self.state.load(Ordering::Acquire) & 0x02 == 0x02 {
            yield_now();
        }
        self.set_timeout_handle(None);
    }
}

impl EventSource for Park {
    // register the coroutine to the park
    fn subscribe(&mut self, co: CoroutineImpl) {
        let cancel = co_cancel_data(&co);
        // if we share the same park, the previous timer may wake up it by false
        // if we not deleted the timer in time
        let timeout_handle = self
            .set_timeout(None)
            .map(|dur| get_scheduler().add_timer(dur, self.wait_co.clone()));
        self.set_timeout_handle(timeout_handle);

        let _g = self.delay_drop();

        // register the coroutine
        self.wait_co.swap(co, Ordering::Release);

        // re-check the state, only clear once after resume
        if self.state.load(Ordering::Acquire) & 1 == 1 {
            // here may have recursive call for subscribe
            // normally the recursion depth is not too deep
            return self.wake_up(true);
        }

        // register the cancel data
        cancel.set_co(self.wait_co.clone());
        // re-check the cancel status
        if cancel.is_canceled() {
            unsafe { cancel.cancel() };
        }
    }

    // when the cancel is true we check the panic or do nothing
    fn yield_back(&self, cancel: &'static Cancel) {
        // we would inc the generation by 2 to another generation
        self.state.fetch_add(0x02, Ordering::Release);

        // should deal with cancel that happened just before the kernel
        self.is_canceled
            .store(cancel.is_canceled(), Ordering::Relaxed);

        if self.check_cancel.load(Ordering::Relaxed) {
            cancel.check_cancel();
        }
    }
}

impl fmt::Debug for Park {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Park {{ co: {:?}, state: {:?} }}",
            self.wait_co, self.state
        )
    }
}
