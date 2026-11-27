//! Labs › Learn from corrections on Windows: the field a dictation was just
//! pasted into, read back through UI Automation while the person fixes it.
//!
//! The output worker takes only Win32 identity right after the paste (the
//! window in front, its process, and its keyboard-focused child window),
//! which is cheap and never blocks on another app. The watch thread then
//! opens UI Automation on its own COM apartment and takes the focused
//! element as the field, but only while that identity still holds, so a
//! focus change between the paste and the first read watches nothing.
//!
//! Every read is bounded by UI Automation's connection and transaction
//! timeouts, so a hung app costs at most [`CALL_TIMEOUT_MS`] per call and
//! the watch then ends as if the field vanished. Hex's own window, password
//! fields, and fields longer than [`MAX_FIELD_CHARS`] are never read. Windows
//! asks for no permission here, but it refuses UI Automation into a window
//! running as administrator while Hex does not (User Interface Privilege
//! Isolation), and such a window takes no Ctrl+V from Hex either.

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement,
    IUIAutomationTextPattern, IUIAutomationValuePattern, UIA_TextPatternId, UIA_ValuePatternId,
};
use windows::core::Interface;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
};

/// Longest field text the watch reads, in UTF-16 units, as on macOS. A
/// longer field is a document, not a correction, so it is skipped rather
/// than truncated.
const MAX_FIELD_CHARS: usize = 20_000;

/// How long one UI Automation call may wait on the app before it fails.
const CALL_TIMEOUT_MS: u32 = 1_000;

/// The window a paste reached and its keyboard focus, taken right after the
/// paste on the output worker. Plain handles and ids, so it moves to the
/// watch thread; a reused handle is told apart by its process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PasteTarget {
    window: isize,
    pid: u32,
    focus: isize,
}

impl PasteTarget {
    /// The window in front and its focused child window. `None` when nothing
    /// is in front or it is Hex's own window.
    pub(crate) fn capture() -> Option<Self> {
        // SAFETY: plain Win32 queries about the window in front.
        let target = unsafe { Self::read() }?;
        // SAFETY: no arguments.
        let own = unsafe { GetCurrentProcessId() };
        (target.pid != own).then_some(target)
    }

    /// Whether the same window is in front with the same focused child.
    fn holds(&self) -> bool {
        // SAFETY: plain Win32 queries about the window in front.
        unsafe { Self::read() }.is_some_and(|now| now == *self)
    }

    unsafe fn read() -> Option<Self> {
        unsafe {
            let window = GetForegroundWindow();
            if window.is_null() {
                return None;
            }
            let mut pid = 0_u32;
            let thread = GetWindowThreadProcessId(window, &mut pid);
            if pid == 0 || thread == 0 {
                return None;
            }
            let mut info = GUITHREADINFO {
                cbSize: size_of::<GUITHREADINFO>() as u32,
                ..Default::default()
            };
            // A window with no focused child (some Electron and UWP hosts)
            // still has a field; its identity is then the window alone.
            let focus = if GetGUIThreadInfo(thread, &mut info) != 0 {
                info.hwndFocus as isize
            } else {
                0
            };
            Some(Self {
                window: window as isize,
                pid,
                focus,
            })
        }
    }
}

/// The field being watched, on the watch thread that opened it. Its UI
/// Automation objects belong to this thread's COM apartment, so it is never
/// moved; dropping it releases them before COM.
pub(crate) struct PasteFocus {
    target: PasteTarget,
    field: IUIAutomationElement,
    automation: IUIAutomation,
    _com: Apartment,
}

impl PasteFocus {
    /// Opens UI Automation on this thread and takes the focused element as
    /// the field, while `target` is still in front with the same focus and
    /// the element belongs to its process. `None` otherwise, for a password
    /// field, or when UI Automation is unavailable.
    pub(crate) fn open(target: PasteTarget) -> Option<Self> {
        let com = Apartment::enter();
        let automation = automation()?;
        if !target.holds() {
            return None;
        }
        // SAFETY: read-only queries on this thread's UI Automation instance.
        let field = unsafe { automation.GetFocusedElement() }.ok()?;
        let pid = unsafe { field.CurrentProcessId() }.ok()?;
        if u32::try_from(pid).ok() != Some(target.pid) || is_password(&field) {
            return None;
        }
        Some(Self {
            target,
            field,
            automation,
            _com: com,
        })
    }

    /// Whether the same window is in front, with the same focused child
    /// window and the same focused element.
    pub(crate) fn unchanged(&self) -> bool {
        if !self.target.holds() {
            return false;
        }
        // SAFETY: read-only queries on this thread's UI Automation instance;
        // both elements came from it.
        unsafe {
            let Ok(now) = self.automation.GetFocusedElement() else {
                return false;
            };
            self.automation
                .CompareElements(&now, &self.field)
                .is_ok_and(|same| same.as_bool())
        }
    }

    /// The field's whole text. `None` for a password field, a field with
    /// neither a text nor a value pattern, or one longer than
    /// [`MAX_FIELD_CHARS`]. The text pattern is asked for at most one unit
    /// past the limit, so a document is never read in full just to be
    /// dropped; a value has no length to ask first, so it is read and then
    /// measured, as macOS measures `AXValue` when the app names no count.
    pub(crate) fn field_text(&self) -> Option<String> {
        if is_password(&self.field) {
            return None;
        }
        // SAFETY: read-only queries on an element from this thread's
        // instance; each BSTR is owned and freed here.
        unsafe {
            if let Ok(pattern) = self
                .field
                .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            {
                let range = pattern.DocumentRange().ok()?;
                let text = range.GetText(MAX_FIELD_CHARS as i32 + 1).ok()?;
                return fits(text.len()).then(|| text.to_string());
            }
            let pattern = self
                .field
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .ok()?;
            let value = pattern.CurrentValue().ok()?;
            fits(value.len()).then(|| value.to_string())
        }
    }
}

/// Whether a field of `units` UTF-16 units is short enough to read.
const fn fits(units: usize) -> bool {
    units <= MAX_FIELD_CHARS
}

/// Whether the element holds a password. An element that can't answer is
/// treated as one, so an unknown field is never read.
fn is_password(element: &IUIAutomationElement) -> bool {
    // SAFETY: a read-only property query.
    unsafe { element.CurrentIsPassword() }.map_or(true, |password| password.as_bool())
}

/// This thread's UI Automation instance with bounded calls: the Windows 8
/// object when there is one, whose timeouts are set, else the original.
fn automation() -> Option<IUIAutomation> {
    // SAFETY: COM is entered on this thread by the caller; the instance is
    // created and used on it.
    unsafe {
        if let Ok(automation) =
            CoCreateInstance::<_, IUIAutomation2>(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
        {
            let _ = automation.SetConnectionTimeout(CALL_TIMEOUT_MS);
            let _ = automation.SetTransactionTimeout(CALL_TIMEOUT_MS);
            return automation.cast::<IUIAutomation>().ok();
        }
        CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()
    }
}

/// This thread's membership in the multithreaded COM apartment, left when
/// dropped if entering it succeeded.
struct Apartment(bool);

impl Apartment {
    fn enter() -> Self {
        // SAFETY: initializes COM for this thread only.
        Self(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx above, on the
            // same thread, after the owner released its COM objects.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_past_the_limit_is_skipped_not_cut() {
        assert!(fits(0));
        assert!(fits(MAX_FIELD_CHARS));
        assert!(!fits(MAX_FIELD_CHARS + 1));
    }

    #[test]
    fn the_target_is_the_window_its_process_and_its_focus() {
        let target = PasteTarget {
            window: 10,
            pid: 20,
            focus: 30,
        };
        assert_ne!(
            target,
            PasteTarget {
                focus: 31,
                ..target
            }
        );
        assert_ne!(target, PasteTarget { pid: 21, ..target });
        assert_ne!(
            target,
            PasteTarget {
                window: 11,
                ..target
            }
        );
    }
}
