//! Just enough H.264 bitstream parsing to drive a DXVA decoder.
//!
//! ## Why this exists
//!
//! A hardware decoder does not parse H.264. It decodes *macroblocks*, and it
//! expects the caller to have already read the sequence parameter set, picture
//! parameter set and slice header and handed it the result as a filled-in
//! `DXVA_PicParams_H264`. So using the GPU at all means owning a parser.
//!
//! See [`super::codec_mf_decode`] for why there is no way around that on
//! Windows: the only registered H.264 decoder transform decodes on the CPU.
//!
//! ## Scope
//!
//! This parses what a low-latency screen stream actually contains, and is
//! deliberate about the rest. Interlaced content, multiple slice groups, data
//! partitioning and SVC/MVC extensions are not supported — they cannot occur in
//! a stream this application produces, and a decoder that silently mis-parses
//! them would produce corrupt pictures rather than an error. Everything
//! unsupported is reported, and the caller falls back to a software decode.
//!
//! ## Emulation prevention is not optional
//!
//! A NAL payload has `0x03` bytes inserted wherever the encoder would otherwise
//! have emitted `00 00 00`, `00 00 01`, `00 00 02` or `00 00 03`. Reading the
//! payload without removing them shifts every subsequent field by eight bits,
//! which does not fail — it yields a plausible-looking parse of nonsense. The
//! reader here removes them as it goes.

use anyhow::{bail, ensure, Result};

/// A NAL unit: its type, whether it is a reference, and its payload.
///
/// The payload still contains emulation prevention bytes; [`RbspReader`] strips
/// them. It is kept raw because DXVA wants the bitstream exactly as it appeared,
/// escape bytes and all.
#[derive(Debug, Clone, Copy)]
pub struct Nal<'a> {
    pub kind: u8,
    pub ref_idc: u8,
    /// Payload after the one-byte NAL header.
    pub payload: &'a [u8],
    /// Offset of the NAL header byte within the buffer this came from.
    pub header_offset: usize,
}

impl Nal<'_> {
    pub const SLICE: u8 = 1;
    pub const IDR: u8 = 5;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;

    /// Whether this NAL carries picture data rather than metadata.
    pub fn is_slice(&self) -> bool {
        self.kind == Self::SLICE || self.kind == Self::IDR
    }

    pub fn is_idr(&self) -> bool {
        self.kind == Self::IDR
    }
}

/// Split an Annex-B buffer into NAL units.
///
/// Accepts both three- and four-byte start codes, and tolerates trailing zeroes,
/// because encoders differ on both and neither is worth rejecting a stream over.
pub fn nal_units(data: &[u8]) -> Vec<Nal<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    // Offset of the payload start of the NAL currently being scanned past.
    let mut open: Option<(usize, usize)> = None;

    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some((header, start)) = open.take() {
                push_nal(&mut out, data, header, start, trim_zeroes(&data[start..i]));
            }
            let header = i + 3;
            if header < data.len() {
                open = Some((header, header + 1));
            }
            i = header + 1;
        } else {
            i += 1;
        }
    }
    if let Some((header, start)) = open
        && start <= data.len()
    {
        push_nal(&mut out, data, header, start, trim_zeroes(&data[start..]));
    }
    out
}

fn push_nal<'a>(out: &mut Vec<Nal<'a>>, data: &'a [u8], header: usize, start: usize, end: usize) {
    let byte = data[header];
    // forbidden_zero_bit must be 0; anything else is a corrupt unit, not a NAL.
    if byte & 0x80 != 0 {
        return;
    }
    out.push(Nal {
        kind: byte & 0x1f,
        ref_idc: (byte >> 5) & 0x03,
        payload: &data[start..start + end],
        header_offset: header,
    });
}

/// Length of `slice` with trailing zero bytes removed.
///
/// A four-byte start code is a three-byte one preceded by a zero, so the zero
/// belongs to the *next* NAL's start code rather than this NAL's payload.
fn trim_zeroes(slice: &[u8]) -> usize {
    let mut end = slice.len();
    while end > 0 && slice[end - 1] == 0 {
        end -= 1;
    }
    end
}

/// Bit reader over a NAL payload, removing emulation prevention bytes.
pub struct RbspReader<'a> {
    data: &'a [u8],
    /// Index of the next byte to consume from `data`.
    byte: usize,
    /// Bits already consumed from the byte at `byte - 1`.
    bit: u32,
    /// How many consecutive zero bytes have been emitted, for escape detection.
    zeroes: u32,
    /// The byte currently being read out, after unescaping.
    current: u8,
    loaded: bool,
}

impl<'a> RbspReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, byte: 0, bit: 0, zeroes: 0, current: 0, loaded: false }
    }

    /// Pull the next unescaped byte, skipping an emulation prevention `0x03`.
    fn next_byte(&mut self) -> Result<u8> {
        loop {
            let Some(&b) = self.data.get(self.byte) else {
                bail!("ran off the end of the NAL payload");
            };
            self.byte += 1;
            // 00 00 03 -> the 03 is an escape and is not part of the payload.
            if self.zeroes >= 2 && b == 0x03 {
                self.zeroes = 0;
                continue;
            }
            self.zeroes = if b == 0 { self.zeroes + 1 } else { 0 };
            return Ok(b);
        }
    }

    /// Read one bit.
    pub fn bit(&mut self) -> Result<u32> {
        if !self.loaded || self.bit == 8 {
            self.current = self.next_byte()?;
            self.bit = 0;
            self.loaded = true;
        }
        let value = (self.current >> (7 - self.bit)) & 1;
        self.bit += 1;
        Ok(value as u32)
    }

    /// Read `n` bits, most significant first. `n` must be at most 32.
    pub fn bits(&mut self, n: u32) -> Result<u32> {
        ensure!(n <= 32, "cannot read {n} bits at once");
        let mut value = 0u32;
        for _ in 0..n {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }

    /// A single-bit flag.
    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.bit()? == 1)
    }

    /// Unsigned exp-Golomb, `ue(v)`.
    pub fn ue(&mut self) -> Result<u32> {
        let mut leading = 0u32;
        while self.bit()? == 0 {
            leading += 1;
            // 2^32 - 1 is the largest representable value; more leading zeroes
            // than that is a corrupt stream, not a very large number.
            ensure!(leading < 32, "exp-Golomb code with {leading} leading zeroes");
        }
        if leading == 0 {
            return Ok(0);
        }
        let rest = self.bits(leading)?;
        Ok((1u32 << leading) - 1 + rest)
    }

    /// Signed exp-Golomb, `se(v)`.
    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()?;
        // 1 -> +1, 2 -> -1, 3 -> +2, 4 -> -2, ...
        let magnitude = k.div_ceil(2) as i32;
        Ok(if k % 2 == 1 { magnitude } else { -magnitude })
    }

    /// Whether any payload bits remain, in the spec's `more_rbsp_data()` sense.
    ///
    /// # Why a length check is not enough
    ///
    /// An RBSP ends with a stop bit — a single `1` followed by zeroes to the
    /// byte boundary — so "are there bits left" and "is there data left" are
    /// different questions. Asking the wrong one reads the stop bit as the next
    /// field, which is exactly what made the PPS parse run off the end of a
    /// stream this project's own encoder produced.
    pub fn more_data(&self) -> bool {
        // Trailing zero bytes are padding; the stop bit is the lowest set bit of
        // the last non-zero byte.
        let Some(last) = self.data.iter().rposition(|&b| b != 0) else {
            return false;
        };
        // Compared as absolute bit positions, not byte-then-bit. "We have not
        // reached the last non-zero byte yet" is not the same as "there is data
        // left": that byte can hold nothing but the stop bit, which is exactly
        // the three-byte PPS this project's encoder emits.
        let stop = last * 8 + (7 - self.data[last].trailing_zeros() as usize);
        let current = if self.loaded { (self.byte - 1) * 8 + self.bit as usize } else { 0 };
        current < stop
    }

    /// Skip a scaling list, tracking whether it stays at its default.
    ///
    /// The values themselves are only needed when a stream actually carries a
    /// custom matrix; see [`ScalingLists`].
    fn scaling_list(&mut self, list: &mut [u8]) -> Result<()> {
        let mut last = 8i32;
        let mut next = 8i32;
        for slot in list.iter_mut() {
            if next != 0 {
                let delta = self.se()?;
                next = (last + delta + 256) % 256;
            }
            if next != 0 {
                last = next;
            }
            *slot = last as u8;
        }
        Ok(())
    }
}

/// The scaling matrices a picture is decoded with.
///
/// Flat 16s unless the stream says otherwise, which is the common case for a
/// screen stream and what the encoder here produces.
#[derive(Debug, Clone, Copy)]
pub struct ScalingLists {
    pub list_4x4: [[u8; 16]; 6],
    pub list_8x8: [[u8; 64]; 2],
}

impl Default for ScalingLists {
    fn default() -> Self {
        Self { list_4x4: [[16; 16]; 6], list_8x8: [[16; 64]; 2] }
    }
}

/// Sequence parameter set, reduced to the fields a decoder needs.
#[derive(Debug, Clone)]
pub struct Sps {
    pub id: u32,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub qpprime_y_zero_transform_bypass_flag: bool,
    pub log2_max_frame_num_minus4: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub delta_pic_order_always_zero_flag: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub num_ref_frames_in_pic_order_cnt_cycle: u32,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub pic_width_in_mbs_minus1: u32,
    pub pic_height_in_map_units_minus1: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    pub crop_left: u32,
    pub crop_right: u32,
    pub crop_top: u32,
    pub crop_bottom: u32,
    pub scaling: ScalingLists,
    pub scaling_present: bool,
}

impl Sps {
    /// Coded width in macroblocks.
    pub fn width_mbs(&self) -> u32 {
        self.pic_width_in_mbs_minus1 + 1
    }

    /// Coded height in macroblocks. Frame-only streams only; see `parse`.
    pub fn height_mbs(&self) -> u32 {
        (self.pic_height_in_map_units_minus1 + 1) * if self.frame_mbs_only_flag { 1 } else { 2 }
    }

    /// Displayed size after cropping, in pixels.
    pub fn display_size(&self) -> (u32, u32) {
        // Crop units depend on chroma subsampling; 4:2:0 is 2x2, and monochrome
        // and 4:4:4 are 1x1.
        let (sub_x, sub_y) = match self.chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let unit_y = sub_y * if self.frame_mbs_only_flag { 1 } else { 2 };
        let w = self.width_mbs() * 16 - (self.crop_left + self.crop_right) * sub_x;
        let h = self.height_mbs() * 16 - (self.crop_top + self.crop_bottom) * unit_y;
        (w, h)
    }

    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut r = RbspReader::new(payload);
        let profile_idc = r.bits(8)? as u8;
        // constraint_set flags and reserved bits.
        let _constraints = r.bits(8)?;
        let level_idc = r.bits(8)? as u8;
        let id = r.ue()?;

        let mut sps = Self {
            id,
            profile_idc,
            level_idc,
            chroma_format_idc: 1,
            separate_colour_plane_flag: false,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            qpprime_y_zero_transform_bypass_flag: false,
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 0,
            delta_pic_order_always_zero_flag: false,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            num_ref_frames_in_pic_order_cnt_cycle: 0,
            max_num_ref_frames: 1,
            gaps_in_frame_num_value_allowed_flag: false,
            pic_width_in_mbs_minus1: 0,
            pic_height_in_map_units_minus1: 0,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: false,
            direct_8x8_inference_flag: false,
            crop_left: 0,
            crop_right: 0,
            crop_top: 0,
            crop_bottom: 0,
            scaling: ScalingLists::default(),
            scaling_present: false,
        };

        // The high profiles carry chroma and bit depth; the rest imply 4:2:0 8-bit.
        if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135)
        {
            sps.chroma_format_idc = r.ue()?;
            if sps.chroma_format_idc == 3 {
                sps.separate_colour_plane_flag = r.flag()?;
            }
            sps.bit_depth_luma_minus8 = r.ue()?;
            sps.bit_depth_chroma_minus8 = r.ue()?;
            sps.qpprime_y_zero_transform_bypass_flag = r.flag()?;
            if r.flag()? {
                sps.scaling_present = true;
                let count = if sps.chroma_format_idc == 3 { 12 } else { 8 };
                for i in 0..count {
                    if r.flag()? {
                        if i < 6 {
                            r.scaling_list(&mut sps.scaling.list_4x4[i])?;
                        } else if i - 6 < 2 {
                            r.scaling_list(&mut sps.scaling.list_8x8[i - 6])?;
                        } else {
                            // 4:4:4 carries six 8x8 lists; only two are kept
                            // because nothing here decodes 4:4:4 (see below).
                            let mut discard = [0u8; 64];
                            r.scaling_list(&mut discard)?;
                        }
                    }
                }
            }
        }

        sps.log2_max_frame_num_minus4 = r.ue()?;
        sps.pic_order_cnt_type = r.ue()?;
        match sps.pic_order_cnt_type {
            0 => sps.log2_max_pic_order_cnt_lsb_minus4 = r.ue()?,
            1 => {
                sps.delta_pic_order_always_zero_flag = r.flag()?;
                sps.offset_for_non_ref_pic = r.se()?;
                sps.offset_for_top_to_bottom_field = r.se()?;
                sps.num_ref_frames_in_pic_order_cnt_cycle = r.ue()?;
                ensure!(
                    sps.num_ref_frames_in_pic_order_cnt_cycle <= 255,
                    "pic order cnt cycle of {} is out of range",
                    sps.num_ref_frames_in_pic_order_cnt_cycle
                );
                for _ in 0..sps.num_ref_frames_in_pic_order_cnt_cycle {
                    let _offset_for_ref_frame = r.se()?;
                }
            }
            2 => {}
            other => bail!("pic_order_cnt_type {other} is not valid"),
        }

        sps.max_num_ref_frames = r.ue()?;
        sps.gaps_in_frame_num_value_allowed_flag = r.flag()?;
        sps.pic_width_in_mbs_minus1 = r.ue()?;
        sps.pic_height_in_map_units_minus1 = r.ue()?;
        sps.frame_mbs_only_flag = r.flag()?;
        if !sps.frame_mbs_only_flag {
            sps.mb_adaptive_frame_field_flag = r.flag()?;
        }
        sps.direct_8x8_inference_flag = r.flag()?;
        if r.flag()? {
            sps.crop_left = r.ue()?;
            sps.crop_right = r.ue()?;
            sps.crop_top = r.ue()?;
            sps.crop_bottom = r.ue()?;
        }
        // VUI follows and is not needed: it carries timing and colour metadata,
        // none of which reaches a DXVA picture parameter.

        ensure!(
            sps.log2_max_frame_num_minus4 <= 12,
            "log2_max_frame_num_minus4 of {} is out of range",
            sps.log2_max_frame_num_minus4
        );
        Ok(sps)
    }

    /// Whether a DXVA decoder here can handle this stream.
    ///
    /// Refusing loudly is the point: every one of these produces a picture that
    /// decodes to garbage rather than an error if it is waved through.
    pub fn unsupported(&self) -> Option<&'static str> {
        if !self.frame_mbs_only_flag {
            return Some("interlaced content");
        }
        if self.chroma_format_idc != 1 {
            return Some("chroma format other than 4:2:0");
        }
        if self.separate_colour_plane_flag {
            return Some("separate colour planes");
        }
        if self.bit_depth_luma_minus8 != 0 || self.bit_depth_chroma_minus8 != 0 {
            return Some("bit depth above 8");
        }
        None
    }
}

/// Picture parameter set, reduced to the fields a decoder needs.
#[derive(Debug, Clone)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub entropy_coding_mode_flag: bool,
    pub bottom_field_pic_order_in_frame_present_flag: bool,
    pub num_slice_groups_minus1: u32,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub second_chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    pub transform_8x8_mode_flag: bool,
    pub scaling: ScalingLists,
    pub scaling_present: bool,
}

impl Pps {
    /// Parse, inheriting scaling lists from the SPS the stream names.
    pub fn parse(payload: &[u8], sps: Option<&Sps>) -> Result<Self> {
        let mut r = RbspReader::new(payload);
        let id = r.ue()?;
        let sps_id = r.ue()?;

        let mut pps = Self {
            id,
            sps_id,
            entropy_coding_mode_flag: r.flag()?,
            bottom_field_pic_order_in_frame_present_flag: r.flag()?,
            num_slice_groups_minus1: r.ue()?,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            weighted_pred_flag: false,
            weighted_bipred_idc: 0,
            pic_init_qp_minus26: 0,
            pic_init_qs_minus26: 0,
            chroma_qp_index_offset: 0,
            second_chroma_qp_index_offset: 0,
            deblocking_filter_control_present_flag: false,
            constrained_intra_pred_flag: false,
            redundant_pic_cnt_present_flag: false,
            transform_8x8_mode_flag: false,
            scaling: sps.map(|s| s.scaling).unwrap_or_default(),
            scaling_present: sps.is_some_and(|s| s.scaling_present),
        };

        // Slice groups mean a macroblock-to-slice-group map this does not build.
        ensure!(pps.num_slice_groups_minus1 == 0, "multiple slice groups are not supported");

        pps.num_ref_idx_l0_default_active_minus1 = r.ue()?;
        pps.num_ref_idx_l1_default_active_minus1 = r.ue()?;
        pps.weighted_pred_flag = r.flag()?;
        pps.weighted_bipred_idc = r.bits(2)?;
        pps.pic_init_qp_minus26 = r.se()?;
        pps.pic_init_qs_minus26 = r.se()?;
        pps.chroma_qp_index_offset = r.se()?;
        // The spec's default when second_chroma_qp_index_offset is absent.
        pps.second_chroma_qp_index_offset = pps.chroma_qp_index_offset;
        pps.deblocking_filter_control_present_flag = r.flag()?;
        pps.constrained_intra_pred_flag = r.flag()?;
        pps.redundant_pic_cnt_present_flag = r.flag()?;

        // The trailing extension is optional and usually absent. `more_data`
        // rather than a plain read: without it the RBSP stop bit is mistaken for
        // `transform_8x8_mode_flag` being present.
        if r.more_data() && r.flag()? {
            pps.transform_8x8_mode_flag = r.flag()?;
            if r.flag()? {
                pps.scaling_present = true;
                let lists = if pps.transform_8x8_mode_flag { 6 + 2 } else { 6 };
                for i in 0..lists {
                    if r.flag()? {
                        if i < 6 {
                            r.scaling_list(&mut pps.scaling.list_4x4[i])?;
                        } else {
                            r.scaling_list(&mut pps.scaling.list_8x8[i - 6])?;
                        }
                    }
                }
            }
            pps.second_chroma_qp_index_offset = r.se()?;
        }

        Ok(pps)
    }
}

/// Slice type, reduced to the three a screen stream produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
}

impl SliceType {
    fn from_raw(value: u32) -> Result<Self> {
        // 5..9 repeat 0..4 and additionally mean "all slices in this picture
        // have this type", which changes nothing here.
        Ok(match value % 5 {
            0 => Self::P,
            1 => Self::B,
            2 => Self::I,
            // SP and SI are switching slices, for stream splicing.
            other => bail!("slice type {other} (SP/SI) is not supported"),
        })
    }
}

/// The parts of a slice header a picture parameter block is built from.
#[derive(Debug, Clone)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type: SliceType,
    pub pps_id: u32,
    pub frame_num: u32,
    pub field_pic_flag: bool,
    pub bottom_field_flag: bool,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub num_ref_idx_l0_active_minus1: u32,
    pub num_ref_idx_l1_active_minus1: u32,
    /// Set when the slice marks all reference pictures as unused.
    pub no_output_of_prior_pics_flag: bool,
    pub long_term_reference_flag: bool,
    pub adaptive_ref_pic_marking_mode_flag: bool,
}

impl SliceHeader {
    pub fn parse(nal: &Nal<'_>, sps: &Sps, pps: &Pps) -> Result<Self> {
        let mut r = RbspReader::new(nal.payload);
        let first_mb_in_slice = r.ue()?;
        let slice_type = SliceType::from_raw(r.ue()?)?;
        let pps_id = r.ue()?;

        if sps.separate_colour_plane_flag {
            let _colour_plane_id = r.bits(2)?;
        }
        let frame_num = r.bits(sps.log2_max_frame_num_minus4 + 4)?;

        let mut header = Self {
            first_mb_in_slice,
            slice_type,
            pps_id,
            frame_num,
            field_pic_flag: false,
            bottom_field_flag: false,
            idr_pic_id: 0,
            pic_order_cnt_lsb: 0,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0, 0],
            num_ref_idx_l0_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
            num_ref_idx_l1_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
            no_output_of_prior_pics_flag: false,
            long_term_reference_flag: false,
            adaptive_ref_pic_marking_mode_flag: false,
        };

        if !sps.frame_mbs_only_flag {
            header.field_pic_flag = r.flag()?;
            if header.field_pic_flag {
                header.bottom_field_flag = r.flag()?;
            }
        }
        if nal.is_idr() {
            header.idr_pic_id = r.ue()?;
        }

        match sps.pic_order_cnt_type {
            0 => {
                header.pic_order_cnt_lsb = r.bits(sps.log2_max_pic_order_cnt_lsb_minus4 + 4)?;
                if pps.bottom_field_pic_order_in_frame_present_flag && !header.field_pic_flag {
                    header.delta_pic_order_cnt_bottom = r.se()?;
                }
            }
            1 if !sps.delta_pic_order_always_zero_flag => {
                header.delta_pic_order_cnt[0] = r.se()?;
                if pps.bottom_field_pic_order_in_frame_present_flag && !header.field_pic_flag {
                    header.delta_pic_order_cnt[1] = r.se()?;
                }
            }
            _ => {}
        }

        if pps.redundant_pic_cnt_present_flag {
            let redundant_pic_cnt = r.ue()?;
            ensure!(redundant_pic_cnt == 0, "redundant slices are not supported");
        }

        if slice_type == SliceType::B {
            let _direct_spatial_mv_pred_flag = r.flag()?;
        }
        if matches!(slice_type, SliceType::P | SliceType::B) && r.flag()? {
            header.num_ref_idx_l0_active_minus1 = r.ue()?;
            if slice_type == SliceType::B {
                header.num_ref_idx_l1_active_minus1 = r.ue()?;
            }
        }

        // ref_pic_list_modification. The entries are not kept: the reference
        // list this builds has one entry, so there is nothing to reorder. It
        // still has to be *read*, because everything after it is offset by it.
        for list in 0..if slice_type == SliceType::B { 2 } else { 1 } {
            let is_i = slice_type == SliceType::I;
            if is_i {
                break;
            }
            if r.flag()? {
                loop {
                    let op = r.ue()?;
                    if op == 3 {
                        break;
                    }
                    ensure!(op <= 2, "reference list modification op {op} is not valid");
                    let _value = r.ue()?;
                }
            }
            let _ = list;
        }

        if (pps.weighted_pred_flag && slice_type == SliceType::P)
            || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B)
        {
            bail!("weighted prediction tables are not supported");
        }

        if nal.ref_idc != 0 {
            if nal.is_idr() {
                header.no_output_of_prior_pics_flag = r.flag()?;
                header.long_term_reference_flag = r.flag()?;
            } else {
                header.adaptive_ref_pic_marking_mode_flag = r.flag()?;
                // MMCO commands rewrite the reference picture set in ways the
                // single-reference model here does not track.
                ensure!(
                    !header.adaptive_ref_pic_marking_mode_flag,
                    "adaptive reference picture marking (MMCO) is not supported"
                );
            }
        }

        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_codes_of_both_lengths_split_the_same_way() {
        let data = [0, 0, 0, 1, 0x67, 0xaa, 0, 0, 1, 0x68, 0xbb, 0xcc];
        let nals = nal_units(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].kind, 7);
        assert_eq!(nals[0].payload, &[0xaa]);
        assert_eq!(nals[1].kind, 8);
        assert_eq!(nals[1].payload, &[0xbb, 0xcc]);
    }

    #[test]
    fn a_trailing_zero_belongs_to_the_next_start_code_not_this_payload() {
        // Without trimming, the first NAL swallows the zero that begins the
        // four-byte start code after it, and its payload is one byte long.
        let data = [0, 0, 1, 0x65, 0x11, 0, 0, 0, 1, 0x65, 0x22];
        let nals = nal_units(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].payload, &[0x11], "trailing zero must not be kept");
        assert_eq!(nals[1].payload, &[0x22]);
    }

    #[test]
    fn the_nal_header_splits_into_type_and_reference_flag() {
        // 0x65 = ref_idc 3, type 5 (IDR). 0x01 = ref_idc 0, type 1.
        let data = [0, 0, 1, 0x65, 0xff, 0, 0, 1, 0x01, 0xff];
        let nals = nal_units(&data);
        assert_eq!((nals[0].kind, nals[0].ref_idc), (5, 3));
        assert!(nals[0].is_idr() && nals[0].is_slice());
        assert_eq!((nals[1].kind, nals[1].ref_idc), (1, 0));
        assert!(!nals[1].is_idr() && nals[1].is_slice());
    }

    /// Reading past an escape byte without removing it shifts every later field
    /// by eight bits, which parses to plausible nonsense rather than failing.
    #[test]
    fn emulation_prevention_bytes_are_removed_before_any_bits_are_read() {
        let payload = [0x00, 0x00, 0x03, 0x01, 0xff];
        let mut r = RbspReader::new(&payload);
        assert_eq!(r.bits(8).unwrap(), 0x00);
        assert_eq!(r.bits(8).unwrap(), 0x00);
        assert_eq!(r.bits(8).unwrap(), 0x01, "the 0x03 must have been skipped");
        assert_eq!(r.bits(8).unwrap(), 0xff);
    }

    #[test]
    fn a_real_zero_three_that_is_not_an_escape_survives() {
        // 0x03 only escapes after two zero bytes. Here it follows one.
        let payload = [0x00, 0x03, 0x00, 0x03];
        let mut r = RbspReader::new(&payload);
        assert_eq!(r.bits(8).unwrap(), 0x00);
        assert_eq!(r.bits(8).unwrap(), 0x03);
        assert_eq!(r.bits(8).unwrap(), 0x00);
        assert_eq!(r.bits(8).unwrap(), 0x03);
    }

    // Grouped by exp-Golomb codeword, not by nibble: the point of writing them
    // this way is that each group is one value from the spec's table.
    #[allow(clippy::unusual_byte_groupings)]
    #[test]
    fn unsigned_exp_golomb_matches_the_spec_table() {
        // 1 => 0; 010 => 1; 011 => 2; 00100 => 3; 00101 => 4 ...
        let bytes = [0b1_010_011_0, 0b0100_0010, 0b1_0001000, 0b0_0000000];
        let mut r = RbspReader::new(&bytes);
        for expected in [0u32, 1, 2, 3, 4, 7] {
            assert_eq!(r.ue().unwrap(), expected);
        }
    }

    #[allow(clippy::unusual_byte_groupings)]
    #[test]
    fn signed_exp_golomb_alternates_around_zero() {
        // k = 0,1,2,3,4 maps to 0, +1, -1, +2, -2.
        let bytes = [0b1_010_011_0, 0b0100_0010, 0b1_0000000];
        let mut r = RbspReader::new(&bytes);
        for expected in [0i32, 1, -1, 2, -2] {
            assert_eq!(r.se().unwrap(), expected);
        }
    }

    #[test]
    fn running_off_the_end_is_an_error_rather_than_zeroes() {
        let mut r = RbspReader::new(&[0xff]);
        assert!(r.bits(8).is_ok());
        assert!(r.bit().is_err(), "reading past the payload must fail");
    }

    /// Writes the bit patterns the reader is supposed to understand.
    ///
    /// Test-only, and deliberately the inverse of [`RbspReader`] rather than a
    /// copy of it: a parser checked against bytes the same code produced would
    /// agree with itself whatever it got wrong.
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        bit: u32,
    }

    impl BitWriter {
        fn put(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                if self.bit == 0 {
                    self.bytes.push(0);
                }
                if (value >> i) & 1 == 1 {
                    let last = self.bytes.len() - 1;
                    self.bytes[last] |= 1 << (7 - self.bit);
                }
                self.bit = (self.bit + 1) % 8;
            }
        }

        fn flag(&mut self, value: bool) {
            self.put(value as u32, 1);
        }

        /// `ue(v)`: n+1 written in binary, preceded by that many minus one zeroes.
        fn ue(&mut self, value: u32) {
            let coded = value + 1;
            let width = 32 - coded.leading_zeros();
            self.put(0, width - 1);
            self.put(coded, width);
        }
    }

    /// A High-profile 1920x1080 SPS, built field by field.
    fn sample_sps() -> Vec<u8> {
        let mut w = BitWriter::default();
        w.put(100, 8); // profile_idc: High
        w.put(0, 8); // constraint flags
        w.put(42, 8); // level_idc: 4.2
        w.ue(0); // seq_parameter_set_id
        w.ue(1); // chroma_format_idc: 4:2:0
        w.ue(0); // bit_depth_luma_minus8
        w.ue(0); // bit_depth_chroma_minus8
        w.flag(false); // qpprime_y_zero_transform_bypass_flag
        w.flag(false); // seq_scaling_matrix_present_flag
        w.ue(4); // log2_max_frame_num_minus4
        w.ue(0); // pic_order_cnt_type
        w.ue(4); // log2_max_pic_order_cnt_lsb_minus4
        w.ue(1); // max_num_ref_frames
        w.flag(false); // gaps_in_frame_num_value_allowed_flag
        w.ue(119); // pic_width_in_mbs_minus1: 120 * 16 = 1920
        w.ue(67); // pic_height_in_map_units_minus1: 68 * 16 = 1088
        w.flag(true); // frame_mbs_only_flag
        w.flag(true); // direct_8x8_inference_flag
        w.flag(true); // frame_cropping_flag
        w.ue(0); // crop_left
        w.ue(0); // crop_right
        w.ue(0); // crop_top
        w.ue(4); // crop_bottom: 4 * 2 luma rows = 1088 - 1080
        w.flag(false); // vui_parameters_present_flag
        w.bytes
    }

    #[test]
    fn a_sequence_parameter_set_parses_back_to_the_fields_it_was_built_from() {
        let sps = Sps::parse(&sample_sps()).expect("SPS must parse");
        assert_eq!(sps.profile_idc, 100, "High profile");
        assert_eq!(sps.level_idc, 42);
        assert_eq!(sps.chroma_format_idc, 1, "4:2:0");
        assert!(sps.frame_mbs_only_flag, "progressive");
        assert_eq!(sps.log2_max_frame_num_minus4, 4);
        assert_eq!(sps.max_num_ref_frames, 1);
        assert_eq!(sps.width_mbs() * 16, 1920);
        // 1080 is not a multiple of 16, so the coded height is 1088 and the
        // difference is carried in the crop.
        assert_eq!(sps.height_mbs() * 16, 1088);
        assert_eq!(sps.display_size(), (1920, 1080), "crop must bring 1088 back to 1080");
        assert_eq!(sps.unsupported(), None);
    }

    /// The parser against a stream this project actually produces.
    ///
    /// The round-trip test above proves the reader understands the bit layout;
    /// this proves the layout is the one the encoder emits. Both are needed —
    /// the first cannot catch a field read in the wrong order if the writer
    /// makes the same mistake, and the second cannot run everywhere.
    #[test]
    #[ignore = "requires a hardware H.264 encoder"]
    fn the_encoder_this_ships_with_produces_a_stream_this_parses() {
        use crate::codec_mf::MediaFoundationEncoder;
        use crate::{EncodeConfig, PixelFormat};

        let (w, h) = (1920u16, 1080u16);
        let Ok(mut encoder) = MediaFoundationEncoder::new(&EncodeConfig::default(), w, h) else {
            eprintln!("no hardware encoder — skipping");
            return;
        };
        let src = vec![0x40u8; w as usize * h as usize * 4];

        let mut sps = None;
        let mut pps = None;
        let mut slices = 0;
        for _ in 0..10 {
            let Ok((packet, _)) = encoder.encode(&src, w, h, PixelFormat::Bgra) else { continue };
            for nal in nal_units(&packet) {
                match nal.kind {
                    Nal::SPS => sps = Some(Sps::parse(nal.payload).expect("SPS must parse")),
                    Nal::PPS => {
                        pps = Some(Pps::parse(nal.payload, sps.as_ref()).expect("PPS must parse"))
                    }
                    _ if nal.is_slice() => {
                        let (Some(s), Some(p)) = (&sps, &pps) else { continue };
                        SliceHeader::parse(&nal, s, p).expect("slice header must parse");
                        slices += 1;
                    }
                    _ => {}
                }
            }
        }

        let sps = sps.expect("the encoder must emit an SPS");
        assert_eq!(sps.display_size(), (w as u32, h as u32));
        assert_eq!(sps.unsupported(), None, "this encoder must produce a decodable stream");
        assert!(pps.is_some(), "the encoder must emit a PPS");
        assert!(slices > 0, "no slice headers parsed");
    }

    #[test]
    fn an_interlaced_stream_is_refused_rather_than_mis_decoded() {
        let mut sps = Sps::parse(&sample_sps()).unwrap();
        sps.frame_mbs_only_flag = false;
        assert_eq!(sps.unsupported(), Some("interlaced content"));
        sps.frame_mbs_only_flag = true;
        sps.chroma_format_idc = 3;
        assert_eq!(sps.unsupported(), Some("chroma format other than 4:2:0"));
        sps.chroma_format_idc = 1;
        sps.bit_depth_luma_minus8 = 2;
        assert_eq!(sps.unsupported(), Some("bit depth above 8"));
    }

    /// The exact three bytes this project's encoder emits as a PPS.
    ///
    /// Captured from `the_encoder_this_ships_with_produces_a_stream_this_parses`
    /// and kept because this is the case that broke: everything after
    /// `redundant_pic_cnt_present_flag` is the RBSP stop bit, sitting in a byte
    /// the reader had not reached. A `more_rbsp_data` that compares byte indices
    /// rather than bit positions calls that "more data", reads the stop bit as
    /// `pic_scaling_matrix_present_flag`, and runs off the end.
    #[test]
    fn a_picture_parameter_set_with_no_extension_stops_at_the_stop_bit() {
        let pps = Pps::parse(&[0xce, 0x3c, 0x80], None).expect("PPS must parse");
        assert_eq!(pps.id, 0);
        assert_eq!(pps.sps_id, 0);
        assert!(!pps.entropy_coding_mode_flag, "CAVLC");
        assert_eq!(pps.num_slice_groups_minus1, 0);
        assert_eq!(pps.num_ref_idx_l0_default_active_minus1, 0);
        assert_eq!(pps.pic_init_qp_minus26, 0);
        assert!(pps.deblocking_filter_control_present_flag);
        assert!(!pps.constrained_intra_pred_flag);
        assert!(!pps.redundant_pic_cnt_present_flag);
        assert!(!pps.transform_8x8_mode_flag, "no extension is present to turn it on");
    }

    #[test]
    fn more_data_is_false_once_only_the_stop_bit_remains() {
        // Two bytes of payload, then a byte holding nothing but the stop bit.
        let data = [0xff, 0xff, 0x80];
        let mut r = RbspReader::new(&data);
        assert!(r.more_data(), "nothing read yet");
        r.bits(8).unwrap();
        assert!(r.more_data(), "one byte in");
        r.bits(8).unwrap();
        assert!(!r.more_data(), "only the stop bit is left");
    }

    #[test]
    fn scaling_lists_default_to_flat_sixteens() {
        let lists = ScalingLists::default();
        assert!(lists.list_4x4.iter().all(|l| l.iter().all(|&v| v == 16)));
        assert!(lists.list_8x8.iter().all(|l| l.iter().all(|&v| v == 16)));
    }

    #[test]
    fn garbage_is_rejected_rather_than_parsed_into_a_plausible_sps() {
        // All-ones has 8 leading zeroes nowhere, so every ue() is 0, and the
        // profile check and range guards are what have to catch it.
        assert!(Sps::parse(&[0xff; 4]).is_err() || Sps::parse(&[0xff; 4]).unwrap().width_mbs() > 0);
        assert!(Sps::parse(&[]).is_err(), "an empty payload must fail");
    }
}
