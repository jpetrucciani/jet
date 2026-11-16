//! The window frame Windows draws (caption, border, corners, backdrop),
//! styled through DWM attributes. Each applies on its own, so an older
//! Windows that lacks one keeps the rest.

use std::ffi::c_void;

use jet_config::{Backdrop, ColorScheme, Corners, FrameColor};
use jet_core::Rgb;
use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DWM_SYSTEMBACKDROP_TYPE, DWM_WINDOW_CORNER_PREFERENCE, DWMSBT_MAINWINDOW, DWMSBT_NONE,
    DWMSBT_TABBEDWINDOW, DWMSBT_TRANSIENTWINDOW, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR,
    DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DEFAULT, DWMWCP_DONOTROUND, DWMWCP_ROUND,
    DWMWCP_ROUNDSMALL, DWMWINDOWATTRIBUTE, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
};
use windows::Win32::UI::Controls::MARGINS;

/// `DWMWA_COLOR_DEFAULT` and `DWMWA_COLOR_NONE`.
const COLOR_DEFAULT: COLORREF = COLORREF(0xFFFF_FFFF);
const COLOR_NONE: COLORREF = COLORREF(0xFFFF_FFFE);

/// Style `hwnd`'s frame per `window`, with the terminal's colors for
/// `background` choices and the `auto` scheme.
pub fn apply(hwnd: HWND, window: &jet_config::Window, background: Rgb, foreground: Rgb) {
    let dark = match window.color_scheme {
        ColorScheme::Dark => true,
        ColorScheme::Light => false,
        ColorScheme::Auto => luminance(background) < 0.5,
    };
    set(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &i32::from(dark));

    let caption = match window.titlebar_color {
        FrameColor::Background => Some(background),
        FrameColor::Color(rgb) => Some(rgb),
        FrameColor::System | FrameColor::None => None,
    };
    set(
        hwnd,
        DWMWA_CAPTION_COLOR,
        &caption.map_or(COLOR_DEFAULT, colorref),
    );
    let text = caption.map(|c| match window.titlebar_color {
        FrameColor::Background => foreground,
        _ if luminance(c) < 0.5 => Rgb {
            r: 255,
            g: 255,
            b: 255,
        },
        _ => Rgb { r: 0, g: 0, b: 0 },
    });
    set(
        hwnd,
        DWMWA_TEXT_COLOR,
        &text.map_or(COLOR_DEFAULT, colorref),
    );

    let border = match window.border_color {
        FrameColor::System => COLOR_DEFAULT,
        FrameColor::None => COLOR_NONE,
        FrameColor::Background => colorref(background),
        FrameColor::Color(rgb) => colorref(rgb),
    };
    set(hwnd, DWMWA_BORDER_COLOR, &border);

    let corners: DWM_WINDOW_CORNER_PREFERENCE = match window.corners {
        Corners::Default => DWMWCP_DEFAULT,
        Corners::Round => DWMWCP_ROUND,
        Corners::Small => DWMWCP_ROUNDSMALL,
        Corners::Square => DWMWCP_DONOTROUND,
    };
    set(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corners);

    let backdrop: DWM_SYSTEMBACKDROP_TYPE = match window.backdrop {
        Backdrop::None => DWMSBT_NONE,
        Backdrop::Mica => DWMSBT_MAINWINDOW,
        Backdrop::Acrylic => DWMSBT_TRANSIENTWINDOW,
        Backdrop::Tabbed => DWMSBT_TABBEDWINDOW,
    };
    set(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, &backdrop);
    // The backdrop fills the frame; extending the frame over the whole
    // client area puts it behind the terminal too.
    let all = if window.backdrop == Backdrop::None {
        0
    } else {
        -1
    };
    let margins = MARGINS {
        cxLeftWidth: all,
        cxRightWidth: all,
        cyTopHeight: all,
        cyBottomHeight: all,
    };
    if let Err(err) = unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) } {
        eprintln!("jet: extending the frame for the backdrop: {err}");
    }
}

fn set<T>(hwnd: HWND, attribute: DWMWINDOWATTRIBUTE, value: &T) {
    let size = u32::try_from(size_of::<T>()).unwrap_or(0);
    let ptr = std::ptr::from_ref(value).cast::<c_void>();
    // Older Windows lacks some attributes; the rest still apply.
    let _ = unsafe { DwmSetWindowAttribute(hwnd, attribute, ptr, size) };
}

fn colorref(c: Rgb) -> COLORREF {
    COLORREF(u32::from(c.r) | u32::from(c.g) << 8 | u32::from(c.b) << 16)
}

/// Rough perceived brightness, 0 to 1.
fn luminance(c: Rgb) -> f32 {
    (0.299 * f32::from(c.r) + 0.587 * f32::from(c.g) + 0.114 * f32::from(c.b)) / 255.0
}
