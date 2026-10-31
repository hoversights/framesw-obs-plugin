//! OBS's main window hidden or shown on its own (`main_window`), macOS.
//!
//! FrameSW runs its own OBS out of sight. On the Mac it did that by hiding
//! the whole OBS app (`NSRunningApplication.hide`), which hides every OBS
//! window together: a projector or a Filters dialog opened from FrameSW
//! un-hid the app and brought the main window up with it, and a click back
//! in FrameSW hid them all again, the projector included. Measured
//! 2026-10-29 on OBS 33.0.0: "Projector - Preview" 480x302 still there,
//! off screen. Taking the main window alone off screen leaves projectors
//! and dialogs free to stay up beside FrameSW.
//!
//! Off screen below Qt, not through it: `[NSWindow orderOut:]` on the main
//! window's NSWindow, so Qt still counts the window as open. The first
//! version called `QWidget::hide()`, and on quit Qt then skipped the hidden
//! window's close event, which is where OBS clears its scenes
//! (`OBSBasic::closeEvent` › `closeWindow`, "==== Shutting down"). Every
//! source outlived the scene cleanup into `obs_shutdown`, which stops the
//! audio output before freeing sources, and a Core Audio input's next
//! buffer read the stopped output: SIGSEGV at 0x38 in `copy_audio_data`,
//! two quits out of two with a PS4 on a capture stick (2026-10-29). Windows
//! is hidden the same way by FrameSW itself (`ShowWindow(SW_HIDE)`), so
//! this request is macOS only.
//!
//! `obs_frontend_get_main_window_handle` returns `(void *)main->winId()`,
//! an `NSView *` on macOS (obs-studio 33.0.0, frontend/OBSStudioAPI.cpp);
//! `[view window]` is its NSWindow. The Objective-C runtime is resolved at
//! runtime like every other symbol here, and every message is sent on the
//! UI thread, which is AppKit's main thread.
//!
//! OBS shows its main window in `OBSBasic::OBSInit` and only afterwards
//! sends FINISHED_LOADING, while obs-websocket already answers before
//! either. A hide asked for before then is remembered and applied once
//! loading has finished, queued behind the rest of OBSInit (which calls
//! `activateWindow()` after FINISHED_LOADING), so it never depends on
//! when FrameSW asks.
//!
//! That hide comes after OBS has shown the window, and FrameSW hiding the
//! app from outside takes it down a look later: 30 ms on screen, 0.85 s
//! into the start, seen by the operator as a flash on every connect
//! (measured 2026-10-30, `CGWindowListCopyWindowInfo` every 5 ms). So an
//! OBS that FrameSW starts out of sight (`FRAMESW_HIDE_MAIN_WINDOW=1` in
//! its environment) has its main window made see-through here, at
//! post-load, which OBS runs inside `OBSBasic::OBSInit` before its
//! `show()`. Once loading has finished it is taken off screen and made
//! opaque again, in that order. Only the main window: dialogs and
//! projectors are windows of their own.

use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use studio_mode_meters_core::metering::{ffi_guard, obs_queue_task, OBS_TASK_UI, SHUTTING_DOWN, THREADS};
use studio_mode_meters_core::obs_data;
use studio_mode_meters_core::{calldata, log_line};

use crate::video_tap::{obs_frontend_add_event_callback, obs_frontend_remove_event_callback, EVENT_EXIT};

// frontend/api/obs-frontend-api.h (obs-studio 33.0.0):
//   EXPORT void *obs_frontend_get_main_window_handle(void);
studio_mode_meters_core::resolved_fn!(obs_frontend_get_main_window_handle: extern "C" fn() -> *mut c_void);

// <objc/runtime.h>: SEL sel_registerName(const char *str);
studio_mode_meters_core::resolved_fn!(sel_registerName: extern "C" fn(*const c_char) -> *mut c_void);
// <objc/message.h>: objc_msgSend, called through a pointer of the exact
// prototype of each message (required on arm64, where it is not variadic).
studio_mode_meters_core::resolved_fn!(objc_msgSend: extern "C" fn());

// `enum obs_frontend_event` in obs-frontend-api.h, counted from
// OBS_FRONTEND_EVENT_STREAMING_STARTING = 0 (no explicit values in the enum).
const EVENT_FINISHED_LOADING: c_int = 26;

/// What FrameSW last asked for. Starts false: OBS shows its window as usual
/// until someone asks otherwise.
static WANT_HIDDEN: AtomicBool = AtomicBool::new(false);
/// FINISHED_LOADING has been seen: the main window exists and OBS has
/// already shown it.
static LOADED: AtomicBool = AtomicBool::new(false);
static CALLBACK_REGISTERED: AtomicBool = AtomicBool::new(false);
/// The main window is see-through, from post-load until it has been taken
/// off screen once loading finished (or until it is shown).
static SEE_THROUGH: AtomicBool = AtomicBool::new(false);

/// Set to `1` by FrameSW in the environment of an OBS it starts out of
/// sight.
const START_HIDDEN_ENV: &str = "FRAMESW_HIDE_MAIN_WINDOW";

/// Called from `obs_module_post_load`, on the UI thread.
pub fn init() {
    if CALLBACK_REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    match obs_frontend_add_event_callback() {
        Some(add) => add(on_frontend_event, std::ptr::null_mut()),
        None => {
            CALLBACK_REGISTERED.store(false, Ordering::Release);
            log_line("obs_frontend_add_event_callback unavailable: main_window can't hide OBS before it finishes loading");
            // Nothing would make it opaque again: left as OBS shows it.
            return;
        }
    }
    if cfg!(target_os = "macos") && std::env::var_os(START_HIDDEN_ENV).is_some_and(|v| v == "1") {
        WANT_HIDDEN.store(true, Ordering::Release);
        match set_alpha(0.0) {
            Ok(()) => {
                SEE_THROUGH.store(true, Ordering::Release);
                log_line("main window see-through until OBS finishes loading (started out of sight)");
            }
            Err(e) => log_line(&format!("main window not made see-through: {e}")),
        }
    }
}

extern "C" fn on_frontend_event(event: c_int, _private_data: *mut c_void) {
    ffi_guard(
        "main_window::on_frontend_event",
        (),
        std::panic::AssertUnwindSafe(|| match event {
            EVENT_FINISHED_LOADING => {
                LOADED.store(true, Ordering::Release);
                if WANT_HIDDEN.load(Ordering::Acquire) || SEE_THROUGH.load(Ordering::Acquire) {
                    // From another thread, so OBS's AutoConnection queues it
                    // behind the rest of OBSInit rather than running it here
                    // (OBSApp.cpp `ui_task_handler`: wait=false is
                    // Qt::AutoConnection, a direct call on the UI thread).
                    let handle = std::thread::spawn(|| {
                        if SHUTTING_DOWN.load(Ordering::Acquire) {
                            return;
                        }
                        if let Some(queue) = obs_queue_task() {
                            queue(OBS_TASK_UI, deferred_hide_on_ui_thread, std::ptr::null_mut(), false);
                        }
                    });
                    if let Ok(mut threads) = THREADS.lock() {
                        threads.push(handle);
                    }
                }
            }
            EVENT_EXIT => {
                // Not about the window: this is the plugin's one frontend
                // callback registered from the start, so the device-list
                // probes it keeps and any Mic check still listening are let
                // go here, before obs_shutdown.
                studio_mode_meters_core::metering::release_kept_probes();
                crate::input_probe::end_all_at_exit();
                if CALLBACK_REGISTERED.swap(false, Ordering::AcqRel) {
                    if let Some(remove) = obs_frontend_remove_event_callback() {
                        remove(on_frontend_event, std::ptr::null_mut());
                    }
                }
            }
            _ => {}
        }),
    );
}

/// The main window's NSWindow, with `sel_registerName` and `objc_msgSend`.
/// UI thread only (AppKit's main thread).
#[cfg(target_os = "macos")]
fn ns_window() -> Result<(*mut c_void, extern "C" fn(*const c_char) -> *mut c_void, extern "C" fn()), String> {
    let handle = obs_frontend_get_main_window_handle().ok_or("obs_frontend_get_main_window_handle unavailable")?;
    let (Some(sel), Some(send)) = (sel_registerName(), objc_msgSend()) else {
        return Err("the Objective-C runtime isn't reachable".into());
    };
    let view = handle();
    if view.is_null() {
        return Err("OBS has no main window".into());
    }
    // SAFETY: objc_msgSend called through the exact prototype of
    // `-[NSView window]`: id (id, SEL).
    let window = unsafe {
        let get: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(send);
        get(view, sel(c"window".as_ptr()))
    };
    if window.is_null() {
        return Err("the main window's NSWindow isn't there".into());
    }
    Ok((window, sel, send))
}

/// Puts the main window on screen and in front, or takes it off screen.
/// Shown, it is opaque. UI thread only (AppKit's main thread).
#[cfg(target_os = "macos")]
fn apply(show: bool) -> Result<(), String> {
    if show && SEE_THROUGH.swap(false, Ordering::AcqRel) {
        set_alpha(1.0)?;
    }
    let (window, sel, send) = ns_window()?;
    // SAFETY: objc_msgSend called through the exact prototype of
    // `-[NSWindow orderOut:]` / `-[NSWindow makeKeyAndOrderFront:]`:
    // void (id, SEL, id).
    unsafe {
        let order: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) = std::mem::transmute(send);
        let message = if show { c"makeKeyAndOrderFront:" } else { c"orderOut:" };
        order(window, sel(message.as_ptr()), std::ptr::null_mut());
    }
    Ok(())
}

/// The main window's opacity, 0.0 (see-through) to 1.0. UI thread only.
#[cfg(target_os = "macos")]
fn set_alpha(alpha: f64) -> Result<(), String> {
    let (window, sel, send) = ns_window()?;
    // SAFETY: objc_msgSend called through the exact prototype of
    // `-[NSWindow setAlphaValue:]` (NSWindow.h: `@property CGFloat
    // alphaValue`; CGFloat is double on 64-bit): void (id, SEL, CGFloat).
    unsafe {
        let set: extern "C" fn(*mut c_void, *mut c_void, f64) = std::mem::transmute(send);
        set(window, sel(c"setAlphaValue:".as_ptr()), alpha);
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn apply(_show: bool) -> Result<(), String> {
    Err("main_window is macOS only; FrameSW hides OBS's window itself elsewhere".into())
}

#[cfg(not(target_os = "macos"))]
fn set_alpha(_alpha: f64) -> Result<(), String> {
    Err("main_window is macOS only".into())
}

extern "C" fn deferred_hide_on_ui_thread(_param: *mut c_void) {
    ffi_guard(
        "main_window::deferred_hide_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            // Asked again since: whatever is wanted now.
            if WANT_HIDDEN.load(Ordering::Acquire) {
                match apply(false) {
                    Ok(()) => log_line("main window hidden once OBS finished loading"),
                    Err(e) => log_line(&format!("main window not hidden after loading: {e}")),
                }
            }
            // Off screen first, then opaque: never on screen in between.
            if SEE_THROUGH.swap(false, Ordering::AcqRel) {
                match set_alpha(1.0) {
                    Ok(()) => log_line("main window opaque again"),
                    Err(e) => log_line(&format!("main window not made opaque again: {e}")),
                }
            }
        }),
    );
}

struct Apply {
    ran: bool,
    show: bool,
    result: Result<(), String>,
}

extern "C" fn apply_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "main_window::apply_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            let state = unsafe { &mut *(param as *mut Apply) };
            state.ran = true;
            state.result = apply(state.show);
        }),
    );
}

extern "C" fn handle_main_window(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_main_window",
        (),
        std::panic::AssertUnwindSafe(|| handle_main_window_impl(request_data, response_data)),
    );
}

/// Request: `{"show": true|false}`. Response: `{"ok": true, "applied": bool}`.
/// `applied` is false when OBS hasn't finished loading yet: a hide is then
/// applied once it has, and a show needs nothing, since OBS shows its own
/// window. Only the main window: projectors and dialogs keep their own
/// visibility. `show` also brings it in front, as Show OBS wants.
fn handle_main_window_impl(request_data: *mut c_void, response_data: *mut c_void) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let Some(show) = obs_data::get_optional_bool(request_data, "show") else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "show is required");
        return;
    };
    if !cfg!(target_os = "macos") {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "main_window is macOS only");
        return;
    }
    WANT_HIDDEN.store(!show, Ordering::Release);
    if !LOADED.load(Ordering::Acquire) {
        if !show && !CALLBACK_REGISTERED.load(Ordering::Acquire) {
            obs_data::set_bool(response_data, "ok", false);
            obs_data::set_string(response_data, "error", "OBS is still loading and its events can't be followed");
            return;
        }
        obs_data::set_bool(response_data, "ok", true);
        obs_data::set_bool(response_data, "applied", false);
        return;
    }
    let Some(queue) = obs_queue_task() else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "obs_queue_task unavailable");
        return;
    };
    let mut state = Apply { ran: false, show, result: Err(String::new()) };
    queue(OBS_TASK_UI, apply_on_ui_thread, (&mut state as *mut Apply).cast(), true);
    match (state.ran, state.result) {
        (true, Ok(())) => {
            obs_data::set_bool(response_data, "ok", true);
            obs_data::set_bool(response_data, "applied", true);
        }
        (true, Err(e)) => {
            obs_data::set_bool(response_data, "ok", false);
            obs_data::set_string(response_data, "error", &e);
        }
        (false, _) => {
            obs_data::set_bool(response_data, "ok", false);
            obs_data::set_string(response_data, "error", "UI-thread task never ran");
        }
    }
}

pub fn requests() -> [(&'static str, calldata::RequestCallbackFn); 1] {
    [("main_window", handle_main_window as calldata::RequestCallbackFn)]
}
