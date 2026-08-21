//! A small blurred picture of a screen, for identifying a machine later.
//!
//! ## Small, and blurred before it leaves the host
//!
//! 64 by 36 pixels with a Gaussian pass applied here, at capture time, drawn
//! scaled up wherever it is shown.
//!
//! The blur happens **once, on the machine being pictured, before anything is
//! sent or stored**. Two reasons, and the second is the important one:
//!
//! - It looks right. Upscaling a tiny image with linear filtering gives the
//!   smeared, faintly diamond-shaped artefacts of stretched pixels, not the
//!   soft falloff of an actual blur. Blurring at source and then magnifying a
//!   smooth image reads the way it is supposed to.
//! - The detail is gone rather than hidden. This ends up as a screenshot of
//!   someone's machine living on *another* person's disk, in a file nobody
//!   thinks about again. The host agreed to be watched for a while; they did
//!   not agree to leave a picture behind. Blurring at the far end, or at draw
//!   time, would put the sharp original on the wire and in the file.
//!
//! What survives is the wallpaper's colour and gross shape — enough to tell one
//! machine from another in a list, and no more.
//!
//! It is deliberately not a compressed image: a JPEG invites someone to raise
//! the resolution later "since the encoder is already there". A fixed-size
//! array of blurred pixels has nowhere to grow.

use super::screen_capture::PixelFormat;

/// Preview width in pixels.
pub const W: usize = 64;
/// Preview height in pixels. 16:9, so a widescreen desktop is not distorted.
pub const H: usize = 36;
/// Bytes in a preview: RGB, no alpha — a desktop is opaque.
pub const BYTES: usize = W * H * 3;

/// Gaussian radius, in preview pixels.
///
/// Three is a little over one and a half sigma each side, which is where the
/// tail stops contributing anything visible at this size. Larger turns every
/// machine into the same grey rectangle, which defeats the point of having a
/// picture at all.
const BLUR_RADIUS: isize = 3;
/// Standard deviation matching [`BLUR_RADIUS`].
const BLUR_SIGMA: f32 = 1.7;

/// Build a preview from a captured frame in whichever format it arrived in.
///
/// Both formats are handled here because the host's capture path chooses
/// between them for reasons that have nothing to do with previews — it converts
/// to NV12 on the GPU when it can, and hands over BGRA when it cannot.
pub fn from_frame(data: &[u8], width: usize, height: usize, format: PixelFormat) -> Option<Vec<u8>> {
    let raw = match format {
        PixelFormat::Bgra => sample(data, width, height, bgra_pixel)?,
        PixelFormat::Nv12 => sample(data, width, height, nv12_pixel)?,
    };
    Some(blur(&raw))
}

/// The same, from an already-decoded RGBA frame.
///
/// The guest's fallback path: what a decoder hands back is RGBA, not the BGRA
/// the capture side produces. Only used when the host is too old to send its
/// own picture.
pub fn from_rgba(data: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
    Some(blur(&sample(data, width, height, rgba_pixel)?))
}

/// One RGBA pixel, which is already in the order wanted.
fn rgba_pixel(data: &[u8], width: usize, _height: usize, x: usize, y: usize) -> Option<[u8; 3]> {
    let i = (y * width + x) * 4;
    Some([*data.get(i)?, *data.get(i + 1)?, *data.get(i + 2)?])
}

/// One BGRA pixel as RGB, or `None` if it lies outside the buffer.
fn bgra_pixel(data: &[u8], width: usize, _height: usize, x: usize, y: usize) -> Option<[u8; 3]> {
    let i = (y * width + x) * 4;
    Some([*data.get(i + 2)?, *data.get(i + 1)?, *data.get(i)?])
}

/// One NV12 pixel as RGB.
///
/// BT.601 limited range, matching what the capture side encodes with — the
/// coefficients are the inverse of the ones in `codec_h264`. A mismatch here
/// would tint every preview slightly, which is the kind of wrongness nobody
/// traces because the picture is blurred anyway.
fn nv12_pixel(data: &[u8], width: usize, height: usize, x: usize, y: usize) -> Option<[u8; 3]> {
    let luma = *data.get(y * width + x)? as i32;
    let chroma = width * height + (y / 2) * width + (x / 2) * 2;
    let cb = *data.get(chroma)? as i32;
    let cr = *data.get(chroma + 1)? as i32;

    let c = luma - 16;
    let d = cb - 128;
    let e = cr - 128;
    let clamp = |v: i32| v.clamp(0, 255) as u8;
    Some([
        clamp((298 * c + 409 * e + 128) >> 8),
        clamp((298 * c - 100 * d - 208 * e + 128) >> 8),
        clamp((298 * c + 516 * d + 128) >> 8),
    ])
}

/// Reads one pixel of a frame as RGB, or `None` if it lies outside the buffer.
///
/// One per source format, so [`sample`] does the boxing and averaging once
/// rather than three times over.
type ReadPixel = fn(&[u8], usize, usize, usize, usize) -> Option<[u8; 3]>;

/// Average a frame down to preview size, one box per output pixel.
fn sample(data: &[u8], width: usize, height: usize, pixel: ReadPixel) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || data.is_empty() {
        return None;
    }

    // Averaging every pixel of a 4K frame is several million reads. A grid
    // within each box is visually identical at this size for a fraction of the
    // cost, and this runs on a thread that has a video stream to keep up with.
    let step_x = (width / W / 4).max(1);
    let step_y = (height / H / 4).max(1);

    let mut out = vec![0u8; BYTES];
    for oy in 0..H {
        let y0 = oy * height / H;
        let y1 = (((oy + 1) * height / H).max(y0 + 1)).min(height);
        for ox in 0..W {
            let x0 = ox * width / W;
            let x1 = (((ox + 1) * width / W).max(x0 + 1)).min(width);

            let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
            let mut y = y0;
            while y < y1 {
                let mut x = x0;
                while x < x1 {
                    // A short buffer aborts the whole preview rather than
                    // producing one with a black corner: a frame that does not
                    // match its own dimensions is not a picture worth keeping.
                    let [pr, pg, pb] = pixel(data, width, height, x, y)?;
                    r += pr as u64;
                    g += pg as u64;
                    b += pb as u64;
                    n += 1;
                    x += step_x;
                }
                y += step_y;
            }

            let o = (oy * W + ox) * 3;
            out[o] = r.checked_div(n).unwrap_or(0) as u8;
            out[o + 1] = g.checked_div(n).unwrap_or(0) as u8;
            out[o + 2] = b.checked_div(n).unwrap_or(0) as u8;
        }
    }
    Some(out)
}

/// A separable Gaussian over a preview-sized image.
///
/// Separable because a 2D Gaussian is the product of two 1D ones: two passes of
/// `2r+1` taps instead of one of `(2r+1)²`, for an identical result.
///
/// Edges clamp rather than wrap. Wrapping would pull the colour of one side of
/// the screen onto the other, which is the kind of small wrongness that makes a
/// picture look broken without anyone being able to say why.
fn blur(rgb: &[u8]) -> Vec<u8> {
    let taps: Vec<f32> = (-BLUR_RADIUS..=BLUR_RADIUS)
        .map(|d| {
            let d = d as f32;
            (-(d * d) / (2.0 * BLUR_SIGMA * BLUR_SIGMA)).exp()
        })
        .collect();
    let total: f32 = taps.iter().sum();

    let read = |src: &[u8], x: isize, y: isize, channel: usize| -> f32 {
        let x = x.clamp(0, W as isize - 1) as usize;
        let y = y.clamp(0, H as isize - 1) as usize;
        src[(y * W + x) * 3 + channel] as f32
    };

    let pass = |src: &[u8], horizontal: bool| -> Vec<u8> {
        let mut out = vec![0u8; BYTES];
        for y in 0..H as isize {
            for x in 0..W as isize {
                for channel in 0..3 {
                    let sum: f32 = taps
                        .iter()
                        .enumerate()
                        .map(|(i, weight)| {
                            let offset = i as isize - BLUR_RADIUS;
                            let (dx, dy) = if horizontal { (offset, 0) } else { (0, offset) };
                            weight * read(src, x + dx, y + dy, channel)
                        })
                        .sum();
                    out[(y as usize * W + x as usize) * 3 + channel] = (sum / total) as u8;
                }
            }
        }
        out
    };

    pass(&pass(rgb, true), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_bgra(width: usize, height: usize, bgr: [u8; 3]) -> Vec<u8> {
        [bgr[0], bgr[1], bgr[2], 255].repeat(width * height)
    }

    /// A frame of one colour stays that colour, blur included.
    ///
    /// This catches a normalisation bug: a Gaussian whose weights do not sum to
    /// one darkens or brightens everything, and on a photograph that reads as
    /// "the blur looks a bit off" rather than a clear failure. On a flat field
    /// it is exact.
    #[test]
    fn a_flat_frame_survives_intact() {
        let frame = flat_bgra(128, 72, [200, 90, 40]);
        let preview = from_frame(&frame, 128, 72, PixelFormat::Bgra).expect("a preview");

        assert_eq!(preview.len(), BYTES);
        for pixel in preview.chunks_exact(3) {
            assert_eq!(pixel, [40, 90, 200], "BGRA must be read as RGB, not reversed");
        }
    }

    /// Left and right halves stay on their own sides.
    ///
    /// A downscaler that transposed or mirrored would still produce plausible
    /// colours, so this checks position rather than content.
    #[test]
    fn the_layout_of_the_screen_is_preserved() {
        let (w, h) = (256usize, 144usize);
        let mut frame = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let left = x < w / 2;
                frame[i] = if left { 0 } else { 255 }; // B
                frame[i + 2] = if left { 255 } else { 0 }; // R
                frame[i + 3] = 255;
            }
        }

        let preview = from_frame(&frame, w, h, PixelFormat::Bgra).expect("a preview");
        let at = |x: usize| {
            let o = ((H / 2) * W + x) * 3;
            (preview[o], preview[o + 2])
        };
        let (left_r, left_b) = at(1);
        let (right_r, right_b) = at(W - 2);
        assert!(left_r > left_b, "left should be red, got r{left_r} b{left_b}");
        assert!(right_b > right_r, "right should be blue, got r{right_r} b{right_b}");
    }

    /// The blur has to actually soften something.
    ///
    /// Without this, a blur that silently did nothing would pass every other
    /// test here.
    #[test]
    fn a_hard_edge_becomes_a_gradient() {
        let (w, h) = (256usize, 144usize);
        let mut frame = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let value = if x < w / 2 { 255 } else { 0 };
                frame[i..i + 3].fill(value);
                frame[i + 3] = 255;
            }
        }

        let preview = from_frame(&frame, w, h, PixelFormat::Bgra).expect("a preview");
        let at = |x: usize| preview[((H / 2) * W + x) * 3];
        let midtones = (0..W).filter(|&x| (24..232).contains(&at(x))).count();

        assert!(midtones >= 3, "expected a soft edge, found {midtones} midtone columns");
        assert!(at(0) > 200, "far left should still be bright");
        assert!(at(W - 1) < 55, "far right should still be dark");
    }

    /// NV12 has to come out the same colour BGRA does.
    ///
    /// The host picks its capture format for reasons unrelated to previews, so a
    /// machine converting on the GPU must not get a differently-tinted card from
    /// one that does not.
    #[test]
    fn nv12_and_bgra_agree_on_colour() {
        let (w, h) = (128usize, 72usize);
        // Mid grey is the useful case: it exercises the luma range offset, and
        // any error in the 16..235 handling shows up as a brightness shift.
        let mut nv12 = vec![128u8; w * h + w * h / 2];
        nv12[..w * h].fill(126); // 16 + 0.5 * 219, near mid grey
        let from_nv12 = from_frame(&nv12, w, h, PixelFormat::Nv12).expect("a preview");

        let grey = from_nv12[0];
        assert!((110..146).contains(&grey), "mid grey should decode near 128, got {grey}");
        for pixel in from_nv12.chunks_exact(3) {
            let spread = pixel.iter().max().unwrap() - pixel.iter().min().unwrap();
            assert!(spread <= 2, "neutral chroma must stay neutral, got {pixel:?}");
        }
    }

    /// RGBA and BGRA must not disagree about which end is red.
    ///
    /// The two paths exist because capture and decode hand over different
    /// orders, and swapping one is a mistake that looks plausible until someone
    /// notices every card is the wrong colour.
    #[test]
    fn rgba_and_bgra_are_not_confused() {
        let bgra = flat_bgra(128, 72, [200, 90, 40]);
        let rgba: Vec<u8> = [40u8, 90, 200, 255].repeat(128 * 72);

        let from_bgra = from_frame(&bgra, 128, 72, PixelFormat::Bgra).expect("bgra");
        let from_rgba = from_rgba(&rgba, 128, 72).expect("rgba");
        assert_eq!(from_bgra, from_rgba, "the same picture in two orders");
        assert_eq!(&from_bgra[..3], [40, 90, 200]);
    }

    /// A frame shorter than its dimensions claim must be refused, not read past.
    #[test]
    fn an_undersized_frame_is_refused() {
        assert!(from_frame(&[0; 16], 256, 144, PixelFormat::Bgra).is_none());
        assert!(from_frame(&[0; 16], 256, 144, PixelFormat::Nv12).is_none());
        assert!(from_frame(&[], 0, 0, PixelFormat::Bgra).is_none());
    }
}
