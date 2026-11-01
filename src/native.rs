//! Native messaging host for the browser extension.
//!
//! The extension opens a port only while one of its balls is on screen. Each
//! message asks for a ball; this process relays "click" or "dismiss" back so
//! the extension can switch to the right tab. It exits when the port closes.

use std::io::{Read, Write};
use std::mem::zeroed;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::{wide, COPYDATA_MAGIC, COPYDATA_REMOVE};

static REPLIED: AtomicBool = AtomicBool::new(false);

pub unsafe fn run() {
    let class = wide("BallPopRelay");
    let mut wc: WNDCLASSW = zeroed();
    wc.hInstance = GetModuleHandleW(null());
    wc.lpfnWndProc = Some(relay_proc);
    wc.lpszClassName = class.as_ptr();
    RegisterClassW(&wc);
    let relay = CreateWindowExW(
        0, class.as_ptr(), null(), 0, 0, 0, 0, 0,
        HWND_MESSAGE, null_mut(), wc.hInstance, null(),
    );
    let relay_id = relay as isize;
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        while let Some(msg) = read_message(&mut stdin) {
            let agent: String = field(&msg, "agent")
                .unwrap_or("agent")
                .to_lowercase()
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            let icon = field(&msg, "icon").and_then(|b64| save_icon(&agent, b64)).unwrap_or_default();
            crate::deliver(&agent, null_mut(), &icon, relay_id as HWND, true);
        }
        PostMessageW(relay_id as HWND, WM_CLOSE, 0, 0); // port closed
    });
    let mut msg: MSG = zeroed();
    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
        DispatchMessageW(&msg);
    }
    std::process::exit(0); // the stdin thread may still be blocked reading
}

unsafe extern "system" fn relay_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COPYDATA => {
            let cds = &*(lparam as *const COPYDATASTRUCT);
            if cds.dwData != COPYDATA_MAGIC {
                return 0;
            }
            let data = std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2);
            let event = String::from_utf16_lossy(data);
            if !REPLIED.swap(true, Ordering::SeqCst) {
                write_message(&format!("{{\"event\":\"{event}\"}}"));
            }
            1
        }
        WM_CLOSE => {
            let host = crate::find_host();
            if !host.is_null() {
                crate::send_payload(host, COPYDATA_REMOVE, &(hwnd as isize).to_string());
            }
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Native messaging framing: 4-byte little-endian length, then UTF-8 JSON.
fn read_message(r: &mut impl Read) -> Option<String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).ok()?;
    let len = u32::from_le_bytes(len) as usize;
    if len > 8 << 20 {
        return None;
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    String::from_utf8(body).ok()
}

fn write_message(json: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&(json.len() as u32).to_le_bytes());
    let _ = out.write_all(json.as_bytes());
    let _ = out.flush();
}

/// Reads a string field from our own flat messages ({"agent":"...", ...});
/// the values never contain quotes or escapes.
fn field<'a>(json: &'a str, name: &str) -> Option<&'a str> {
    let rest = &json[json.find(&format!("\"{name}\""))? + name.len() + 2..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(&rest[..rest.find('"')?])
}

/// Saves the site's icon (base64) to %TEMP%\ballpop-icons\<agent>.img.
fn save_icon(agent: &str, b64: &str) -> Option<String> {
    let bytes = base64_decode(b64)?;
    if bytes.is_empty() {
        return None;
    }
    let dir = std::env::temp_dir().join("ballpop-icons");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{agent}.img"));
    if std::fs::read(&path).ok().as_deref() != Some(&bytes[..]) {
        std::fs::write(&path, &bytes).ok()?;
    }
    path.to_str().map(String::from)
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}
