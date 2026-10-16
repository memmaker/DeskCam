//! System-wide hotkey via Carbon's RegisterEventHotKey. Unlike NSEvent global
//! monitors this needs no Accessibility permission, and it swallows the key so
//! the stop combo never reaches the game being recorded.

use std::ffi::c_void;
use std::sync::OnceLock;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type EventHandlerUPP = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: EventHandlerUPP,
        num_types: u32,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: *mut c_void,
        options: u32,
        out_ref: *mut *mut c_void,
    ) -> i32;
    fn UnregisterEventHotKey(hotkey: *mut c_void) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | s[3] as u32
}

const CMD: u32 = 0x0100;
const SHIFT: u32 = 0x0200;
const OPTION: u32 = 0x0800;
const CONTROL: u32 = 0x1000;

static CALLBACK: OnceLock<fn()> = OnceLock::new();

extern "C" fn on_hotkey(_: *mut c_void, _: *mut c_void, _: *mut c_void) -> i32 {
    if let Some(cb) = CALLBACK.get() {
        cb();
    }
    0
}

/// Installs the Carbon event handler once; `cb` runs on the main thread.
pub fn install(cb: fn()) {
    if CALLBACK.set(cb).is_err() {
        return;
    }
    let spec = EventTypeSpec { event_class: fourcc(b"keyb"), event_kind: 5 /* kEventHotKeyPressed */ };
    unsafe {
        InstallEventHandler(
            GetApplicationEventTarget(),
            on_hotkey,
            1,
            &spec,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

pub struct HotKey(*mut c_void);

impl HotKey {
    pub fn register(spec: &str) -> Result<HotKey, String> {
        let (key, mods) = parse(spec)?;
        let mut out = std::ptr::null_mut();
        let id = EventHotKeyID { signature: fourcc(b"DCam"), id: 1 };
        let err = unsafe { RegisterEventHotKey(key, mods, id, GetApplicationEventTarget(), 0, &mut out) };
        if err != 0 {
            return Err(format!("hotkey {spec} unavailable (error {err})"));
        }
        Ok(HotKey(out))
    }
}

impl Drop for HotKey {
    fn drop(&mut self) {
        unsafe { UnregisterEventHotKey(self.0) };
    }
}

/// Parses "cmd+alt+r" style strings into (virtual keycode, Carbon modifier mask).
pub fn parse(spec: &str) -> Result<(u32, u32), String> {
    let mut mods = 0;
    let mut key = None;
    for part in spec.to_lowercase().split('+').map(str::trim) {
        match part {
            "cmd" | "command" | "⌘" => mods |= CMD,
            "shift" | "⇧" => mods |= SHIFT,
            "alt" | "opt" | "option" | "⌥" => mods |= OPTION,
            "ctrl" | "control" | "⌃" => mods |= CONTROL,
            k => key = Some(keycode(k).ok_or_else(|| format!("unknown key '{k}' in hotkey '{spec}'"))?),
        }
    }
    key.map(|k| (k, mods)).ok_or_else(|| format!("hotkey '{spec}' has no key"))
}

/// Human-readable form, e.g. "⌥⌘R".
pub fn display(spec: &str) -> String {
    let mut m = String::new();
    let mut k = String::new();
    for part in spec.split('+').map(str::trim) {
        match part.to_lowercase().as_str() {
            "ctrl" | "control" => m.push('⌃'),
            "alt" | "opt" | "option" => m.push('⌥'),
            "shift" => m.push('⇧'),
            "cmd" | "command" => m.push('⌘'),
            _ => k = part.to_uppercase(),
        }
    }
    // macOS convention orders modifiers ⌃⌥⇧⌘.
    let order = ['⌃', '⌥', '⇧', '⌘'];
    let mut sorted: Vec<char> = m.chars().collect();
    sorted.sort_by_key(|c| order.iter().position(|o| o == c));
    sorted.into_iter().collect::<String>() + &k
}

fn keycode(k: &str) -> Option<u32> {
    const LETTERS: [(char, u32); 26] = [
        ('a', 0), ('b', 11), ('c', 8), ('d', 2), ('e', 14), ('f', 3), ('g', 5), ('h', 4), ('i', 34),
        ('j', 38), ('k', 40), ('l', 37), ('m', 46), ('n', 45), ('o', 31), ('p', 35), ('q', 12),
        ('r', 15), ('s', 1), ('t', 17), ('u', 32), ('v', 9), ('w', 13), ('x', 7), ('y', 16), ('z', 6),
    ];
    const DIGITS: [u32; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    const FKEYS: [u32; 15] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111, 105, 107, 113];
    let mut chars = k.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if let Some(&(_, code)) = LETTERS.iter().find(|(l, _)| *l == c) {
            return Some(code);
        }
        if let Some(d) = c.to_digit(10) {
            return Some(DIGITS[d as usize]);
        }
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<usize>().ok()) {
        return FKEYS.get(n.wrapping_sub(1)).copied();
    }
    match k {
        "space" => Some(49),
        "escape" | "esc" => Some(53),
        "return" | "enter" => Some(36),
        "tab" => Some(48),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        assert_eq!(parse("cmd+alt+r"), Ok((15, CMD | OPTION)));
        assert_eq!(parse("ctrl+shift+F12"), Ok((111, CONTROL | SHIFT)));
        assert_eq!(parse("f9"), Ok((101, 0)));
        assert!(parse("cmd+alt").is_err());
        assert!(parse("cmd+foo").is_err());
        assert_eq!(display("cmd+alt+r"), "⌥⌘R");
    }
}
