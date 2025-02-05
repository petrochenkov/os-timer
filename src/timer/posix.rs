use core::{ptr, time, mem};
use core::cell::Cell;
use core::sync::atomic::{AtomicUsize, Ordering};
use super::BoxFnPtr;

extern crate alloc;
use alloc::boxed::Box;

mod ffi {
    use core::mem;
    pub use libc::c_void;
    #[allow(non_camel_case_types)]
    pub type timer_t = usize;

    pub type Callback = unsafe extern "C" fn(libc::sigval);

    pub unsafe extern "C" fn timer_callback(value: libc::sigval) {
        if !value.sival_ptr.is_null() {
            let cb: fn() -> () = mem::transmute(value.sival_ptr);

            (cb)();
        }
    }

    pub unsafe extern "C" fn timer_callback_unsafe(value: libc::sigval) {
        if !value.sival_ptr.is_null() {
            let cb: unsafe fn() -> () = mem::transmute(value.sival_ptr);

            (cb)();
        }
    }

    pub unsafe extern "C" fn timer_callback_generic<T: FnMut() -> ()>(value: libc::sigval) {
        if !value.sival_ptr.is_null() {
            let cb = &mut *(value.sival_ptr as *mut T);

            (cb)();
        }
    }

    #[repr(C)]
    #[derive(PartialEq)]
    pub struct timespec {
        pub tv_sec: libc::time_t,
        pub tv_nsec: libc::c_long,
    }

    #[repr(C)]
    #[derive(PartialEq)]
    pub struct itimerspec {
        pub it_interval: timespec,
        pub it_value: timespec,
    }

    pub const ZERO_TIMER_DURATION: itimerspec = itimerspec {
        it_interval: timespec {
            tv_sec: 0,
            tv_nsec: 0
        },
        it_value: timespec {
            tv_sec: 0,
            tv_nsec: 0
        },
    };

    extern "C" {
        pub fn timer_settime(timerid: timer_t, flags: libc::c_int, new_value: *const itimerspec, old_value: *mut itimerspec) -> libc::c_int;
        pub fn timer_gettime(timerid: timer_t, curr_value: *const itimerspec) -> libc::c_int;
        pub fn timer_delete(timerid: timer_t);
    }

    #[link(name = "os-timer-posix-c", lind = "static")]
    extern "C" {
        pub fn posix_timer(clock: libc::c_int, cb: Callback, data: *mut libc::c_void) -> timer_t;
    }
}

enum CallbackVariant {
    Trivial(*mut ffi::c_void),
    Boxed(Box<dyn FnMut()>),
}

///Timer's callback abstraction
pub struct Callback {
    variant: CallbackVariant,
    ffi_cb: ffi::Callback,
}

impl Callback {
    ///Creates raw callback for platform timer.
    ///
    ///Signature depends on platform.
    pub unsafe fn raw(ffi_cb: ffi::Callback, data: *mut ffi::c_void) -> Self {
        Self {
            variant: CallbackVariant::Trivial(data),
            ffi_cb,
        }
    }

    ///Creates callback using plain rust function
    pub fn plain(cb: fn()) -> Self {
        Self {
            variant: CallbackVariant::Trivial(cb as _),
            ffi_cb: ffi::timer_callback,
        }
    }

    ///Creates callback using plain unsafe function
    pub fn unsafe_plain(cb: unsafe fn()) -> Self {
        Self {
            variant: CallbackVariant::Trivial(cb as _),
            ffi_cb: ffi::timer_callback_unsafe,
        }
    }

    ///Creates callback using closure, storing it on heap.
    pub fn closure<F: 'static + FnMut()>(cb: F) -> Self {
        Self {
            variant: CallbackVariant::Boxed(Box::new(cb)),
            ffi_cb: ffi::timer_callback_generic::<F>,
        }
    }
}

///Posix timer wrapper
pub struct Timer {
    inner: AtomicUsize,
    data: Cell<BoxFnPtr>,
}

impl Timer {
    #[inline]
    ///Creates new uninitialized instance.
    ///
    ///In order to use it one must call `init`.
    pub const unsafe fn uninit() -> Self {
        Self {
            inner: AtomicUsize::new(0),
            data: Cell::new(BoxFnPtr::new()),
        }
    }

    #[inline(always)]
    fn get_inner(&self) -> usize {
        let inner = self.inner.load(Ordering::Acquire);
        debug_assert_ne!(inner, 0, "Timer has not been initialized");
        inner
    }

    #[inline(always)]
    ///Returns whether timer is initialized
    pub fn is_init(&self) -> bool {
        self.inner.load(Ordering::Acquire) != 0
    }

    #[must_use]
    ///Performs timer initialization
    ///
    ///`cb` pointer to function to invoke when timer expires.
    ///
    ///Returns whether timer has been initialized successfully or not.
    ///
    ///If timer is already initialized does nothing, returning false.
    pub fn init(&self, cb: Callback) -> bool {
        if self.is_init() {
            return false;
        }

        let ffi_cb = cb.ffi_cb;
        let (data, ffi_data) = match cb.variant {
            CallbackVariant::Trivial(data) => (BoxFnPtr(0), data),
            CallbackVariant::Boxed(cb) => unsafe {
                let raw = Box::into_raw(cb);
                (BoxFnPtr(mem::transmute(raw)), raw as *mut ffi::c_void)
            },
        };

        let handle = unsafe {
            ffi::posix_timer(libc::CLOCK_MONOTONIC, ffi_cb, ffi_data)
        };

        match self.inner.compare_exchange(0, handle, Ordering::SeqCst, Ordering::Acquire) {
            Ok(_) => match handle {
                0 => false,
                _ => {
                    //safe because we can never reach here once `handle.is_null() != true`
                    self.data.set(data);
                    true
                },
            },
            Err(_) => {
                unsafe {
                    ffi::timer_delete(handle);
                }
                false
            }
        }
    }

    ///Creates new timer, invoking provided `cb` when timer expires.
    ///
    ///On failure, returns `None`
    pub fn new(cb: Callback) -> Option<Self> {
        let ffi_cb = cb.ffi_cb;
        let (data, ffi_data) = match cb.variant {
            CallbackVariant::Trivial(data) => (BoxFnPtr(0), data),
            CallbackVariant::Boxed(cb) => unsafe {
                let raw = Box::into_raw(cb);
                (BoxFnPtr(mem::transmute(raw)), raw as *mut ffi::c_void)
            },
        };

        let handle = unsafe {
            ffi::posix_timer(libc::CLOCK_MONOTONIC, ffi_cb, ffi_data)
        };

        if handle == 0 {
            return None;
        }

        Some(Self {
            inner: AtomicUsize::new(handle),
            data: Cell::new(data),
        })
    }

    ///Schedules timer to alarm periodically with `interval` with initial alarm of `timeout`.
    ///
    ///Note that if timer has been scheduled before, but hasn't expire yet, behaviour is undefined (Callback may or may not be called).
    ///To prevent that user must `cancel` timer first.
    ///
    ///Returns `true` if successfully set, otherwise on error returns `false`
    pub fn schedule_interval(&self, timeout: time::Duration, interval: time::Duration) -> bool {
        let it_value = ffi::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            #[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
            tv_nsec: timeout.subsec_nanos() as libc::suseconds_t,
            #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        };

        let it_interval = ffi::timespec {
            tv_sec: interval.as_secs() as libc::time_t,
            #[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
            tv_nsec: interval.subsec_nanos() as libc::suseconds_t,
            #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
            tv_nsec: interval.subsec_nanos() as libc::c_long,
        };

        let new_value = ffi::itimerspec {
            it_interval,
            it_value,
        };

        unsafe {
            ffi::timer_settime(self.get_inner(), 0, &new_value, ptr::null_mut()) == 0
        }
    }

    #[inline]
    ///Returns `true` if timer has been scheduled and still pending.
    ///
    ///On Win/Mac it only returns whether timer has been scheduled, as there is no way to check
    ///whether timer is ongoing
    pub fn is_scheduled(&self) -> bool {
        let handle = self.get_inner();
        let curr_value = unsafe {
            let mut curr_value = mem::MaybeUninit::<ffi::itimerspec>::uninit();

            if ffi::timer_gettime(handle, curr_value.as_mut_ptr()) != 0 {
                return false;
            }
            curr_value.assume_init()
        };

        curr_value != ffi::ZERO_TIMER_DURATION
    }

    #[inline]
    ///Cancels ongoing timer, if it was scheduled.
    pub fn cancel(&self) {
        if self.is_scheduled() {
            unsafe {
                ffi::timer_settime(self.get_inner(), 0, &mem::MaybeUninit::zeroed().assume_init(), ptr::null_mut());
            }
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let handle = self.inner.load(Ordering::Relaxed);
        if handle != 0 {
            self.cancel();
            unsafe {
                ffi::timer_delete(handle)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_plain_fn() {
        let mut timer = unsafe {
            Timer::uninit()
        };

        fn cb() {
        }

        let closure = || {
        };

        assert!(timer.init(Callback::plain(cb)));
        let ptr = timer.inner.load(Ordering::Relaxed);
        assert_ne!(ptr, 0);
        assert!(timer.data.get_mut().is_null());

        assert!(!timer.init(Callback::closure(closure)));
        assert_ne!(ptr, 0);
        assert_eq!(ptr, timer.inner.load(Ordering::Relaxed));
        assert!(timer.data.get_mut().is_null());
    }

    #[test]
    fn init_closure() {
        let mut timer = unsafe {
            Timer::uninit()
        };

        fn cb() {
        }

        let closure = || {
        };

        assert!(timer.init(Callback::closure(closure)));
        let ptr = timer.inner.load(Ordering::Relaxed);
        assert_ne!(ptr, 0);
        assert!(!timer.data.get_mut().is_null());

        assert!(!timer.init(Callback::plain(cb)));
        assert_ne!(ptr, 0);
        assert_eq!(ptr, timer.inner.load(Ordering::Relaxed));
        assert!(!timer.data.get_mut().is_null());
    }
}
