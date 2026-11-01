//! Colour conversion between BGRA, NV12 and RGBA, on the processor.
//!
//! Every path in Tain writes and reads BT.601 limited range, and the two
//! directions here are each other's inverse. They are shared by every platform
//! — the Windows Media Foundation encoder and decoder and the Linux VA-API ones
//! — because two copies of a set of coefficients are two chances for a picture
//! to change colour depending on which encoder and decoder happened to run.
//!
//! Moved here unchanged from `codec_mf` and `codec_mf_decode`, which re-export
//! them under their old paths.

/// Bytes in an NV12 frame: full-size luma plus half-size interleaved chroma.
pub(crate) fn nv12_len(width: u16, height: u16) -> usize {
    let (w, h) = (width as usize, height as usize);
    w * h + w * (h / 2)
}

/// BGRA to NV12: luma plane, then interleaved U/V at half resolution.
///
/// Same BT.601 coefficients as the I420 converter — the two must agree, or the
/// picture shifts colour depending on which encoder happens to be in use.
pub fn bgra_to_nv12(bgra: &[u8], width: u16, height: u16, out: &mut Vec<u8>) {
    out.resize(nv12_len(width, height), 0);
    bgra_to_nv12_into(bgra, width, height, out);
}

/// How many row-pairs one worker converts before the split stops paying.
///
/// Below roughly this much work per thread the spawn costs more than it saves,
/// so small frames stay on one thread.
pub(crate) const ROWS_PER_WORKER: usize = 64;

/// As [`bgra_to_nv12`], but into a caller-owned buffer — which is what lets the
/// encoder convert straight into the media buffer the transform will read,
/// rather than into a staging `Vec` that then has to be copied.
///
/// Split across threads because at 2560x1600 this is 4.1 million pixels of
/// scalar arithmetic per frame, measured at ~7 ms — nearly half the hardware
/// encoder's entire frame time, and pure waste on a machine with idle cores.
/// The bands are disjoint slices of the output, so no synchronisation is needed
/// beyond the scope's join.
pub fn bgra_to_nv12_into(bgra: &[u8], width: u16, height: u16, out: &mut [u8]) {
    let (w, h) = (width as usize, height as usize);
    debug_assert!(bgra.len() >= w * h * 4);
    debug_assert!(out.len() >= nv12_len(width, height));

    let row_pairs = h / 2;
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(row_pairs.div_ceil(ROWS_PER_WORKER))
        .max(1);
    let band = row_pairs.div_ceil(workers);

    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    // One band is `band` row-pairs: 2*band luma rows and `band` chroma rows.
    let bands = y_plane
        .chunks_mut(band * 2 * w)
        .zip(uv_plane.chunks_mut(band * w))
        .enumerate();

    if workers == 1 {
        for (index, (y, uv)) in bands {
            convert_band(bgra, w, index * band, y, uv);
        }
        return;
    }

    std::thread::scope(|scope| {
        for (index, (y, uv)) in bands {
            scope.spawn(move || convert_band(bgra, w, index * band, y, uv));
        }
    });
}

/// Convert one horizontal band, starting at `first_row_pair` of the source.
fn convert_band(bgra: &[u8], w: usize, first_row_pair: usize, y_plane: &mut [u8], uv_plane: &mut [u8]) {
    for local in 0..y_plane.len() / (2 * w) {
        let y0 = (first_row_pair + local) * 2;
        let top = &bgra[y0 * w * 4..(y0 + 1) * w * 4];
        let bottom = &bgra[(y0 + 1) * w * 4..(y0 + 2) * w * 4];
        let (y_top, y_bottom) = y_plane[local * 2 * w..(local + 1) * 2 * w].split_at_mut(w);
        let uv_row = local * w;

        for col_pair in 0..w / 2 {
            let x0 = col_pair * 8;
            let px = [
                (top[x0] as i32, top[x0 + 1] as i32, top[x0 + 2] as i32),
                (top[x0 + 4] as i32, top[x0 + 5] as i32, top[x0 + 6] as i32),
                (bottom[x0] as i32, bottom[x0 + 1] as i32, bottom[x0 + 2] as i32),
                (bottom[x0 + 4] as i32, bottom[x0 + 5] as i32, bottom[x0 + 6] as i32),
            ];
            let luma =
                |(b, g, r): (i32, i32, i32)| (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
            y_top[col_pair * 2] = luma(px[0]);
            y_top[col_pair * 2 + 1] = luma(px[1]);
            y_bottom[col_pair * 2] = luma(px[2]);
            y_bottom[col_pair * 2 + 1] = luma(px[3]);

            let (b, g, r) = (
                (px[0].0 + px[1].0 + px[2].0 + px[3].0) / 4,
                (px[0].1 + px[1].1 + px[2].1 + px[3].1) / 4,
                (px[0].2 + px[1].2 + px[2].2 + px[3].2) / 4,
            );
            // NV12 interleaves U then V, unlike I420's separate planes.
            uv_plane[uv_row + col_pair * 2] =
                (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            uv_plane[uv_row + col_pair * 2 + 1] =
                (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

/// NV12 to RGBA, split across threads.
///
/// The inverse of the encoder's converter and threaded for the same reason: at
/// 1600p it is four million pixels of scalar arithmetic sitting on the guest's
/// critical path, and leaving it on one core is how a fast machine still reports
/// as "slow decoding".
pub fn nv12_to_rgba(nv12: &[u8], width: u16, height: u16, out: &mut [u8]) {
    let (w, h) = (width as usize, height as usize);
    debug_assert!(nv12.len() >= nv12_len(width, height));
    debug_assert!(out.len() >= w * h * 4);

    let (y_plane, uv_plane) = nv12.split_at(w * h);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(h.div_ceil(ROWS_PER_WORKER))
        .max(1);
    let band = h.div_ceil(workers);

    let bands = out[..w * h * 4].chunks_mut(band * w * 4).enumerate();
    if workers == 1 {
        for (index, dst) in bands {
            rgba_band(y_plane, uv_plane, w, index * band, dst);
        }
        return;
    }
    std::thread::scope(|scope| {
        for (index, dst) in bands {
            scope.spawn(move || rgba_band(y_plane, uv_plane, w, index * band, dst));
        }
    });
}

/// Convert one horizontal band of NV12 into RGBA.
fn rgba_band(y_plane: &[u8], uv_plane: &[u8], w: usize, first_row: usize, out: &mut [u8]) {
    for local in 0..out.len() / (w * 4) {
        let y_row = first_row + local;
        let uv_row = (y_row / 2) * w;
        for x in 0..w {
            // BT.601 limited range — the inverse of the coefficients the encoder
            // side uses. The two must agree or the picture shifts colour
            // depending on which decoder happened to run.
            let y = (y_plane[y_row * w + x] as i32 - 16) * 298;
            let chroma = uv_row + (x & !1);
            let u = uv_plane[chroma] as i32 - 128;
            let v = uv_plane[chroma + 1] as i32 - 128;

            let i = (local * w + x) * 4;
            out[i] = ((y + 409 * v + 128) >> 8).clamp(0, 255) as u8;
            out[i + 1] = ((y - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
            out[i + 2] = ((y + 516 * u + 128) >> 8).clamp(0, 255) as u8;
            out[i + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Red-ish on the left half, blue-ish on the right.
    fn pattern(width: u16, height: u16) -> Vec<u8> {
        let mut data = vec![0u8; width as usize * height as usize * 4];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let i = (y * width as usize + x) * 4;
                let left = x < width as usize / 2;
                data[i] = if left { 30 } else { 200 };
                data[i + 1] = 40;
                data[i + 2] = if left { 220 } else { 40 };
                data[i + 3] = 255;
            }
        }
        data
    }

    #[test]
    fn nv12_is_sized_and_laid_out_correctly() {
        assert_eq!(nv12_len(64, 32), 64 * 32 + 64 * 16);
        let mut out = Vec::new();
        bgra_to_nv12(&pattern(64, 32), 64, 32, &mut out);
        assert_eq!(out.len(), nv12_len(64, 32));
        let row = &out[..64];
        let avg = |s: &[u8]| s.iter().map(|v| *v as u32).sum::<u32>() / s.len() as u32;
        assert_ne!(avg(&row[..32]), avg(&row[32..]), "luma must differ across the split");
    }

    #[test]
    fn nv12_and_i420_agree_on_colour() {
        // The software encoder converts to I420 and the hardware ones to NV12;
        // they must agree or the picture changes colour with the encoder.
        let (w, h) = (64usize, 32usize);
        let src = pattern(w as u16, h as u16);
        let mut i420 = crate::codec_h264::I420Frame::new(w, h);
        i420.read_bgra(&src);
        let mut nv12 = Vec::new();
        bgra_to_nv12(&src, w as u16, h as u16, &mut nv12);

        use openh264::formats::YUVSource;
        assert_eq!(&nv12[..w * h], i420.y());
        let uv = &nv12[w * h..];
        let u: Vec<u8> = uv.iter().step_by(2).copied().collect();
        let v: Vec<u8> = uv.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(u.as_slice(), i420.u());
        assert_eq!(v.as_slice(), i420.v());
    }

    #[test]
    fn grey_stays_grey() {
        let (w, h) = (64u16, 32u16);
        let mut nv12 = vec![128u8; nv12_len(w, h)];
        nv12[..w as usize * h as usize].fill(128);
        let mut rgba = vec![0u8; w as usize * h as usize * 4];
        nv12_to_rgba(&nv12, w, h, &mut rgba);
        for px in rgba.chunks_exact(4) {
            assert!(px[0].abs_diff(px[1]) <= 2 && px[1].abs_diff(px[2]) <= 2, "{px:?}");
            assert!((125..=135).contains(&px[0]), "{px:?}");
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn a_frame_survives_the_round_trip() {
        // Large enough to be split across threads in both directions.
        let (w, h) = (640u16, 360u16);
        let bgra = pattern(w, h);
        let mut nv12 = Vec::new();
        bgra_to_nv12(&bgra, w, h, &mut nv12);
        let mut rgba = vec![0u8; w as usize * h as usize * 4];
        nv12_to_rgba(&nv12, w, h, &mut rgba);
        for (src, out) in bgra.chunks_exact(4).zip(rgba.chunks_exact(4)) {
            // Each half is a flat colour, so 4:2:0 loses nothing but rounding.
            assert!(out[0].abs_diff(src[2]) <= 3, "red {} vs {}", out[0], src[2]);
            assert!(out[1].abs_diff(src[1]) <= 3, "green {} vs {}", out[1], src[1]);
            assert!(out[2].abs_diff(src[0]) <= 3, "blue {} vs {}", out[2], src[0]);
        }
    }
}
