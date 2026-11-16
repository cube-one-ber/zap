//! Interrupts. The worker turns SIGINT and SIGTERM into a cancellation it
//! checks between steps; the unprivileged process defers them while the
//! worker runs, so it can report how the worker ended.
use crate::errors::{Kind, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub static CANCELLED: AtomicBool = AtomicBool::new(false);

extern "C" fn cancel(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
}

extern "C" fn ignore(_: libc::c_int) {}

const HANDLED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT];
static SAVED: Mutex<Option<[libc::sigaction; 3]>> = Mutex::new(None);

fn action(handler: extern "C" fn(libc::c_int), flags: libc::c_int) -> libc::sigaction {
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = handler as usize;
        libc::sigemptyset(&mut act.sa_mask);
        act.sa_flags = flags;
        act
    }
}

/// In the worker: record SIGINT and SIGTERM for `check`.
pub fn install() {
    let act = action(cancel, 0);
    unsafe {
        libc::sigaction(libc::SIGINT, &act, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &act, std::ptr::null_mut());
    }
}

/// Survive the terminal's interrupts until `restore`. A handler, unlike
/// SIG_IGN, is reset by exec, so child processes still receive them.
pub fn defer_interrupts() {
    let act = action(ignore, libc::SA_RESTART);
    let mut saved: [libc::sigaction; 3] = unsafe { std::mem::zeroed() };
    for (signal, previous) in HANDLED.iter().zip(saved.iter_mut()) {
        unsafe { libc::sigaction(*signal, &act, previous) };
    }
    *SAVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(saved);
}

pub fn restore() {
    if let Some(saved) = SAVED.lock().unwrap_or_else(|e| e.into_inner()).take() {
        for (signal, previous) in HANDLED.iter().zip(saved.iter()) {
            unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) };
        }
    }
}

pub fn check() -> Result<()> {
    if CANCELLED.load(Ordering::Relaxed) {
        Err(Kind::Cancelled.into())
    } else {
        Ok(())
    }
}
