use core::{time, ptr, mem};
use core::cell::Cell;
use core::sync::atomic::{AtomicPtr, Ordering};
use super::FatPtr;

extern crate alloc;
use alloc::boxed::Box;

mod ffi {
    pub use core::ffi::c_void;

    type DWORD = u32;
    type BOOL = i32;

    #[repr(C)]
    pub struct FileTime {
        pub low_date_time: DWORD,
        pub high_date_time: DWORD,
    }

    pub type Callback = Option<unsafe extern "system" fn(cb_inst: *mut c_void, ctx: *mut c_void, timer: *mut c_void)>;

    extern "system" {
        pub fn CloseThreadpoolTimer(ptr: *mut c_void);
        pub fn CreateThreadpoolTimer(cb: Callback, user_data: *mut c_void, env: *mut c_void) -> *mut c_void;
        pub fn SetThreadpoolTimerEx(timer: *mut c_void, pftDueTime: *mut FileTime, msPeriod: DWORD, msWindowLength: DWORD) -> BOOL;
        pub fn WaitForThreadpoolTimerCallbacks(timer: *mut c_void, fCancelPendingCallbacks: BOOL);
    }
}

unsafe extern "system" fn timer_callback(_: *mut ffi::c_void, data: *mut ffi::c_void, _: *mut ffi::c_void) {
    let cb: fn() -> () = mem::transmute(data);

    (cb)();
}

unsafe extern "system" fn timer_callback_unsafe(_: *mut ffi::c_void, data: *mut ffi::c_void, _: *mut ffi::c_void) {
    let cb: unsafe fn() -> () = mem::transmute(data);

    (cb)();
}

unsafe extern "system" fn timer_callback_generic<T: FnMut() -> ()>(_: *mut ffi::c_void, data: *mut ffi::c_void, _: *mut ffi::c_void) {
    let cb = &mut *(data as *mut T);

    (cb)();
}

enum CallbackVariant {
    PlainUnsafe(unsafe fn()),
    Plain(fn()),
    Closure(Box<dyn FnMut()>),
}

///Timer's callback abstraction
pub struct Callback {
    variant: CallbackVariant,
    ffi_cb: ffi::Callback,
}

//Cannot do From<T> for Callback
//cuz no fucking specialization in stable
impl Callback {
    ///Creates callback using plain rust function
    pub fn plain(cb: fn()) -> Self {
        Self {
            variant: CallbackVariant::Plain(cb),
            ffi_cb: Some(timer_callback),
        }
    }

    ///Creates callback using plain unsafe function
    pub fn unsafe_plain(cb: unsafe fn()) -> Self {
        Self {
            variant: CallbackVariant::PlainUnsafe(cb),
            ffi_cb: Some(timer_callback_unsafe),
        }
    }

    ///Creates callback using closure, storing it on heap.
    pub fn closure<F: 'static + FnMut()>(cb: F) -> Self {
        Self {
            variant: CallbackVariant::Closure(Box::new(cb)),
            ffi_cb: Some(timer_callback_generic::<F>),
        }
    }
}

///Windows thread pool timer
pub struct Timer {
    inner: AtomicPtr<ffi::c_void>,
    data: Cell<FatPtr>,
}

impl Timer {
    #[inline]
    ///Creates new uninitialized instance.
    ///
    ///In order to use it one must call `init`.
    pub const unsafe fn uninit() -> Self {
        Self {
            inner: AtomicPtr::new(ptr::null_mut()),
            data: Cell::new(0),
        }
    }

    #[inline(always)]
    fn get_inner(&self) -> *mut ffi::c_void {
        let inner = self.inner.load(Ordering::Acquire);
        debug_assert!(!inner.is_null(), "Timer has not been initialized");
        inner
    }

    #[inline(always)]
    ///Returns whether timer is initialized
    pub fn is_init(&self) -> bool {
        !self.inner.load(Ordering::Acquire).is_null()
    }

    #[must_use]
    ///Performs timer initialization
    ///
    ///`cb` is variant of callback to invoke when timer expires
    ///
    ///Returns whether timer has been initialized successfully or not.
    ///
    ///If timer is already initialized does nothing, returning false.
    pub fn init(&self, cb: Callback) -> bool {
        if self.is_init() {
            return false;
        }

        let ffi_cb = cb.ffi_cb;
        let ffi_data = match cb.variant {
            CallbackVariant::Plain(cb) => cb as *mut ffi::c_void,
            CallbackVariant::PlainUnsafe(cb) => cb as *mut ffi::c_void,
            CallbackVariant::Closure(ref cb) => &*cb as *const _ as *mut ffi::c_void,
        };

        let handle = unsafe {
            ffi::CreateThreadpoolTimer(ffi_cb, ffi_data, ptr::null_mut())
        };

        match self.inner.compare_exchange(ptr::null_mut(), handle, Ordering::SeqCst, Ordering::Acquire) {
            Ok(_) => match handle.is_null() {
                true => false,
                false => {
                    match cb.variant {
                        CallbackVariant::Closure(cb) => unsafe {
                            //safe because we can never reach here once `handle.is_null() != true`
                            self.data.set(mem::transmute(Box::into_raw(cb)))
                        },
                        _ => (),
                    }
                    true
                },
            },
            Err(_) => {
                unsafe {
                    ffi::CloseThreadpoolTimer(handle);
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
            CallbackVariant::Plain(cb) => (0, cb as *mut ffi::c_void),
            CallbackVariant::PlainUnsafe(cb) => (0, cb as *mut ffi::c_void),
            CallbackVariant::Closure(cb) => unsafe {
                let raw = Box::into_raw(cb);
                (mem::transmute(raw), raw as *mut ffi::c_void)
            },
        };

        let handle = unsafe {
            ffi::CreateThreadpoolTimer(ffi_cb, ffi_data, ptr::null_mut())
        };

        if handle.is_null() {
            return None;
        }

        Some(Self {
            inner: AtomicPtr::new(handle),
            data: Cell::new(data),
        })
    }

    ///Schedules timer to alarm periodically with `interval` with initial alarm of `timeout`.
    ///
    ///Note that if timer has been scheduled before, but hasn't expire yet, it shall be cancelled.
    ///To prevent that user must `cancel` timer first.
    ///
    ///# Note
    ///
    ///- `interval` is truncated by `u32::max_value()`
    ///
    ///Returns `true` if successfully set, otherwise on error returns `false`
    pub fn schedule_interval(&self, timeout: time::Duration, interval: time::Duration) -> bool {
        let mut ticks = i64::from(timeout.subsec_nanos() / 100);
        ticks += (timeout.as_secs() * 10_000_000) as i64;
        let ticks = -ticks;

        let interval = interval.as_millis() as u32;

        unsafe {
            let mut time: ffi::FileTime = mem::transmute(ticks);
            ffi::SetThreadpoolTimerEx(self.get_inner(), &mut time, interval, 0);
        }

        true
    }

    ///Cancels ongoing timer, if it was armed.
    pub fn cancel(&self) {
        let handle = self.get_inner();
        unsafe {
            ffi::SetThreadpoolTimerEx(handle, ptr::null_mut(), 0, 0);
            ffi::WaitForThreadpoolTimerCallbacks(handle, 1);
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let handle = self.inner.load(Ordering::Relaxed);
        if !handle.is_null() {
            self.cancel();
            unsafe {
                ffi::CloseThreadpoolTimer(handle);
            }
        }

        let data = self.data.get();
        if data != 0 {
            unsafe {
                let _ = Box::from_raw(mem::transmute::<_, *mut dyn FnMut()>(data));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_plain_fn() {
        let timer = unsafe {
            Timer::uninit()
        };

        fn cb() {
        }

        let closure = || {
        };

        assert!(timer.init(Callback::plain(cb)));
        let ptr = timer.inner.load(Ordering::Relaxed);
        assert!(!ptr.is_null());
        assert_eq!(timer.data.get(), 0);

        assert!(!timer.init(Callback::closure(closure)));
        assert!(!ptr.is_null());
        assert_eq!(ptr, timer.inner.load(Ordering::Relaxed));
        assert_eq!(timer.data.get(), 0);
    }

    #[test]
    fn init_closure() {
        let timer = unsafe {
            Timer::uninit()
        };

        fn cb() {
        }

        let closure = || {
        };

        assert!(timer.init(Callback::closure(closure)));
        let ptr = timer.inner.load(Ordering::Relaxed);
        assert!(!ptr.is_null());
        assert_ne!(timer.data.get(), 0);

        assert!(!timer.init(Callback::plain(cb)));
        assert!(!ptr.is_null());
        assert_eq!(ptr, timer.inner.load(Ordering::Relaxed));
        assert_ne!(timer.data.get(), 0);
    }
}
