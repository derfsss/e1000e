//! The diag build's extras log: RAM:e1000e_diag.log.
//!
//! A device's unit task is an exec task and cannot call DOS, so a small
//! logger process (CreateNewProc) owns the file. Lines reach it as messages
//! on its port (amigaos4::msg); it appends each with the seconds since the
//! driver started. It starts when the driver is loaded and stops at
//! expunge, which waits until the process is gone (its code is in this
//! segment).

use alloc::string::String;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use amigaos4::msg::{Message, Port, Sender};
use amigaos4::sys::{self, TagItem};

const LOG_FILE: &[u8] = amigaos4::amstr!("RAM:e1000e_diag.log");
const PROC_NAME: &[u8] = amigaos4::amstr!("e1000e_diag.device log");

enum Log {
    Line(String),
    Quit,
}

/// The logger's port while it runs, and the lines that came before it
/// took them. Under its own exec mutex.
struct Slot {
    state: UnsafeCell<State>,
    mutex: AtomicPtr<c_void>,
}
struct State {
    sender: Option<Sender<Log>>,
    /// Lines queued while the logger starts (at most EARLY_MAX).
    early: alloc::vec::Vec<String>,
    /// The logger was started and has not stopped.
    starting: bool,
}
const EARLY_MAX: usize = 64;
// SAFETY: `state` is only touched with `mutex` held.
unsafe impl Sync for Slot {}

static SLOT: Slot = Slot {
    state: UnsafeCell::new(State { sender: None, early: alloc::vec::Vec::new(), starting: false }),
    mutex: AtomicPtr::new(null_mut()),
};
/// The timebase when the logger started (line time stamps).
static T0_HI: AtomicU32 = AtomicU32::new(0);
static T0_LO: AtomicU32 = AtomicU32::new(0);

fn with_slot<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    let m = SLOT.mutex.load(Ordering::Acquire);
    if m.is_null() {
        return None;
    }
    // SAFETY: the mutex guards the slot.
    unsafe {
        sys::exec_mutex_obtain(m as sys::APTR);
        let r = f(&mut *SLOT.state.get());
        sys::exec_mutex_release(m as sys::APTR);
        Some(r)
    }
}

/// Queue a line for the log (dropped if the logger does not run).
pub fn line(text: String) {
    let _ = with_slot(|s| {
        if let Some(snd) = s.sender.as_ref() {
            if let Ok(m) = Message::new(Log::Line(text)) {
                let _ = snd.send(m);
            }
        } else if s.starting && s.early.len() < EARLY_MAX {
            s.early.push(text);
        }
    });
}

fn write_str(fh: u32, s: &str) {
    if fh != 0 && !s.is_empty() {
        // SAFETY: an open file handle and a buffer of s.len() bytes.
        unsafe { sys::dos_write(fh, s.as_ptr() as _, s.len() as i32) };
    }
}

extern "C" fn logger_main(_args: *const u8, _len: i32, _sysbase: *mut c_void) -> i32 {
    let Ok(port) = Port::<Log>::new() else { return 20 };
    // SAFETY: plain DOS calls in this process.
    let fh = unsafe {
        let fh = sys::dos_open(LOG_FILE.as_ptr() as _, sys::MODE_READWRITE);
        if fh != 0 {
            sys::dos_change_file_position(fh, 0, sys::OFFSET_END);
        }
        fh
    };
    let hz = super::timebase_hz();
    write_str(fh, &alloc::format!("--- {} loaded ---\n", super::NAME));
    // Take over: the lines that came first, then the port for the rest.
    let early = with_slot(|s| {
        s.sender = Some(port.sender());
        core::mem::take(&mut s.early)
    })
    .unwrap_or_default();
    for text in early {
        write_str(fh, &alloc::format!("{:>6} {} (queued at start-up)\n", "-", text));
    }
    loop {
        let Ok(msg) = port.recv() else { break };
        match &*msg {
            Log::Line(text) => {
                let t0 = ((T0_HI.load(Ordering::Relaxed) as u64) << 32) | T0_LO.load(Ordering::Relaxed) as u64;
                let ms = amigaos4::arch::timebase().wrapping_sub(t0) * 1000 / hz;
                write_str(fh, &alloc::format!("{:>6}.{:03} {}\n", ms / 1000, ms % 1000, text));
            }
            Log::Quit => break,
        }
    }
    let _ = with_slot(|s| {
        s.sender = None;
        s.starting = false;
    });
    drop(port);
    if fh != 0 {
        write_str(fh, "--- expunged ---\n");
        // SAFETY: the handle opened above.
        unsafe { sys::dos_close(fh) };
    }
    0
}

/// Start the logger (at driver init). Without DOS or memory the driver
/// runs on with no log.
pub fn start() {
    // SAFETY: exec and DOS calls at driver init.
    unsafe {
        if SLOT.mutex.load(Ordering::Acquire).is_null() {
            let m = sys::exec_alloc_sys_object(sys::ASOT_MUTEX, [TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 }].as_ptr());
            if m.is_null() {
                return;
            }
            SLOT.mutex.store(m as *mut c_void, Ordering::Release);
        }
        let _ = with_slot(|s| s.starting = true);
        let t0 = amigaos4::arch::timebase();
        T0_HI.store((t0 >> 32) as u32, Ordering::Relaxed);
        T0_LO.store(t0 as u32, Ordering::Relaxed);
        let tags = [
            TagItem { ti_Tag: sys::NP_START, ti_Data: logger_main as *const () as usize as u32 },
            TagItem { ti_Tag: sys::NP_NAME, ti_Data: PROC_NAME.as_ptr() as u32 },
            TagItem { ti_Tag: sys::NP_PRIORITY, ti_Data: (-1i32) as u32 },
            TagItem { ti_Tag: sys::NP_CHILD, ti_Data: 0 },
            TagItem { ti_Tag: sys::TAG_DONE, ti_Data: 0 },
        ];
        if sys::dos_create_new_proc(tags.as_ptr()).is_null() {
            let _ = with_slot(|s| {
                s.starting = false;
                s.early.clear();
            });
            amigaos4::serial_println!("{}: the RAM: log process did not start", super::NAME);
        }
        // Lines sent before the process takes over are queued (State::early).
    }
}

/// Stop the logger (at expunge) and wait until its process is gone.
pub fn stop() {
    let sent = with_slot(|s| match s.sender.as_ref() {
        Some(snd) => Message::new(Log::Quit).ok().map(|m| snd.send(m).is_ok()).unwrap_or(false),
        None => false,
    })
    .unwrap_or(false);
    if sent {
        // Expunge runs with timer.device available (units have run).
        if let Ok(mut t) = amigaos4::timer::AmigaTimer::open(amigaos4::timer::TimerUnit::MicroHz) {
            for _ in 0..300 {
                // SAFETY: a name lookup.
                if unsafe { sys::exec_find_task(PROC_NAME.as_ptr() as _) }.is_null() {
                    break;
                }
                let _ = t.delay(amigaos4::timer::TimerVal::from_millis(10));
            }
        }
    }
    let m = SLOT.mutex.swap(null_mut(), Ordering::AcqRel);
    if !m.is_null() {
        // SAFETY: the mutex from start(); the logger no longer uses it.
        unsafe { sys::exec_free_sys_object(sys::ASOT_MUTEX, m as sys::APTR) };
    }
}
