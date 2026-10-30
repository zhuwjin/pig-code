use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::atomic::{AtomicIsize, Ordering},
};

use nsh_platform_windows::{
    ComponentContext, Components, NativeComponent,
    windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
            Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT},
            System::Threading::GetCurrentThreadId,
            UI::{
                HiDpi::GetDpiForWindow,
                WindowsAndMessaging::{
                    CallWindowProcW, DefWindowProcW, EnumThreadWindows, GWLP_WNDPROC,
                    GetClientRect, GetWindowRect, GetWindowTextW, IsWindow, KillTimer,
                    PostMessageW, SetTimer, SetWindowLongPtrW, WM_APP, WM_DPICHANGED, WM_NCDESTROY,
                    WM_PAINT, WM_SIZE, WM_TIMER, WNDPROC,
                },
            },
        },
        core::BOOL,
    },
};

const COMPONENT_NAME: &str = "pig/window-guard";
const LAYOUT_MESSAGE: u32 = WM_APP + 0x51;
const FALLBACK_TIMER: usize = 0x504947;

struct ResizeState {
    original: WNDPROC,
    expected: Cell<(f32, f32)>,
    pending: Cell<bool>,
    forwarding: Cell<bool>,
}

thread_local! {
    static STATES: RefCell<HashMap<isize, Rc<ResizeState>>> = RefCell::new(HashMap::new());
}

static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);

fn state(hwnd: HWND) -> Option<Rc<ResizeState>> {
    STATES.with(|states| states.borrow().get(&(hwnd.0 as isize)).cloned())
}

fn call_original(
    state: &ResizeState,
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { CallWindowProcW(state.original, hwnd, message, wparam, lparam) }
}

fn window_size(hwnd: HWND) -> (f32, f32) {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
    }
    let scale = unsafe { GetDpiForWindow(hwnd) }.max(1) as f32 / 96.0;
    (
        (rect.right - rect.left) as f32 / scale,
        (rect.bottom - rect.top) as f32 / scale,
    )
}

fn same_size(a: (f32, f32), b: (f32, f32)) -> bool {
    (a.0 - b.0).abs() < 0.51 && (a.1 - b.1).abs() < 0.51
}

fn complete_layout(state: &ResizeState, size: (f32, f32)) -> bool {
    state.pending.get() && same_size(state.expected.get(), size)
}

fn trace(action: &str, hwnd: HWND) {
    if std::env::var_os("PIG_RESIZE_TRACE").is_some() {
        let (width, height) = window_size(hwnd);
        eprintln!("resize {action}: {width}x{height}");
    }
}

fn release(hwnd: HWND, state: &ResizeState) {
    if !state.pending.replace(false) {
        return;
    }
    trace("paint", hwnd);
    unsafe {
        let _ = KillTimer(Some(hwnd), FALLBACK_TIMER);
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

fn forward_resize(
    state: &ResizeState,
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if state.forwarding.get() {
        return LRESULT(0);
    }
    if message == WM_SIZE && wparam.0 == 1 {
        return call_original(state, hwnd, message, wparam, lparam);
    }
    trace("defer", hwnd);
    state.pending.set(true);
    unsafe {
        SetTimer(Some(hwnd), FALLBACK_TIMER, 50, None);
    }
    state.forwarding.set(true);
    let mut frame = RECT::default();
    let (message, lparam) = if message == WM_SIZE {
        unsafe {
            let _ = GetWindowRect(hwnd, &mut frame);
        }
        // NSH 0.3.0 paints immediately for WM_SIZE, but only updates the
        // viewport for WM_DPICHANGED. The layout callback re-enables paint.
        (WM_DPICHANGED, LPARAM(&frame as *const _ as isize))
    } else {
        (message, lparam)
    };
    let result = call_original(state, hwnd, message, wparam, lparam);
    state.forwarding.set(false);
    state.expected.set(window_size(hwnd));
    result
}

fn detach(hwnd: HWND) {
    let state = STATES.with(|states| states.borrow_mut().remove(&(hwnd.0 as isize)));
    if let Some(state) = state {
        unsafe {
            if IsWindow(Some(hwnd)).as_bool() {
                SetWindowLongPtrW(
                    hwnd,
                    GWLP_WNDPROC,
                    state.original.map_or(0, |proc| proc as usize) as isize,
                );
            }
            let _ = KillTimer(Some(hwnd), FALLBACK_TIMER);
        }
    }
    if MAIN_WINDOW.load(Ordering::Acquire) == hwnd.0 as isize {
        MAIN_WINDOW.store(0, Ordering::Release);
    }
}

unsafe extern "system" fn resize_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let Some(state) = state(hwnd) else {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    };
    match message {
        WM_SIZE | WM_DPICHANGED => forward_resize(&state, hwnd, message, wparam, lparam),
        WM_PAINT if state.pending.get() => {
            let mut paint = PAINTSTRUCT::default();
            unsafe {
                BeginPaint(hwnd, &mut paint);
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        LAYOUT_MESSAGE => {
            let bits = lparam.0 as u64;
            let size = (
                f32::from_bits((bits >> 32) as u32),
                f32::from_bits(bits as u32),
            );
            if complete_layout(&state, size) {
                trace("layout", hwnd);
                release(hwnd, &state);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == FALLBACK_TIMER => {
            release(hwnd, &state);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let result = call_original(&state, hwnd, message, wparam, lparam);
            detach(hwnd);
            result
        }
        _ => call_original(&state, hwnd, message, wparam, lparam),
    }
}

fn attach(hwnd: HWND) -> bool {
    if state(hwnd).is_some() {
        return true;
    }
    let previous = unsafe {
        SetWindowLongPtrW(
            hwnd,
            GWLP_WNDPROC,
            resize_proc as *const () as usize as isize,
        )
    };
    if previous == 0 {
        return false;
    }
    let original = unsafe { std::mem::transmute::<isize, WNDPROC>(previous) };
    STATES.with(|states| {
        states.borrow_mut().insert(
            hwnd.0 as isize,
            Rc::new(ResizeState {
                original,
                expected: Cell::new(window_size(hwnd)),
                pending: Cell::new(false),
                forwarding: Cell::new(false),
            }),
        );
    });
    MAIN_WINDOW.store(hwnd.0 as isize, Ordering::Release);
    true
}

unsafe extern "system" fn find_main_window(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let mut title = [0u16; 64];
    let length = unsafe { GetWindowTextW(hwnd, &mut title) };
    if length > 0 && String::from_utf16_lossy(&title[..length as usize]) == "Pig Code" {
        attach(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

pub fn layout_committed(width: f32, height: f32) {
    let hwnd = MAIN_WINDOW.load(Ordering::Acquire);
    if hwnd == 0 {
        return;
    }
    let packed = ((width.to_bits() as u64) << 32) | height.to_bits() as u64;
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut _)),
            LAYOUT_MESSAGE,
            WPARAM(0),
            LPARAM(packed as isize),
        );
    }
}

struct WindowGuard {
    hwnd: Cell<Option<isize>>,
}

impl WindowGuard {
    fn new(_context: ComponentContext) -> Self {
        unsafe {
            let _ = EnumThreadWindows(GetCurrentThreadId(), Some(find_main_window), LPARAM(0));
        }
        let hwnd = MAIN_WINDOW.load(Ordering::Acquire);
        Self {
            hwnd: Cell::new((hwnd != 0).then_some(hwnd)),
        }
    }
}

impl NativeComponent for WindowGuard {
    fn set_prop(&mut self, name: &str, value: Option<&serde_json::Value>) {
        let _ = (name, value);
    }
}

impl Drop for WindowGuard {
    fn drop(&mut self) {
        if let Some(hwnd) = self.hwnd.get() {
            detach(HWND(hwnd as *mut _));
        }
    }
}

pub fn register(components: &Components) {
    nsh_app_pig_code_ts::agent::set_layout_callback(layout_committed);
    components.register(
        COMPONENT_NAME,
        Rc::new(|context| Box::new(WindowGuard::new(context))),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ResizeState {
        ResizeState {
            original: None,
            expected: Cell::new((800.0, 600.0)),
            pending: Cell::new(true),
            forwarding: Cell::new(false),
        }
    }

    #[test]
    fn only_the_latest_layout_releases_the_resize_wait() {
        let state = state();
        assert!(!complete_layout(&state, (700.0, 600.0)));
        assert!(state.pending.get());
        assert!(!complete_layout(&state, (800.0, 500.0)));
        assert!(state.pending.get());
        assert!(complete_layout(&state, (800.0, 600.0)));
        release(HWND::default(), &state);
        assert!(!state.pending.get());
    }
}
