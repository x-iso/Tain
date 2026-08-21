//! Window icon, rasterised from the Tain mark.
//!
//! `assets/logo.svg` is three flat shapes in a single colour, so filling them
//! directly is simpler and more predictable than pulling an SVG rasteriser into
//! the build for one asset. Keep this in sync if the mark changes.
//!
//! Asset geometry (viewBox `0 0 20 20`, fill `#599ADE`):
//! - `rect(0, 0, 10, 20)`            — the tall bar
//! - `rect(13, 7, 7, 13)`            — the short bar
//! - `M14 0.5 L20 0 V5 L14 4.5 Z`    — the flag, whose top and bottom edges slope

/// Brand blue, straight from the asset.
const BLUE: [u8; 3] = [0x59, 0x9A, 0xDE];

/// The asset's coordinate space is 20×20.
const VIEWBOX: f32 = 20.0;

/// Samples per axis, per pixel. The flag's sloped edges alias badly without this.
const SUPERSAMPLE: u32 = 4;

/// Is this point (in viewBox coordinates) inside the mark?
fn inside(x: f32, y: f32) -> bool {
    // Tall bar.
    if (0.0..10.0).contains(&x) && (0.0..VIEWBOX).contains(&y) {
        return true;
    }
    // Short bar.
    if (13.0..VIEWBOX).contains(&x) && (7.0..VIEWBOX).contains(&y) {
        return true;
    }
    // Flag: spans x 14..20, with edges sloping from y 0.5→0 (top) and 4.5→5 (bottom).
    if (14.0..=VIEWBOX).contains(&x) {
        let t = (x - 14.0) / 6.0;
        let top = 0.5 - 0.5 * t;
        let bottom = 4.5 + 0.5 * t;
        if y >= top && y <= bottom {
            return true;
        }
    }
    false
}

/// Rasterise the mark to premultiplied-free RGBA at `size`×`size`.
pub fn window_icon(size: u32) -> egui::IconData {
    let size = size.max(1);
    let mut rgba = vec![0u8; (size as usize) * (size as usize) * 4];
    let scale = VIEWBOX / size as f32;
    let step = 1.0 / SUPERSAMPLE as f32;
    let samples = (SUPERSAMPLE * SUPERSAMPLE) as f32;

    for py in 0..size {
        for px in 0..size {
            let mut hits = 0u32;
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let x = (px as f32 + (sx as f32 + 0.5) * step) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) * step) * scale;
                    if inside(x, y) {
                        hits += 1;
                    }
                }
            }

            if hits > 0 {
                let i = ((py * size + px) * 4) as usize;
                rgba[i] = BLUE[0];
                rgba[i + 1] = BLUE[1];
                rgba[i + 2] = BLUE[2];
                rgba[i + 3] = (255.0 * hits as f32 / samples).round() as u8;
            }
        }
    }

    egui::IconData { rgba, width: size, height: size }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(icon: &egui::IconData, x: u32, y: u32) -> u8 {
        icon.rgba[((y * icon.width + x) * 4 + 3) as usize]
    }

    #[test]
    fn icon_has_the_requested_dimensions() {
        let icon = window_icon(64);
        assert_eq!((icon.width, icon.height), (64, 64));
        assert_eq!(icon.rgba.len(), 64 * 64 * 4);
    }

    #[test]
    fn tall_bar_is_opaque_and_the_gap_beside_it_is_clear() {
        let icon = window_icon(40); // 2px per viewBox unit
        // x=4 (viewBox 2), y=20 (viewBox 10) is inside the tall bar.
        assert_eq!(alpha_at(&icon, 4, 20), 255);
        // viewBox x≈11, y≈10 falls in the gap between the bars.
        assert_eq!(alpha_at(&icon, 22, 20), 0);
    }

    #[test]
    fn short_bar_and_flag_are_filled() {
        let icon = window_icon(40);
        // Short bar: viewBox (16, 14).
        assert_eq!(alpha_at(&icon, 32, 28), 255);
        // Flag body: viewBox (17, 2.5).
        assert_eq!(alpha_at(&icon, 34, 5), 255);
        // Below the flag, above the short bar: viewBox (17, 6) is empty.
        assert_eq!(alpha_at(&icon, 34, 12), 0);
    }

    #[test]
    fn filled_pixels_all_carry_the_brand_colour() {
        let icon = window_icon(32);
        for px in icon.rgba.chunks_exact(4) {
            if px[3] > 0 {
                assert_eq!([px[0], px[1], px[2]], BLUE);
            }
        }
    }

    #[test]
    fn sloped_edges_produce_partial_coverage() {
        // Without antialiasing every pixel would be 0 or 255; the flag's angled
        // edges should yield at least one intermediate alpha.
        let icon = window_icon(64);
        assert!(
            icon.rgba.chunks_exact(4).any(|px| (1..255).contains(&px[3])),
            "expected antialiased edge pixels"
        );
    }

    #[test]
    fn zero_size_does_not_panic() {
        let icon = window_icon(0);
        assert_eq!(icon.width, 1);
    }
}
