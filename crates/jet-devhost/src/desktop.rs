//! Desktop access for smoke tests: screenshots, synthetic input, and finding
//! the window of the build devhost launched.

use std::ffi::c_void;
use std::io;

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetForegroundWindow, GetSystemMetrics, GetWindow, GetWindowRect,
    GetWindowThreadProcessId, IsWindowVisible, PW_RENDERFULLCONTENT, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetForegroundWindow,
};
use windows::core::BOOL;

/// Real pixels in screenshots and window rects, not DPI-virtualized ones.
pub fn init() {
    // Best effort: fails harmlessly if awareness is already set.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// What a screenshot shows.
pub enum Capture {
    /// Everything on screen.
    Desktop,
    /// One window's own contents, even when other windows cover it.
    Window(HWND),
}

/// PNG of `what`.
pub fn screenshot(what: Capture) -> io::Result<Vec<u8>> {
    let r = match what {
        Capture::Desktop => unsafe {
            let (x, y) = (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
            );
            RECT {
                left: x,
                top: y,
                right: x + GetSystemMetrics(SM_CXVIRTUALSCREEN),
                bottom: y + GetSystemMetrics(SM_CYVIRTUALSCREEN),
            }
        },
        Capture::Window(hwnd) => {
            let mut rect = RECT::default();
            unsafe { GetWindowRect(hwnd, &mut rect) }.map_err(io::Error::other)?;
            rect
        }
    };
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return Err(io::Error::other("nothing to capture"));
    }
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bitmap.into());
        let drawn = match what {
            Capture::Desktop => BitBlt(
                mem,
                0,
                0,
                w,
                h,
                Some(screen),
                r.left,
                r.top,
                SRCCOPY | CAPTUREBLT,
            )
            .map_err(io::Error::other),
            // RENDERFULLCONTENT also gets DirectX/flip-model content.
            Capture::Window(hwnd) => {
                if PrintWindow(hwnd, mem, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool() {
                    Ok(())
                } else {
                    Err(io::Error::other("PrintWindow failed"))
                }
            }
        };
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Negative: rows top to bottom.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = GetDIBits(
            mem,
            bitmap,
            0,
            h as u32,
            Some(bgra.as_mut_ptr().cast::<c_void>()),
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        drawn?;
        if lines != h {
            return Err(io::Error::other("GetDIBits failed"));
        }
    }
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
    let mut png_bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut png_bytes, w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(&bgra))
        .map_err(io::Error::other)?;
    Ok(png_bytes)
}

/// The visible top-level window of `pid`, if it has one.
pub fn window_of(pid: u32) -> Option<HWND> {
    struct Search {
        pid: u32,
        found: Option<HWND>,
    }
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the &mut Search passed to EnumWindows below,
        // alive for the whole enumeration.
        let search = unsafe { &mut *(lparam.0 as *mut Search) };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let top_level = unsafe { GetWindow(hwnd, GW_OWNER) }.is_err();
        if pid == search.pid && top_level && unsafe { IsWindowVisible(hwnd) }.as_bool() {
            search.found = Some(hwnd);
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut search = Search { pid, found: None };
    // Stopping early reports an error; `found` says whether it worked.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut search as isize)) };
    search.found
}

/// Bring `hwnd` to the front so synthetic input reaches it, and fail unless
/// it really is in front: `SendInput` types into whatever has the focus, and
/// that must never be someone else's app. Windows only lets the process that
/// got the last input take the foreground, so a harmless Alt tap through
/// `SendInput` makes it ours first.
pub fn focus(hwnd: HWND) -> io::Result<()> {
    const VK_MENU: u16 = 0x12;
    send(&[key(VK_MENU, false), key(VK_MENU, true)])?;
    let _ = unsafe { SetForegroundWindow(hwnd) };
    if unsafe { GetForegroundWindow() } == hwnd {
        Ok(())
    } else {
        Err(io::Error::other(
            "the build's window is not in front (another app kept the foreground); not typing",
        ))
    }
}

/// Type `text` into the foreground window. `\n` presses Enter, `\t` Tab.
pub fn type_text(text: &str) -> io::Result<()> {
    let mut inputs = Vec::new();
    for c in text.chars() {
        match c {
            '\n' | '\r' => tap(&mut inputs, 0x0D),
            '\t' => tap(&mut inputs, 0x09),
            _ => {
                let mut units = [0u16; 2];
                for &unit in c.encode_utf16(&mut units).iter() {
                    inputs.push(unicode(unit, false));
                    inputs.push(unicode(unit, true));
                }
            }
        }
    }
    send(&inputs)
}

/// Press a chord like `ctrl+c`, `shift+tab`, `up` or `f5`.
pub fn press_keys(chord: &str) -> io::Result<()> {
    let mut parts: Vec<&str> = chord.split('+').map(str::trim).collect();
    let main = parts.pop().ok_or_else(|| bad_key(chord))?;
    let mods = parts
        .iter()
        .map(|m| match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Ok(0x11),
            "shift" => Ok(0x10),
            "alt" => Ok(0x12),
            "win" | "super" => Ok(0x5B),
            _ => Err(bad_key(chord)),
        })
        .collect::<io::Result<Vec<u16>>>()?;
    let vk = virtual_key(main).ok_or_else(|| bad_key(chord))?;
    let mut inputs: Vec<INPUT> = mods.iter().map(|&m| key(m, false)).collect();
    tap(&mut inputs, vk);
    inputs.extend(mods.iter().rev().map(|&m| key(m, true)));
    send(&inputs)
}

fn virtual_key(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    let named = match lower.as_str() {
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" => 0x08,
        "space" => 0x20,
        "delete" | "del" => 0x2E,
        "insert" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    let mut chars = lower.chars();
    match (chars.next(), chars.next()) {
        (Some(c @ ('a'..='z' | '0'..='9')), None) => Some(c.to_ascii_uppercase() as u16),
        (Some('f'), Some(_)) => match lower[1..].parse::<u16>() {
            Ok(n @ 1..=24) => Some(0x6F + n),
            _ => None,
        },
        _ => None,
    }
}

fn bad_key(chord: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unknown key {chord:?}"),
    )
}

fn tap(inputs: &mut Vec<INPUT>, vk: u16) {
    inputs.push(key(vk, false));
    inputs.push(key(vk, true));
}

fn key(vk: u16, up: bool) -> INPUT {
    keyboard(
        vk,
        0,
        if up {
            KEYEVENTF_KEYUP
        } else {
            KEYBD_EVENT_FLAGS(0)
        },
    )
}

fn unicode(unit: u16, up: bool) -> INPUT {
    let flags = if up {
        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
    } else {
        KEYEVENTF_UNICODE
    };
    keyboard(0, unit, flags)
}

fn keyboard(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> io::Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
