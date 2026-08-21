//! The host's mouse pointer, sent as data rather than painted into the frame.
//!
//! ## Why not just capture it
//!
//! Desktop Duplication deliberately leaves the pointer out of the image, and
//! that turns out to be the right shape for a remote desktop anyway. Compositing
//! it into the frame before encoding would mean:
//!
//! - the cursor moves at the *stream's* frame rate, so it lags and stutters
//!   exactly when the link is worst — and the pointer is the one thing a user
//!   watches continuously;
//! - every pointer movement dirties the frame and costs a re-encode, on a
//!   desktop that is otherwise static;
//! - a hardware cursor gets blurred by lossy compression, which reads as the
//!   whole session being soft.
//!
//! Sending position and shape separately lets the guest draw it sharp, at its
//! own frame rate, over whatever video frame it happens to be showing.
//!
//! ## Shapes are sent once
//!
//! A pointer shape changes when it crosses a text field or a resize handle, not
//! per frame. [`CursorShape`] carries an id so the guest can cache: the host
//! sends the image the first time it sees it and a bare position afterwards.

use serde::{Deserialize, Serialize};

/// Where the pointer is, and whether it is visible at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorPosition {
    /// Position from the top-left, in **desktop** pixels.
    pub x: i32,
    pub y: i32,
    /// The desktop these coordinates are relative to.
    ///
    /// # Why this is not the frame size
    ///
    /// Desktop Duplication reports the pointer in the host's native resolution,
    /// while the video may be scaled down — by the quality controller, or by the
    /// guest's own window being smaller than the host's screen. Scaling the
    /// position against the frame instead put the pointer progressively further
    /// from where it actually was, and drew it too large, because both errors
    /// come from the same wrong divisor.
    pub desktop_width: u16,
    pub desktop_height: u16,
    /// Whether the pointer is being drawn on the host at all. Full-screen video
    /// and games hide it, and a remote pointer that stayed visible over a hidden
    /// one would be a ghost the user cannot get rid of.
    pub visible: bool,
}

/// A pointer image, sent once per distinct shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShape {
    /// Identifies this image so the guest can cache and re-use it.
    pub id: u64,
    pub width: u16,
    pub height: u16,
    /// Where the pointer's "point" sits inside the image.
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    /// RGBA8, `width * height * 4` bytes, already un-masked.
    pub rgba: Vec<u8>,
}

/// Pointer state for one frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorUpdate {
    pub position: CursorPosition,
    /// The image, when the guest has not seen this one before.
    pub shape: Option<CursorShape>,
}

/// The three shapes Desktop Duplication reports, as sent by the driver.
///
/// Only [`Self::Color`] is a plain image. The other two encode transparency and
/// inversion in a way that has to be undone before anything can draw them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    /// 1 bit per pixel: an AND mask stacked above an XOR mask, double height.
    Monochrome,
    /// 32-bit BGRA with a real alpha channel.
    Color,
    /// 32-bit BGRA whose alpha is unused; the mask lives in the top bit instead.
    MaskedColor,
}

impl ShapeKind {
    /// From the `Type` field of `DXGI_OUTDUPL_POINTER_SHAPE_INFO`.
    pub fn from_raw(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Monochrome),
            2 => Some(Self::Color),
            4 => Some(Self::MaskedColor),
            _ => None,
        }
    }
}

/// Turn a driver pointer shape into straight RGBA.
///
/// Returns the image and its true height, which differs from the reported one
/// for monochrome shapes — those stack two masks vertically and are described
/// as twice as tall as they draw.
///
/// # The masks are not decoration
///
/// A monochrome pointer is two 1-bit planes. AND is applied first, then XOR:
/// `AND=1, XOR=0` is transparent, `AND=1, XOR=1` inverts what is underneath.
///
/// Inversion cannot be expressed in RGBA against unknown content, so it is
/// approximated as opaque **black**. The shapes that use it are the I-beam and
/// the resize handles, which live over text fields and document backgrounds —
/// light, overwhelmingly. Rendering it white, as the first version did, made
/// the I-beam invisible in precisely the place it matters.
pub fn to_rgba(
    kind: ShapeKind,
    data: &[u8],
    width: u32,
    height: u32,
    pitch: u32,
) -> Option<(Vec<u8>, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    match kind {
        ShapeKind::Monochrome => {
            // Two stacked masks, so the drawn pointer is half the reported size.
            let real_height = height / 2;
            if real_height == 0 {
                return None;
            }
            let mut rgba = vec![0u8; (width * real_height * 4) as usize];
            for y in 0..real_height {
                for x in 0..width {
                    let byte = (x / 8) as usize;
                    let bit = 7 - (x % 8);
                    let and_row = (y * pitch) as usize;
                    let xor_row = ((y + real_height) * pitch) as usize;
                    let and = data.get(and_row + byte)? >> bit & 1;
                    let xor = data.get(xor_row + byte)? >> bit & 1;

                    let out = ((y * width + x) * 4) as usize;
                    let px = rgba.get_mut(out..out + 4)?;
                    match (and, xor) {
                        // Opaque black, then opaque white.
                        (0, 0) => px.copy_from_slice(&[0, 0, 0, 255]),
                        (0, 1) => px.copy_from_slice(&[255, 255, 255, 255]),
                        // Transparent.
                        (1, 0) => px.copy_from_slice(&[0, 0, 0, 0]),
                        // Invert, which cannot be expressed against unknown
                        // content. Black rather than white: the shapes that
                        // rely on inversion are the I-beam and the resize
                        // handles, and those are used over text fields and
                        // document backgrounds, which are overwhelmingly light.
                        // White made the I-beam invisible exactly where it is
                        // needed most.
                        _ => px.copy_from_slice(&[0, 0, 0, 255]),
                    }
                }
            }
            Some((rgba, real_height))
        }
        ShapeKind::Color => {
            let mut rgba = vec![0u8; (width * height * 4) as usize];
            for y in 0..height {
                for x in 0..width {
                    let src = (y * pitch + x * 4) as usize;
                    let bgra = data.get(src..src + 4)?;
                    let out = ((y * width + x) * 4) as usize;
                    let px = rgba.get_mut(out..out + 4)?;
                    // BGRA to RGBA.
                    px.copy_from_slice(&[bgra[2], bgra[1], bgra[0], bgra[3]]);
                }
            }
            Some((rgba, height))
        }
        ShapeKind::MaskedColor => {
            let mut rgba = vec![0u8; (width * height * 4) as usize];
            for y in 0..height {
                for x in 0..width {
                    let src = (y * pitch + x * 4) as usize;
                    let bgra = data.get(src..src + 4)?;
                    let out = ((y * width + x) * 4) as usize;
                    let px = rgba.get_mut(out..out + 4)?;
                    // The alpha byte is a mask, not opacity: 0 means draw the
                    // colour, anything else means XOR the colour with what is
                    // already on screen.
                    //
                    // # XOR with black is how this format says "transparent"
                    //
                    // Rendering every masked pixel as white — which the first
                    // version did — turns the entire background of an I-beam
                    // into a solid white square, because that background is
                    // masked pixels of colour zero. XORing zero changes
                    // nothing, so those pixels are transparent.
                    if bgra[3] == 0 {
                        px.copy_from_slice(&[bgra[2], bgra[1], bgra[0], 255]);
                    } else if bgra[0] == 0 && bgra[1] == 0 && bgra[2] == 0 {
                        px.copy_from_slice(&[0, 0, 0, 0]);
                    } else {
                        // A real inversion. Not expressible against unknown
                        // content, so the colour is drawn as-is — which is what
                        // it would come to over a dark background.
                        px.copy_from_slice(&[bgra[2], bgra[1], bgra[0], 255]);
                    }
                }
            }
            Some((rgba, height))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_type_the_driver_can_report_is_recognised() {
        assert_eq!(ShapeKind::from_raw(1), Some(ShapeKind::Monochrome));
        assert_eq!(ShapeKind::from_raw(2), Some(ShapeKind::Color));
        assert_eq!(ShapeKind::from_raw(4), Some(ShapeKind::MaskedColor));
        // Unknown types are refused rather than guessed at: drawing a shape
        // with the wrong interpretation produces a block of noise.
        assert_eq!(ShapeKind::from_raw(0), None);
        assert_eq!(ShapeKind::from_raw(3), None);
    }

    /// A monochrome shape is half as tall as it says, and its masks mean
    /// specific things.
    #[test]
    fn a_monochrome_pointer_unpacks_to_the_four_states_its_masks_encode() {
        // 8x2 reported: one row of AND, one row of XOR, so 8x1 drawn.
        // AND row: 1100 0000  -> first two transparent-or-invert
        // XOR row: 1010 0000
        //   x0: and=1 xor=1 -> invert  -> white
        //   x1: and=1 xor=0 -> transparent
        //   x2: and=0 xor=1 -> white
        //   x3: and=0 xor=0 -> black
        let data = [0b1100_0000u8, 0b1010_0000];
        let (rgba, height) = to_rgba(ShapeKind::Monochrome, &data, 8, 2, 1).expect("unpack");
        assert_eq!(height, 1, "a monochrome shape draws at half its reported height");
        assert_eq!(rgba.len(), 8 * 4, "eight pixels of RGBA");

        assert_eq!(&rgba[0..4], &[0, 0, 0, 255], "invert approximates to black");
        assert_eq!(&rgba[4..8], &[0, 0, 0, 0], "AND without XOR is transparent");
        assert_eq!(&rgba[8..12], &[255, 255, 255, 255], "XOR alone is white");
        assert_eq!(&rgba[12..16], &[0, 0, 0, 255], "neither is opaque black");
    }

    #[test]
    fn a_colour_pointer_is_reordered_and_keeps_its_alpha() {
        // One pixel, BGRA, half transparent.
        let data = [10u8, 20, 30, 128];
        let (rgba, height) = to_rgba(ShapeKind::Color, &data, 1, 1, 4).expect("unpack");
        assert_eq!(height, 1);
        assert_eq!(rgba, vec![30, 20, 10, 128], "BGRA must become RGBA");
    }

    /// The masked-colour alpha byte is a mask, not opacity.
    ///
    /// Reading it as opacity makes every pointer of this type invisible, which
    /// is a blank screen rather than an error.
    #[test]
    fn a_masked_colour_pointer_does_not_read_its_mask_as_transparency() {
        // Mask 0 means draw the colour as-is.
        let data = [10u8, 20, 30, 0, 40, 50, 60, 0xff];
        let (rgba, _) = to_rgba(ShapeKind::MaskedColor, &data, 2, 1, 8).expect("unpack");
        assert_eq!(&rgba[0..4], &[30, 20, 10, 255], "mask 0 draws opaque colour");
        assert_eq!(&rgba[4..8], &[60, 50, 40, 255], "a masked colour is drawn, not whitened");
    }

    /// The bug behind "the pointer is a white square over text fields".
    ///
    /// A masked pixel of colour zero XORs nothing into the screen, which is how
    /// this format spells transparent. Rendering those white filled the whole
    /// bounding box of an I-beam with solid white.
    #[test]
    fn a_masked_black_pixel_is_transparent_rather_than_a_white_block() {
        // A 2x2 shape: three masked-black pixels around one drawn pixel, which
        // is the shape of every cursor that uses this type.
        let data = [
            0u8, 0, 0, 0xff, 0, 0, 0, 0xff, // masked black — transparent
            0, 0, 0, 0xff, 200, 210, 220, 0, // masked black, then a real colour
        ];
        let (rgba, _) = to_rgba(ShapeKind::MaskedColor, &data, 2, 2, 8).expect("unpack");

        let transparent = rgba.chunks_exact(4).filter(|px| px[3] == 0).count();
        assert_eq!(transparent, 3, "masked black must not become opaque");
        assert_eq!(&rgba[12..16], &[220, 210, 200, 255], "the drawn pixel survives");
        assert!(
            !rgba.chunks_exact(4).all(|px| px == [255, 255, 255, 255]),
            "the whole shape came out white — this is the white-square bug"
        );
    }

    #[test]
    fn a_truncated_shape_is_refused_rather_than_read_past_the_end() {
        // Claims 8x8 but carries two bytes.
        assert!(to_rgba(ShapeKind::Color, &[0, 0], 8, 8, 32).is_none());
        assert!(to_rgba(ShapeKind::Monochrome, &[0], 8, 8, 1).is_none());
    }

    #[test]
    fn a_zero_sized_shape_is_refused() {
        assert!(to_rgba(ShapeKind::Color, &[0; 16], 0, 4, 16).is_none());
        assert!(to_rgba(ShapeKind::Color, &[0; 16], 4, 0, 16).is_none());
        // A monochrome shape one row tall has no XOR plane at all.
        assert!(to_rgba(ShapeKind::Monochrome, &[0; 4], 8, 1, 1).is_none());
    }

    /// Row padding must be honoured or the image shears.
    #[test]
    fn a_padded_pitch_is_respected() {
        // 1 pixel wide, 2 rows, but each row is 8 bytes on disk.
        let data = [10u8, 20, 30, 255, 0, 0, 0, 0, 40, 50, 60, 255, 0, 0, 0, 0];
        let (rgba, _) = to_rgba(ShapeKind::Color, &data, 1, 2, 8).expect("unpack");
        assert_eq!(&rgba[0..4], &[30, 20, 10, 255]);
        assert_eq!(&rgba[4..8], &[60, 50, 40, 255], "second row must skip the padding");
    }
}
