//! Tab icons from SVG files, drawn with resvg to fit a square.

use std::path::Path;

use jet_config::Icon;
use resvg::{tiny_skia, usvg};

/// Premultiplied RGBA, as the color glyph atlas holds.
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// An image icon, scaled to fit a `px` square; text icons have none.
pub fn bitmap(icon: &Icon, px: u32) -> Result<Bitmap, String> {
    match icon {
        Icon::File(path) => load(path, px),
        Icon::Svg(svg) => render(svg.as_bytes(), px).map_err(|e| format!("inline icon: {e}")),
        Icon::Text(_) => Err("a text icon isn't an image".into()),
    }
}

/// The SVG at `path`, scaled to fit a `px` square.
fn load(path: &Path, px: u32) -> Result<Bitmap, String> {
    let data = std::fs::read(path).map_err(|e| format!("icon {}: {e}", path.display()))?;
    render(&data, px).map_err(|e| format!("icon {}: {e}", path.display()))
}

fn render(svg: &[u8], px: u32) -> Result<Bitmap, String> {
    // Browsers forgive a missing namespace and so do we: icons written
    // into a config often leave it out.
    let fixed;
    let svg = match std::str::from_utf8(svg) {
        Ok(text) if !text.contains("xmlns") => {
            fixed = text.replacen("<svg", r#"<svg xmlns="http://www.w3.org/2000/svg""#, 1);
            fixed.as_bytes()
        }
        _ => svg,
    };
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).map_err(|e| e.to_string())?;
    let size = tree.size();
    let scale = px as f32 / size.width().max(size.height());
    let width = ((size.width() * scale).round() as u32).max(1);
    let height = ((size.height() * scale).round() as u32).max(1);
    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("empty image")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(Bitmap {
        width,
        height,
        rgba: pixmap.take(),
    })
}

#[cfg(test)]
mod tests {
    use jet_config::Icon;

    use crate::icon::{bitmap, render};

    #[test]
    fn the_jetmux_icon_draws() -> Result<(), String> {
        let icon = render(crate::app::JETMUX_ICON.as_bytes(), 16)?;
        assert_eq!((icon.width, icon.height), (16, 16));
        assert!(icon.rgba.chunks(4).any(|p| p[3] > 0));
        Ok(())
    }

    #[test]
    fn svgs_fit_the_square() -> Result<(), String> {
        let wide = br##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20">
            <rect width="40" height="20" fill="#ff0000"/></svg>"##;
        let icon = render(wide, 16)?;
        assert_eq!((icon.width, icon.height), (16, 8));
        // Opaque red, premultiplied.
        assert_eq!(icon.rgba.get(..4), Some(&[255, 0, 0, 255][..]));
        assert!(render(b"not svg", 16).is_err());
        let bare = r#"<svg viewBox="0 0 2 2"><rect width="2" height="2"/></svg>"#;
        let bare = bitmap(&Icon::Svg(bare.into()), 4)?;
        assert_eq!((bare.width, bare.height), (4, 4));
        let missing = bitmap(&Icon::File("missing.svg".into()), 16).err();
        assert!(missing.is_some_and(|e| e.contains("missing.svg")));
        Ok(())
    }
}
