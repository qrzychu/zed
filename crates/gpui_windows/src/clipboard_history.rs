//! Support for pasting from the Windows clipboard history (`Win+V`).
//!
//! When the user picks an item from the clipboard history flyout, Windows doesn't
//! send the application any kind of "paste" notification; it just injects a
//! synthetic `Ctrl+V` keystroke sequence (via `SendInput`) into the foreground
//! window. If the user's keymap doesn't bind `ctrl-v` to paste (e.g. in Vim mode,
//! where `ctrl-v` starts a visual block selection), picking an item would silently
//! do the wrong thing.
//!
//! Injected keystrokes are only distinguishable from physical ones inside a
//! low-level keyboard hook (`WH_KEYBOARD_LL`), where the event carries the
//! `LLKHF_INJECTED` flag; by the time the keystroke arrives as a `WM_KEYDOWN`,
//! that information is gone. So we install such a hook: when an injected `Ctrl+V`
//! is headed for one of our windows, we swallow the keystroke and post
//! [`WM_GPUI_PASTE`] instead, which GPUI dispatches as the application's
//! `OsAction::Paste` action, independent of the user's keymap.
//!
//! A `WH_KEYBOARD_LL` hook is global: its callback runs on the thread that
//! installed it for *every* keystroke on the system, and if that thread is slow
//! to respond Windows silently removes the hook (`LowLevelHooksTimeout`). To keep
//! that off Zed's UI thread, the hook lives on a dedicated thread that does
//! nothing but pump messages. It is also only installed while a Zed window is
//! focused (see [`set_clipboard_history_hook_focus`]), so the hook isn't in the
//! system-wide input path while Zed sits in the background.

use std::{
    sync::atomic::{AtomicU32, Ordering},
    thread::JoinHandle,
};

use ::util::ResultExt;
use anyhow::Context;
use windows::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT, VK_V,
        },
        WindowsAndMessaging::{
            CallNextHookEx, GetClassNameW, GetForegroundWindow, GetMessageW, GetSystemMetrics,
            GetWindowThreadProcessId, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG,
            PM_NOREMOVE, PeekMessageW, PostMessageW, PostThreadMessageW, SM_REMOTESESSION,
            SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT,
            WM_SYSKEYDOWN, WM_USER,
        },
    },
};

use crate::{HWND, WINDOW_CLASS_NAME, WM_GPUI_PASTE, get_module_handle};

/// Thread id of the hook thread, published once it is pumping messages so the UI
/// thread can drive it via [`PostThreadMessageW`]. `0` means no hook thread is
/// running (headless, a Remote Desktop session, or before/after its lifetime).
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

// Thread messages handled by the hook thread's message loop. These are posted to
// the hook thread (which owns no windows) via `PostThreadMessageW`, so they live
// on a separate queue and don't collide with the `WM_GPUI_*` window messages.
const MSG_INSTALL_HOOK: u32 = WM_USER;
const MSG_UNINSTALL_HOOK: u32 = WM_USER + 1;

/// Owns the hook thread; stops it (and so uninstalls the hook) on drop.
pub(crate) struct ClipboardHistoryPasteHook {
    thread: Option<JoinHandle<()>>,
}

impl ClipboardHistoryPasteHook {
    /// Spawns the dedicated hook thread. Returns `None` (and installs nothing) in
    /// a Remote Desktop session, where every keystroke arrives flagged as injected
    /// (`LLKHF_INJECTED`) and so a clipboard-history paste can't be told apart from
    /// a physical `Ctrl+V` — intercepting would hijack real `Ctrl+V` presses.
    pub(crate) fn install() -> Option<Self> {
        if unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0 {
            return None;
        }

        let thread = std::thread::Builder::new()
            .name("clipboard-history-hook".to_owned())
            .spawn(hook_thread_main)
            .context("spawning clipboard history hook thread")
            .log_err()?;

        Some(Self {
            thread: Some(thread),
        })
    }
}

impl Drop for ClipboardHistoryPasteHook {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let thread_id = HOOK_THREAD_ID.load(Ordering::SeqCst);
        if thread_id == 0 {
            // The thread never became ready; don't block joining it. Hooks are
            // removed automatically when the process exits.
            return;
        }
        unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }.log_err();
        thread.join().ok();
    }
}

/// Installs the keyboard hook while a Zed window is focused and removes it when
/// focus leaves the app, so the global low-level hook isn't in the system-wide
/// input path while Zed sits in the background. No-op when no hook thread is
/// running (headless, or a Remote Desktop session where the hook is disabled).
pub(crate) fn set_clipboard_history_hook_focus(focused: bool) {
    let thread_id = HOOK_THREAD_ID.load(Ordering::SeqCst);
    if thread_id == 0 {
        return;
    }
    let message = if focused {
        MSG_INSTALL_HOOK
    } else {
        MSG_UNINSTALL_HOOK
    };
    unsafe { PostThreadMessageW(thread_id, message, WPARAM(0), LPARAM(0)) }.log_err();
}

fn hook_thread_main() {
    // Force a message queue to exist on this thread before publishing our id, so
    // `PostThreadMessageW` calls from the UI thread have somewhere to land.
    let mut msg = MSG::default();
    let _ = unsafe { PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE) };
    HOOK_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);

    // Zed is the foreground app at launch, so install immediately; the UI thread
    // toggles this as focus changes (see `set_clipboard_history_hook_focus`).
    let mut hook = HookGuard::default();
    hook.install();

    loop {
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        // `GetMessageW` returns 0 on `WM_QUIT` and -1 on error.
        if result.0 <= 0 {
            break;
        }
        match msg.message {
            MSG_INSTALL_HOOK => hook.install(),
            MSG_UNINSTALL_HOOK => hook.uninstall(),
            _ => {}
        }
    }

    HOOK_THREAD_ID.store(0, Ordering::SeqCst);
    // `hook` drops here, uninstalling the hook if it is still installed.
}

/// Holds the installed hook, if any, and uninstalls it on drop.
#[derive(Default)]
struct HookGuard(Option<HHOOK>);

impl HookGuard {
    fn install(&mut self) {
        if self.0.is_some() {
            return;
        }
        self.0 = unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(hook_proc),
                Some(get_module_handle().into()),
                0,
            )
        }
        .context("installing clipboard history keyboard hook")
        .log_err();
    }

    fn uninstall(&mut self) {
        if let Some(hook) = self.0.take() {
            unsafe { UnhookWindowsHookEx(hook) }.log_err();
        }
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        self.uninstall();
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code as u32 == HC_ACTION {
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if is_injected_ctrl_v(event) {
            let foreground = unsafe { GetForegroundWindow() };
            if is_our_window(foreground) {
                // Swallow the synthetic keystroke (both the key-down and the key-up)
                // so it never reaches the keymap, and have the window perform a
                // semantic paste instead.
                if wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN {
                    unsafe { PostMessageW(Some(foreground), WM_GPUI_PASTE, WPARAM(0), LPARAM(0)) }
                        .log_err();
                }
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

// Note that this also matches injected Ctrl+V from other software (macro tools,
// remote desktop, etc.), not just the clipboard history flyout — Windows doesn't
// identify the injector. Treating any injected Ctrl+V as "the sender wants to
// paste" is the desired behavior in practice.
fn is_injected_ctrl_v(event: &KBDLLHOOKSTRUCT) -> bool {
    event.vkCode == VK_V.0 as u32
        && event.flags.0 & LLKHF_INJECTED.0 != 0
        && is_key_down(VK_CONTROL)
        && !is_key_down(VK_SHIFT)
        && !is_key_down(VK_MENU)
}

fn is_key_down(key: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(key.0 as i32) as u16 & 0x8000 != 0 }
}

fn is_our_window(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    // The hook is global, so another Zed process's window (which has the same
    // class name) can be foreground. Only handle windows owned by this process —
    // otherwise we'd swallow another instance's keystroke and post a paste message
    // cross-process to it.
    let mut process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id != unsafe { GetCurrentProcessId() } {
        return false;
    }
    let mut class_name = [0u16; 32];
    let len = unsafe { GetClassNameW(hwnd, &mut class_name) } as usize;
    let expected = unsafe { WINDOW_CLASS_NAME.as_wide() };
    len == expected.len() && class_name[..len] == *expected
}
