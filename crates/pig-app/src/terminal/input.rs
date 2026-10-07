//! Keyboard mapping and IME input handling.
//!
//! Core ported from tty7 `src/terminal/input.rs`: keystroke_to_bytes (kitty
//! CSI-u plus the legacy named-key table plus xterm modifier params plus
//! Ctrl-letter C0 mapping) and TerminalInputHandler.
//! Dropped parts:
//! - the `local_conpty` branch (ConPTY Ctrl+J special encoding); macOS first,
//!   Windows left as a TODO;
//! - `reshape_option_keystroke` / `defer_to_ime` / `meta_chord_bypasses_ime`:
//!   Option-as-Meta relies on a gpui fork patch in tty7
//!   (`prefers_ime_for_printable_keys`); gpui-pre lacks that API, so the default
//!   IME path is used (printable chars go straight to the PTY via key_char).

use alacritty_terminal::term::TermMode;
use gpui_kit::{App, Bounds, InputHandler, Keystroke, Pixels, UTF16Selection, Window};

use super::view::TerminalView;

/// Terminal state that affects key encoding: the kitty keyboard protocol flags
/// plus the DECCKM application cursor keys mode (see KeyFlags in tty7 input.rs)
#[derive(Clone, Copy, Default)]
pub(crate) struct KeyFlags {
    disambiguate: bool,
    report_all_keys: bool,
    report_text: bool,
    /// DECCKM. ncurses programs that enable it via smkx only accept arrow keys in SS3 form
    app_cursor: bool,
}

impl KeyFlags {
    pub(crate) fn from_mode(mode: &TermMode) -> Self {
        Self {
            disambiguate: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
            report_all_keys: mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
            report_text: mode.contains(TermMode::REPORT_ASSOCIATED_TEXT),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
        }
    }

    fn kitty_active(self) -> bool {
        self.disambiguate || self.report_all_keys
    }

    fn app_cursor(self) -> bool {
        self.app_cursor
    }
}

pub(crate) fn keystroke_to_bytes(ks: &Keystroke, flags: KeyFlags) -> Option<Vec<u8>> {
    if flags.kitty_active()
        && !ks.modifiers.platform
        && let Some(bytes) = encode_kitty(ks, flags)
    {
        return Some(bytes);
    }
    legacy_keystroke_to_bytes(ks, flags)
}

/// xterm's modifier parameter: 1 + shift + 2*alt + 4*ctrl.
/// The platform (cmd) modifier has no xterm encoding and is deliberately excluded.
fn xterm_mods(m: &gpui_kit::Modifiers) -> u32 {
    1 + u32::from(m.shift) + 2 * u32::from(m.alt) + 4 * u32::from(m.control)
}

fn encode_kitty(ks: &Keystroke, kitty: KeyFlags) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    let mods = xterm_mods(m);

    if ks.key.as_str() == "escape" {
        return Some(csi_u(27, mods, None));
    }

    let legacy_ctrl_code = match ks.key.as_str() {
        "enter" => Some(13u32),
        "tab" => Some(9),
        "backspace" => Some(127),
        _ => None,
    };
    if let Some(code) = legacy_ctrl_code {
        if mods == 1 && !kitty.report_all_keys {
            return None;
        }
        return Some(csi_u(code, mods, None));
    }

    // F3 is the only functional key the kitty protocol and terminfo do not
    // share (see the long note at tty7 input.rs:165): programs that negotiated
    // the protocol only take the CSI 13~ form, while the legacy path still sends \x1bOR.
    if ks.key.as_str() == "f3" {
        let s = if mods == 1 {
            "\x1b[13~".to_string()
        } else {
            format!("\x1b[13;{mods}~")
        };
        return Some(s.into_bytes());
    }

    if let Some(seq) = functional_key(ks.key.as_str(), mods, kitty.app_cursor()) {
        return Some(seq);
    }

    if let Some(code) = kitty_function_key(ks.key.as_str()) {
        return Some(csi_u(code, mods, None));
    }

    let modified = m.control || m.alt;
    if (modified || kitty.report_all_keys)
        && let Some(code) = text_key_code(ks)
    {
        let text = kitty.report_text.then(|| associated_text(ks)).flatten();
        return Some(csi_u(code, mods, text.as_deref()));
    }

    None
}

fn csi_u(code: u32, mods: u32, text: Option<&[u32]>) -> Vec<u8> {
    let mut s = format!("\x1b[{code}");
    match text {
        Some(cps) => {
            let joined = cps.iter().map(u32::to_string).collect::<Vec<_>>().join(":");
            s.push_str(&format!(";{mods};{joined}"));
        }
        None if mods != 1 => s.push_str(&format!(";{mods}")),
        None => {}
    }
    s.push('u');
    s.into_bytes()
}

/// Cursor/editing/function keys, encoded per the `xterm-256color` terminfo we
/// advertise; these are exactly the sequences ncurses compares byte for byte.
///
/// Unmodified arrow keys follow DECCKM: normally CSI A, SS3 A when application
/// cursor keys mode is on. With modifiers it is always CSI 1;<mods>A (xterm
/// ignores DECCKM under modifiers).
fn functional_key(key: &str, mods: u32, app_cursor: bool) -> Option<Vec<u8>> {
    let letter = match key {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    };
    if let Some(l) = letter {
        let s = match (mods, app_cursor) {
            (1, false) => format!("\x1b[{l}"),
            (1, true) => format!("\x1bO{l}"),
            _ => format!("\x1b[1;{mods}{l}"),
        };
        return Some(s.into_bytes());
    }
    if let Some(form) = function_key(key) {
        let s = match form {
            // kf1=\EOP .. kf4=\EOS; with modifiers go CSI 1;<mods> (kf13=\E[1;2P, i.e. Shift+F1)
            FunctionKey::Ss3(l) if mods == 1 => format!("\x1bO{l}"),
            FunctionKey::Ss3(l) => format!("\x1b[1;{mods}{l}"),
            FunctionKey::Tilde(n) if mods == 1 => format!("\x1b[{n}~"),
            FunctionKey::Tilde(n) => format!("\x1b[{n};{mods}~"),
        };
        return Some(s.into_bytes());
    }
    let num = match key {
        "insert" => Some(2u32),
        "delete" => Some(3),
        "pageup" => Some(5),
        "pagedown" => Some(6),
        _ => None,
    };
    if let Some(n) = num {
        let s = if mods != 1 {
            format!("\x1b[{n};{mods}~")
        } else {
            format!("\x1b[{n}~")
        };
        return Some(s.into_bytes());
    }
    None
}

/// The two shapes xterm-256color gives functional keys
enum FunctionKey {
    /// Unmodified SS3 <letter>, with modifiers CSI 1;<mods> <letter>
    Ss3(char),
    /// CSI <n>~ or CSI <n>;<mods>~
    Tilde(u32),
}

/// f1..f12: the key names gpui uses uniformly across platforms, and exactly the
/// range of keys xterm-256color natively defines. Numbering follows the PC-style
/// table (starting at 15, skipping 16 and 22, the VT220 keyboard's do/help);
/// guessing a contiguous range is the classic bug of sending F6 as F5. F13 and up
/// are deliberately excluded: in terminfo, kf13+ are the modified forms of
/// F1..F8, and only the kitty protocol can represent them unambiguously
/// (see kitty_function_key).
fn function_key(key: &str) -> Option<FunctionKey> {
    Some(match key {
        "f1" => FunctionKey::Ss3('P'),
        "f2" => FunctionKey::Ss3('Q'),
        "f3" => FunctionKey::Ss3('R'),
        "f4" => FunctionKey::Ss3('S'),
        "f5" => FunctionKey::Tilde(15),
        "f6" => FunctionKey::Tilde(17),
        "f7" => FunctionKey::Tilde(18),
        "f8" => FunctionKey::Tilde(19),
        "f9" => FunctionKey::Tilde(20),
        "f10" => FunctionKey::Tilde(21),
        "f11" => FunctionKey::Tilde(23),
        "f12" => FunctionKey::Tilde(24),
        _ => return None,
    })
}

/// f13..f24 can only go through the kitty protocol (private-use code points 57376..57387)
fn kitty_function_key(key: &str) -> Option<u32> {
    let n: u32 = key.strip_prefix('f')?.parse().ok()?;
    (13..=24).contains(&n).then(|| 57376 + (n - 13))
}

fn text_key_code(ks: &Keystroke) -> Option<u32> {
    match ks.key.as_str() {
        "space" => Some(0x20),
        key => {
            let mut chars = key.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(c.to_ascii_lowercase() as u32)
        }
    }
}

fn associated_text(ks: &Keystroke) -> Option<Vec<u32>> {
    let ch = ks.key_char.as_deref()?;
    let cps: Vec<u32> = ch
        .chars()
        .map(|c| c as u32)
        .filter(|&c| c >= 0x20 && !(0x7f..=0x9f).contains(&c))
        .collect();
    (!cps.is_empty()).then_some(cps)
}

/// The C0 control byte for a Ctrl+<key> chord.
///
/// Letters fold to 0x01..=0x1A; the rest is the VT-220 table (chapter 3.2.5):
/// digits 2..8 plus the punctuation on the same key positions (for Ctrl+^ typed
/// as Ctrl+Shift+6, shift is already consumed). Ctrl+/ is not in the table; it
/// is the US convention inherited from xterm (vim's <C-/>). Ctrl+- is
/// deliberately excluded: outside macOS it shrinks the font size, and readline's
/// undo is served by Ctrl+_.
fn ctrl_c0(key: &str) -> Option<u8> {
    if let [b] = key.as_bytes()
        && b.is_ascii_alphabetic()
    {
        return Some(b.to_ascii_uppercase() & 0x1f);
    }
    Some(match key {
        "space" | "2" | "@" => 0x00,
        "3" | "[" => 0x1b,
        "4" | "\\" => 0x1c,
        "5" | "]" => 0x1d,
        "6" | "^" => 0x1e,
        "7" | "_" | "/" => 0x1f,
        "8" | "?" => 0x7f,
        _ => return None,
    })
}

fn legacy_keystroke_to_bytes(ks: &Keystroke, flags: KeyFlags) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    let key = ks.key.as_str();

    if m.control
        && !m.platform
        && let Some(b) = ctrl_c0(key)
    {
        if b == b'\n' && !m.alt && !m.shift {
            // Ctrl+J = LF. TODO(Windows): ConPTY decodes a bare LF as
            // Ctrl+Enter; tty7 sends a special APC sequence for that
            // (input.rs:43 legacy_newline_bytes), to be added when the Windows
            // branch is validated
            return Some(b"\n".to_vec());
        }
        if m.alt {
            return Some(vec![0x1b, b]);
        }
        return Some(vec![b]);
    }

    if let Some(seq) = functional_key(key, xterm_mods(m), flags.app_cursor()) {
        return Some(seq);
    }

    let seq: Option<&[u8]> = match key {
        "enter" => Some(b"\r"),
        "tab" => Some(b"\t"),
        "backspace" => Some(b"\x7f"),
        "escape" => Some(b"\x1b"),
        _ => None,
    };
    if let Some(seq) = seq {
        if m.alt {
            let mut v = vec![0x1b];
            v.extend_from_slice(seq);
            return Some(v);
        }
        return Some(seq.to_vec());
    }

    if m.platform {
        return None;
    }
    // Printable chars prefer key_char (IME commits go through InputHandler, not key_down)
    if let Some(ch) = &ks.key_char
        && !ch.is_empty()
    {
        let mut v = Vec::new();
        if m.alt {
            v.push(0x1b);
        }
        v.extend_from_slice(ch.as_bytes());
        return Some(v);
    }
    None
}

/// IME input handler (see TerminalInputHandler in tty7 input.rs).
/// Registered via `window.handle_input` at every paint; only effective while
/// this terminal holds focus.
pub(crate) struct TerminalInputHandler {
    view: gpui_kit::Entity<TerminalView>,
    /// Screen coordinates of the cursor cell, the anchor for the IME candidate window (with no cursor the whole input goes dead; the IME uses the window default position)
    cursor_bounds: Option<Bounds<Pixels>>,
}

impl TerminalInputHandler {
    pub(crate) fn new(
        view: gpui_kit::Entity<TerminalView>,
        cursor_bounds: Option<Bounds<Pixels>>,
    ) -> Self {
        Self {
            view,
            cursor_bounds,
        }
    }
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        let marked = &self.view.read(cx).marked_text;
        if marked.is_empty() {
            None
        } else {
            Some(0..marked.encode_utf16().count())
        }
    }

    fn text_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _adjusted: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut App,
    ) {
        let text = text.to_string();
        self.view.update(cx, |view, cx| {
            view.clear_marked_text(cx);
            view.input_text(&text, cx);
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        let new_text = new_text.to_string();
        self.view
            .update(cx, |view, cx| view.set_marked_text(new_text, cx));
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.view.update(cx, |view, cx| view.clear_marked_text(cx));
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let mut bounds = self.cursor_bounds?;
        let cell_width = self.view.read(cx).cell_width;
        bounds.origin.x += cell_width * range_utf16.start as f32;
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui_kit::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(ks: &Keystroke) -> Option<Vec<u8>> {
        keystroke_to_bytes(ks, KeyFlags::default())
    }

    fn ks(mods: gpui_kit::Modifiers, key: &str, key_char: Option<&str>) -> Keystroke {
        Keystroke {
            modifiers: mods,
            key: key.to_string(),
            key_char: key_char.map(str::to_string),
        }
    }

    #[test]
    fn 映射ctrl字母() {
        let ctrl = gpui_kit::Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(ctrl, "c", None)), Some(vec![0x03]));
        assert_eq!(legacy(&ks(ctrl, "a", None)), Some(vec![0x01]));
        assert_eq!(legacy(&ks(ctrl, "[", None)), Some(vec![0x1b]));
        assert_eq!(legacy(&ks(ctrl, "2", None)), Some(vec![0x00]));
    }

    #[test]
    fn 映射命名键与alt前缀() {
        let none = gpui_kit::Modifiers::default();
        assert_eq!(legacy(&ks(none, "enter", None)), Some(b"\r".to_vec()));
        assert_eq!(legacy(&ks(none, "up", None)), Some(b"\x1b[A".to_vec()));
        let alt = gpui_kit::Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(alt, "up", None)), Some(b"\x1b[1;3A".to_vec()));
        assert_eq!(legacy(&ks(alt, "enter", None)), Some(b"\x1b\r".to_vec()));
    }

    #[test]
    fn 可打印字符走key_char_cmd除外() {
        let none = gpui_kit::Modifiers::default();
        assert_eq!(legacy(&ks(none, "a", Some("a"))), Some(b"a".to_vec()));
        let cmd = gpui_kit::Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(cmd, "a", Some("a"))), None);
    }

    #[test]
    fn kitty协议编码修饰键() {
        let flags = KeyFlags {
            disambiguate: true,
            report_all_keys: false,
            report_text: false,
            app_cursor: false,
        };
        let shift = gpui_kit::Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks(shift, "enter", None), flags),
            Some(b"\x1b[13;2u".to_vec())
        );
        let ctrl = gpui_kit::Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks(ctrl, "j", None), flags),
            Some(b"\x1b[106;5u".to_vec())
        );
    }

    #[test]
    fn 应用光标键模式() {
        let flags = KeyFlags {
            app_cursor: true,
            ..Default::default()
        };
        let none = gpui_kit::Modifiers::default();
        assert_eq!(
            keystroke_to_bytes(&ks(none, "up", None), flags),
            Some(b"\x1bOA".to_vec())
        );
    }
}
