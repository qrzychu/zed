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
//! that information is gone. So we install such a hook for the lifetime of the
//! message loop: when an injected `Ctrl+V` is headed for one of our windows, we
//! swallow the keystroke and post [`WM_GPUI_PASTE`] instead, which GPUI dispatches
//! as the application's `OsAction::Paste` action, independent of the user's keymap.

use ::util::ResultExt;
use anyhow::Context;
use windows::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT, VK_V,
        },
        WindowsAndMessaging::{
            CallNextHookEx, GetClassNameW, GetForegroundWindow, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
            LLKHF_INJECTED, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
            WM_KEYDOWN, WM_SYSKEYDOWN,
        },
    },
};

use crate::{HWND, WINDOW_CLASS_NAME, WM_GPUI_PASTE, get_module_handle};

/// Owns the low-level keyboard hook; uninstalls it on drop.
pub(crate) struct ClipboardHistoryPasteHook(HHOOK);

impl ClipboardHistoryPasteHook {
    /// Installs the hook on the current thread, which must pump messages: the
    /// system delivers the hook callback through this thread's message loop.
    pub(crate) fn install() -> Option<Self> {
        unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(hook_proc),
                Some(get_module_handle().into()),
                0,
            )
        }
        .context("installing clipboard history keyboard hook")
        .log_err()
        .map(Self)
    }
}

impl Drop for ClipboardHistoryPasteHook {
    fn drop(&mut self) {
        unsafe { UnhookWindowsHookEx(self.0) }.log_err();
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code as u32 == HC_ACTION {
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if is_injected_ctrl_v(event) {
            let foreground = unsafe { GetForegroundWindow() };
            if is_gpui_window(foreground) {
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

fn is_gpui_window(hwnd: HWND) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let mut class_name = [0u16; 32];
    let len = unsafe { GetClassNameW(hwnd, &mut class_name) } as usize;
    let expected = unsafe { WINDOW_CLASS_NAME.as_wide() };
    len == expected.len() && class_name[..len] == *expected
}
