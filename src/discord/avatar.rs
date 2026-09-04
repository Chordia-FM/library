//! The bot's avatar: the Chordia mark in the bot's colour, unless the owner brought their own.
//!
//! The geometry is the one in `frontend/src/components/brand/mark.ts` (the record's rings, the C
//! that is the C of "Chordia", the label ring), drawn exactly as `scripts/generate-brand-assets.ts`
//! draws `logo512.png`: the app's dark backdrop edge to edge, the record in its light ink, the C
//! and the label in the accent. Rendered at 512 px. Every
//! accent change re-renders and re-uploads it, through the same rate-limit-aware path as the emoji
//! set. Once the owner uploads their own image the mark is left alone: `avatar_managed` turns off
//! and nothing here touches the avatar again until they turn it back on.

use base64::Engine;
use serde_json::json;

use crate::discord::emoji::normalize_hex;
use crate::discord::rest::{Rest, RestError};

const SIZE: u32 = 512;
/// The app canvas (`--background`) and its light ink, as the brand generator has them.
const BG: &str = "#0b0910";
const INK: &str = "#f0eff5";

/// The mark as SVG in a 64-unit box, matching `mark.ts` number for number.
pub fn mark_svg(accent: &str) -> String {
    // openArc(15, 48): from +48° to −48° the long way round, i.e. a ring open to the east.
    let (ax, ay) = polar(15.0, 48.0);
    let (bx, by) = polar(15.0, -48.0);
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <rect width="64" height="64" fill="{BG}"/>
  <circle cx="32" cy="32" r="29" fill="none" stroke="{INK}" stroke-width="3"/>
  <circle cx="32" cy="32" r="24" fill="none" stroke="{INK}" stroke-width="1.2" opacity="0.45"/>
  <circle cx="32" cy="32" r="20.5" fill="none" stroke="{INK}" stroke-width="1.2" opacity="0.45"/>
  <path d="M {ax:.2} {ay:.2} A 15 15 0 1 1 {bx:.2} {by:.2}" fill="none" stroke="{accent}" stroke-width="5" stroke-linecap="round"/>
  <circle cx="32" cy="32" r="3.5" fill="none" stroke="{accent}" stroke-width="2.5"/>
</svg>"##
    )
}

fn polar(r: f64, deg: f64) -> (f64, f64) {
    let rad = deg.to_radians();
    (32.0 + r * rad.cos(), 32.0 + r * rad.sin())
}

/// Render the mark in `hex` as a 512 px PNG.
pub fn render_png(hex: &str) -> anyhow::Result<Vec<u8>> {
    let hex = normalize_hex(hex).ok_or_else(|| anyhow::anyhow!("bad colour {hex:?}"))?;
    let tree = resvg::usvg::Tree::from_str(&mark_svg(&hex), &resvg::usvg::Options::default())?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(SIZE, SIZE)
        .ok_or_else(|| anyhow::anyhow!("pixmap allocation failed"))?;
    let scale = SIZE as f32 / 64.0;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap.encode_png()?)
}

/// `PATCH /users/@me` with an image. `mime` is `image/png` for the mark, whatever the owner
/// uploaded otherwise (PNG, JPEG, GIF, WebP).
pub async fn upload(rest: &Rest, mime: &str, bytes: &[u8]) -> Result<(), RestError> {
    let image = format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    );
    rest.patch::<serde_json::Value>("/users/@me", &json!({ "avatar": image }))
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_matches_the_web_geometry() {
        let svg = mark_svg("#f2258c");
        // The same arc the favicon generator emits for openArc(15, 48).
        assert!(
            svg.contains("M 42.04 43.15 A 15 15 0 1 1 42.04 20.85"),
            "{svg}"
        );
        assert!(svg.contains(r#"r="29""#) && svg.contains(r#"r="3.5""#));
    }

    #[test]
    fn renders_a_png_with_accent_on_the_c() {
        let png = render_png("#ff0000").unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert!(png.len() < 200 * 1024, "{} bytes", png.len());
    }
}
