//! A tiny picture of a host, kept so its card is recognisable at a glance.
//!
//! ## Small, and blurred before it is stored
//!
//! 64 by 36 pixels with a Gaussian pass applied at capture time, drawn scaled
//! up to fill the card.
//!
//! The blur is done **once, here, before anything is written to disk** rather
//! than at draw time. Two reasons, and the second is the important one:
//!
//! - It looks right. Upscaling a tiny image with linear filtering gives the
//!   smeared, faintly diamond-shaped artefacts of stretched pixels, not the
//!   soft falloff of an actual blur. Blurring at source and then magnifying a
//!   smooth image reads the way it is supposed to.
//! - The detail is gone rather than hidden. A thumbnail of someone's desktop is
//!   a screenshot of their machine living on *another* person's disk, in a file
//!   nobody thinks about again. The host agreed to be watched for a while; they
//!   did not agree to leave a picture behind. Blurring at render would leave the
//!   sharp original sitting in the file for anyone who opened it.
//!
//! What survives is the wallpaper's colour and gross shape — enough to tell one
//! machine from another in a list, and no more.
//!
//! It is also why this is not stored as a compressed image: a JPEG invites
//! someone to raise the resolution later "since the encoder is already there".
//! A fixed-size array of blurred pixels has nowhere to grow.

pub use tain_core::preview::{BYTES, H, W};

/// Encode a preview for the connections file.
///
/// Hex rather than base64 purely to avoid a dependency for 1.7 KB of pixels;
/// the file is already JSON and already human-readable, and a preview that can
/// be deleted with a text editor is a feature rather than a cost.
pub fn encode(rgb: &[u8]) -> String {
    rgb.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode a preview, rejecting anything that is not exactly the right size.
///
/// Strict on purpose: this comes from a file a user can edit, and a short
/// buffer reaching `ColorImage` is a panic rather than a wrong picture.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    if text.len() != BYTES * 2 {
        return None;
    }
    let mut out = Vec::with_capacity(BYTES);
    for pair in text.as_bytes().chunks_exact(2) {
        let hex = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(hex, 16).ok()?);
    }
    Some(out)
}

/// Turn a decoded preview into an image egui can upload.
pub fn to_color_image(rgb: &[u8]) -> Option<egui::ColorImage> {
    if rgb.len() != BYTES {
        return None;
    }
    let pixels = rgb
        .chunks_exact(3)
        .map(|p| egui::Color32::from_rgb(p[0], p[1], p[2]))
        .collect();
    Some(egui::ColorImage { size: [W, H], pixels })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The pixel maths and the blur are tested in `tain_core::preview`; what
    // is left here is storage and getting it onto the screen.

    #[test]
    fn encoding_round_trips() {
        let rgb: Vec<u8> = (0..BYTES).map(|i| (i % 251) as u8).collect();
        let decoded = decode(&encode(&rgb)).expect("round trip");
        assert_eq!(decoded, rgb);
    }

    /// The connections file is user-editable, so decoding is the boundary where
    /// nonsense has to stop.
    #[test]
    fn a_malformed_preview_is_rejected_rather_than_panicking() {
        assert!(decode("").is_none());
        assert!(decode("abcd").is_none(), "right alphabet, wrong length");
        assert!(decode(&"zz".repeat(BYTES)).is_none(), "right length, not hex");
        assert!(to_color_image(&[0; 4]).is_none());
    }

    #[test]
    fn a_decoded_preview_becomes_an_image_of_the_expected_size() {
        let image = to_color_image(&vec![7; BYTES]).expect("an image");
        assert_eq!(image.size, [W, H]);
        assert_eq!(image.pixels.len(), W * H);
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(7, 7, 7));
    }
}
