//! Stereo pairs stored in one file, read and written as the specifications that define them say.
//!
//! - **HEIF**: a stereo pair entity group in the `grpl` box (`ster`, ISO/IEC 23008-12:2025 §6.8.5,
//!   or `stem`, Amd. 1:2025 §6.8.11, which adds a monoscopic image) names the left and the right
//!   eye's image items, in that order ([`lightcraft_meta::stereo_pair`]). The primary image stays
//!   the file's 2D picture.
//! - **MPO**: CIPA DC-007-2009 (Multi-Picture Format). Individual Images of the MP Type Disparity
//!   (020002.H, §5.2.3.3.1 Table 4) are views of one scene; their MP Individual Image Numbers count
//!   the viewpoints from the leftmost one (§6.2.2.2), so viewpoint 1 is the left eye.
//! - **JPS**: the VRex "General Purpose Stereoscopic Data Descriptor" (1997), an APP3 segment
//!   `_JPSJPS_` with a 32-bit descriptor written most significant byte first: media type (bits
//!   0–7, 1 = stereo), layout (bits 8–15: 1 interleaved, 2 side by side, 3 over/under, 4 anaglyph),
//!   flags (bit 16 half height, bit 17 half width, bit 18 left field first, or for an anaglyph the
//!   red field's eye) and the separation between the fields in pixels (bits 24–31). A `.jps` file
//!   without the descriptor is side by side, full height, right field first (the specification's
//!   "Assumptions"). A monoscopic descriptor (media type 0) is one picture, not a pair.
//! - **PNG**: the `sTER` chunk (Extensions to the PNG 1.2 Specification, version 1.4.0, §3.6),
//!   before the first `IDAT`: mode 0 is cross-fuse (right eye on the left), mode 1 diverging-fuse
//!   (left eye on the left); the right subimage starts on a column divisible by eight, with up to
//!   seven padding columns between the two.
//!
//! [`layout`] says where a file keeps its two eyes, [`decode_pair`] decodes them (an anaglyph's
//! eyes are rebuilt from its colours, as far as they can be: each eye keeps only what its filter
//! passed), and
//! [`encode_mpo`], [`encode_jps`] and [`encode_png`] write a pair given as one full side-by-side
//! picture (left eye in the left half).

use std::ops::Range;

use lightcraft_raster::resample::{Filter, fit, resize};
use lightcraft_raster::{Image, Plane};
use serde::{Deserialize, Serialize};

use crate::encode::{ChromaSubsampling, EncodeImage, EncodeMeta, Samples};
use crate::{DecodeOptions, Decoded, Error, Format, Result, exif, jpeg, sniff};

/// MP Type Code of a Disparity image (CIPA DC-007-2009 Table 4): one view of a stereo scene.
const MP_TYPE_DISPARITY: u32 = 0x02_0002;
/// Representative Image Flag of an Individual Image Attribute (§5.2.3.3.1, Figure 8).
const MP_REPRESENTATIVE: u32 = 0x2000_0000;
/// MP Entry and MP Attribute IFD tags (§5.2.3, §5.2.4 Table 5).
const MPF_VERSION: u16 = 0xB000;
const MPF_NUMBER_OF_IMAGES: u16 = 0xB001;
const MPF_ENTRY: u16 = 0xB002;
const MPF_INDIVIDUAL_NUM: u16 = 0xB101;
const MPF_BASE_VIEWPOINT: u16 = 0xB204;
/// JPS descriptor fields (VRex, "Media Type – SD_MTYPE_STEREOSCOPIC_IMAGE").
const JPS_STEREO: u32 = 0x01;
const JPS_INTERLEAVED: u32 = 0x01;
const JPS_SIDE_BY_SIDE: u32 = 0x02;
const JPS_OVER_UNDER: u32 = 0x03;
const JPS_ANAGLYPH: u32 = 0x04;
const JPS_HALF_HEIGHT: u32 = 0x01;
const JPS_HALF_WIDTH: u32 = 0x02;
const JPS_LEFT_FIRST: u32 = 0x04;
/// PNG `sTER` modes.
const STER_CROSS: u8 = 0;
const STER_DIVERGING: u8 = 1;

/// An eye: the one a packed pair stores first (in the left half, the top half or the first row),
/// or the one an anaglyph's red channel is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Eye {
    Left,
    Right,
}

/// Where a stereo file keeps its two eyes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StereoLayout {
    /// The image items of a HEIF stereo pair entity group: the left and the right eye's item ids.
    Heif { left: u32, right: u32 },
    /// Two Individual Images of an MPO: the byte ranges of the left and the right eye's JPEG.
    Mpo { left: Range<usize>, right: Range<usize> },
    /// One picture with both eyes side by side, `gap` columns apart. `squeezed`: each eye is stored
    /// at half its width (JPS "half width").
    SideBySide { first: Eye, gap: u32, squeezed: bool },
    /// One picture with both eyes one above the other, `gap` rows apart. `squeezed`: each eye is
    /// stored at half its height (JPS "half height").
    OverUnder { first: Eye, gap: u32, squeezed: bool },
    /// One picture whose rows alternate between the eyes (JPS "interleaved"), starting with `first`.
    Rows { first: Eye },
    /// One picture of both eyes in complementary colours (JPS "anaglyph"): `red` is the eye of the
    /// red channel, green and blue are the other's.
    Anaglyph { red: Eye },
}

/// The stereo layout `bytes` declare, or `None` for an ordinary (or undecidable) file: a HEIF with
/// a stereo pair entity group, an MPO with two Disparity images, a JPEG with a stereo JPS
/// descriptor, or a PNG with `sTER`. `jps` says the file is named `.jps`, which makes a JPEG
/// without a descriptor a cross-eyed side-by-side pair. Only headers and boxes are read.
pub fn layout(bytes: &[u8], jps: bool) -> Option<StereoLayout> {
    match sniff(bytes)? {
        Format::Heif | Format::Avif => lightcraft_meta::stereo_pair(bytes).map(|p| StereoLayout::Heif { left: p.left, right: p.right }),
        Format::Jpeg => {
            let m = jpeg::parse_markers(bytes)?;
            if let Some(l) = mpo_layout(bytes, &m) {
                return Some(l);
            }
            match m.jps {
                Some(d) => jps_layout(d),
                None if jps => Some(StereoLayout::SideBySide { first: Eye::Right, gap: 0, squeezed: false }),
                None => None,
            }
        }
        Format::Png => png_layout(bytes),
        _ => None,
    }
}

/// Byte-order aware reads of an MP Entry (CIPA DC-007 §5.2.3.3, Figure 7).
fn long(le: bool, r: &[u8; 16], o: usize) -> u32 {
    let a = [r[o], r[o + 1], r[o + 2], r[o + 3]];
    if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
}

/// The MP Individual Image Number in the MP Attribute IFD at `pos`, if there is one.
fn individual_number(t: &exif::Tiff, pos: usize) -> Option<u32> {
    if pos == 0 {
        return None;
    }
    let (entries, _) = t.ifd(pos)?;
    let e = entries.iter().find(|e| e.tag == MPF_INDIVIDUAL_NUM)?;
    t.uint(e)
}

/// The left and right eye of an MPO: the first two Disparity images in viewpoint order. A number
/// of 0 marks a composite of several views and FFFFFFFF.H an unknown position (§6.2.2.2):
/// composites are not eyes, and images of unknown (or missing) number keep their order in the MP
/// Index, after the numbered ones.
fn mpo_layout(bytes: &[u8], m: &jpeg::Markers) -> Option<StereoLayout> {
    let (base, data) = m.mpf.as_ref()?;
    let t = exif::Tiff::new(data)?;
    let index = t.first_ifd()?;
    let (entries, attributes) = t.ifd(index)?;
    let e = entries.iter().find(|e| e.tag == MPF_ENTRY)?;
    let list = t.bytes(e)?;
    let le = data.starts_with(b"II");
    let mut views: Vec<(u32, usize, Range<usize>)> = Vec::new();
    for (i, rec) in list.as_chunks::<16>().0.iter().enumerate().take(16) {
        if long(le, rec, 0) & 0xFF_FFFF != MP_TYPE_DISPARITY {
            continue;
        }
        let size = long(le, rec, 4) as usize;
        // the First Individual Image's offset is 0; the others count from the MP header (§5.2.3.3.3)
        let start = if i == 0 { 0 } else { base.checked_add(long(le, rec, 8) as usize)? };
        let range = start..start.checked_add(size)?;
        let Some(image) = bytes.get(range.clone()).filter(|b| b.starts_with(&[0xFF, 0xD8])) else { continue };
        let number = if i == 0 {
            individual_number(&t, attributes)
        } else {
            jpeg::parse_markers(image).and_then(|mi| {
                let (_, d) = mi.mpf?;
                let ti = exif::Tiff::new(&d)?;
                individual_number(&ti, ti.first_ifd()?)
            })
        };
        match number {
            Some(0) => continue,
            n => views.push((n.unwrap_or(u32::MAX), i, range)),
        }
    }
    views.sort_by_key(|v| (v.0, v.1));
    let mut views = views.into_iter().map(|v| v.2);
    Some(StereoLayout::Mpo { left: views.next()?, right: views.next()? })
}

/// The layout a JPS descriptor declares; `None` for a monoscopic image (one picture, whichever eye
/// it is for) and for values the specification does not define.
fn jps_layout(d: u32) -> Option<StereoLayout> {
    if d & 0xFF != JPS_STEREO {
        return None;
    }
    let flags = (d >> 16) & 0xFF;
    // the field order bit: the left (or top) field is the left eye's, or an anaglyph's red field is
    let first = if flags & JPS_LEFT_FIRST != 0 { Eye::Left } else { Eye::Right };
    let gap = d >> 24;
    match (d >> 8) & 0xFF {
        JPS_SIDE_BY_SIDE => Some(StereoLayout::SideBySide { first, gap, squeezed: flags & JPS_HALF_WIDTH != 0 }),
        JPS_OVER_UNDER => Some(StereoLayout::OverUnder { first, gap, squeezed: flags & JPS_HALF_HEIGHT != 0 }),
        // the separation is undefined for these two: readers ignore it
        JPS_INTERLEAVED => Some(StereoLayout::Rows { first }),
        JPS_ANAGLYPH => Some(StereoLayout::Anaglyph { red: first }),
        _ => None,
    }
}

/// A PNG's `sTER` chunk (before the first `IDAT`) as a side-by-side layout: the padding between
/// the subimages follows from the image width, and a width that needs more than seven padding
/// columns is an error (the chunk is then ignored).
fn png_layout(bytes: &[u8]) -> Option<StereoLayout> {
    let mut width = 0u32;
    let mut p = 8usize;
    while let Some(head) = bytes.get(p..p.checked_add(8)?) {
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let data = bytes.get(p + 8..p.checked_add(8)?.checked_add(len)?)?;
        match &head[4..8] {
            b"IHDR" => width = u32::from_be_bytes([*data.first()?, *data.get(1)?, *data.get(2)?, *data.get(3)?]),
            b"sTER" => {
                let first = match *data.first()? {
                    STER_CROSS => Eye::Right,
                    STER_DIVERGING => Eye::Left,
                    _ => return None,
                };
                // padding := 15 - ((W - 1) mod 16); more than 7 is an error
                let gap = 15 - (width.checked_sub(1)? % 16);
                return (gap <= 7 && width >= gap + 2).then_some(StereoLayout::SideBySide { first, gap, squeezed: false });
            }
            b"IDAT" | b"IEND" => return None,
            _ => {}
        }
        p = p.checked_add(12)?.checked_add(len)?;
    }
    None
}

/// Decode both eyes of a stereo file at the same size, each fitted within `opts.max_size`. Packed
/// pairs are decoded at full size and split in source pixels; squeezed and interleaved eyes are
/// stretched back to their full width or height. Each eye keeps the file's orientation tag (not
/// applied, as for [`crate::decode`]); HEIF eyes come with their container transforms applied.
pub fn decode_pair(bytes: &[u8], layout: &StereoLayout, opts: DecodeOptions) -> Result<(Decoded, Decoded)> {
    let (left, right) = match layout {
        StereoLayout::Heif { left, right } => (crate::heif::decode_item(bytes, *left, &opts)?, crate::heif::decode_item(bytes, *right, &opts)?),
        StereoLayout::Mpo { left, right } => {
            let get = |r: &Range<usize>| bytes.get(r.clone()).ok_or_else(|| Error::Malformed(Format::Jpeg, "MPO image outside the file".into()));
            (crate::decode(get(left)?, opts)?, crate::decode(get(right)?, opts)?)
        }
        _ => {
            let whole = crate::decode(bytes, DecodeOptions { max_size: None, ..opts })?;
            split(&whole, layout)?
        }
    };
    // views of one scene share one frame: a second view of another size is resampled to the first
    let right =
        if (right.width, right.height) == (left.width, left.height) { right } else { resized(&right, left.width as usize, left.height as usize) };
    Ok(match opts.max_size {
        Some((w, h)) if w > 0 && h > 0 && (left.width > w || left.height > h) => {
            let left = fitted(&left, w as usize, h as usize);
            let right = resized(&right, left.width as usize, left.height as usize);
            (left, right)
        }
        _ => (left, right),
    })
}

/// The eyes of a packed picture, `first` and the other in that order of storage, as (left, right).
fn split(d: &Decoded, layout: &StereoLayout) -> Result<(Decoded, Decoded)> {
    if let StereoLayout::Anaglyph { red } = *layout {
        return Ok(anaglyph_eyes(d, red));
    }
    let (w, h) = (d.width as usize, d.height as usize);
    let bad = || Error::Malformed(d.format, "stereo picture too small for its layout".into());
    let (a, b, first) = match *layout {
        StereoLayout::SideBySide { first, gap, squeezed } => {
            let eye = w.checked_sub(gap as usize).ok_or_else(bad)? / 2;
            if eye == 0 {
                return Err(bad());
            }
            let (a, b) = (crop(d, 0, 0, eye, h), crop(d, eye + gap as usize, 0, eye, h));
            if squeezed { (resized(&a, eye * 2, h), resized(&b, eye * 2, h), first) } else { (a, b, first) }
        }
        StereoLayout::OverUnder { first, gap, squeezed } => {
            let eye = h.checked_sub(gap as usize).ok_or_else(bad)? / 2;
            if eye == 0 {
                return Err(bad());
            }
            let (a, b) = (crop(d, 0, 0, w, eye), crop(d, 0, eye + gap as usize, w, eye));
            if squeezed { (resized(&a, w, eye * 2), resized(&b, w, eye * 2), first) } else { (a, b, first) }
        }
        StereoLayout::Rows { first } => {
            if h < 2 {
                return Err(bad());
            }
            let eye = h / 2;
            let (a, b) = (rows(d, 0, eye), rows(d, 1, eye));
            (resized(&a, w, eye * 2), resized(&b, w, eye * 2), first)
        }
        StereoLayout::Heif { .. } | StereoLayout::Mpo { .. } | StereoLayout::Anaglyph { .. } => {
            return Err(Error::Malformed(d.format, "the eyes are not one packed picture".into()));
        }
    };
    Ok(match first {
        Eye::Left => (a, b),
        Eye::Right => (b, a),
    })
}

/// The two eyes of a red/cyan anaglyph (left, right): the red channel is one eye's, green and blue
/// are the other's. What a filter took out is gone, so each eye fills its missing channels from
/// the ones it has: the red eye is (R, (R+G)/2, (R+B)/2), the cyan eye ((R+G)/2, G, B).
fn anaglyph_eyes(d: &Decoded, red: Eye) -> (Decoded, Decoded) {
    let mix =
        |f: fn([f32; 3]) -> [f32; 3]| Image { width: d.image.width, height: d.image.height, data: d.image.data.iter().map(|&p| f(p)).collect() };
    let red_eye = mix(|[r, g, b]| [r, (r + g) / 2.0, (r + b) / 2.0]);
    let cyan_eye = mix(|[r, g, b]| [(r + g) / 2.0, g, b]);
    let source = (d.source_width, d.source_height);
    let (red_eye, cyan_eye) = (with_pixels(d, red_eye, d.alpha.clone(), source), with_pixels(d, cyan_eye, d.alpha.clone(), source));
    match red {
        Eye::Left => (red_eye, cyan_eye),
        Eye::Right => (cyan_eye, red_eye),
    }
}

/// `d` with its pixels (and alpha) replaced by `image`/`alpha`, sized accordingly.
fn with_pixels(d: &Decoded, image: lightcraft_raster::Rgb32f, alpha: Option<Plane>, source: (u32, u32)) -> Decoded {
    Decoded {
        format: d.format,
        width: image.width as u32,
        height: image.height as u32,
        image,
        alpha,
        space: d.space.clone(),
        bit_depth: d.bit_depth,
        float: d.float,
        has_alpha: d.has_alpha,
        grayscale: d.grayscale,
        cmyk: d.cmyk,
        orientation: d.orientation,
        exif: d.exif.clone(),
        xmp: d.xmp.clone(),
        icc: d.icc.clone(),
        source_width: source.0,
        source_height: source.1,
    }
}

fn crop(d: &Decoded, x: usize, y: usize, w: usize, h: usize) -> Decoded {
    let image = d.image.crop(x, y, w, h);
    let alpha = d.alpha.as_ref().map(|a| a.crop(x, y, w, h));
    let source = (image.width as u32, image.height as u32);
    with_pixels(d, image, alpha, source)
}

/// Every other row of `d`, starting at `start`: `n` rows.
fn rows(d: &Decoded, start: usize, n: usize) -> Decoded {
    let pick = |img_w: usize, src: &[[f32; 3]]| -> Vec<[f32; 3]> {
        (0..n).flat_map(|i| src.get((start + 2 * i) * img_w..(start + 2 * i + 1) * img_w).unwrap_or(&[]).iter().copied()).collect()
    };
    let w = d.width as usize;
    let image = Image { width: w, height: n, data: pick(w, &d.image.data) };
    let alpha = d.alpha.as_ref().map(|a| Image {
        width: w,
        height: n,
        data: (0..n).flat_map(|i| a.data.get((start + 2 * i) * w..(start + 2 * i + 1) * w).unwrap_or(&[]).iter().copied()).collect(),
    });
    with_pixels(d, image, alpha, (w as u32, n as u32))
}

fn resized(d: &Decoded, w: usize, h: usize) -> Decoded {
    let image = resize(&d.image, w, h, Filter::Mitchell);
    let alpha = d.alpha.as_ref().map(|a| resize(a, w, h, Filter::Mitchell));
    with_pixels(d, image, alpha, (d.source_width, d.source_height))
}

fn fitted(d: &Decoded, w: usize, h: usize) -> Decoded {
    let image = fit(&d.image, w, h, Filter::Mitchell);
    let alpha = d.alpha.as_ref().map(|a| resize(a, image.width, image.height, Filter::Mitchell));
    with_pixels(d, image, alpha, (d.source_width, d.source_height))
}

// ---------------------------------------------------------------------------------------------
// Writing

/// Owned interleaved samples of one picture made from parts of an [`EncodeImage`].
enum Owned {
    U8(Vec<u8>),
    U16(Vec<u16>),
    F32(Vec<f32>),
}

impl Owned {
    fn image(&self, width: u32, height: u32, channels: u8) -> EncodeImage<'_> {
        let samples = match self {
            Owned::U8(v) => Samples::U8(v),
            Owned::U16(v) => Samples::U16(v),
            Owned::F32(v) => Samples::F32(v),
        };
        EncodeImage::new(width, height, channels, samples)
    }
}

/// A new picture made of column ranges `(x0, width)` of `img`, side by side in that order with
/// `gap` zero columns between them.
fn pack(img: &EncodeImage, parts: &[(u32, u32)], gap: u32) -> Result<(Owned, u32)> {
    let ch = img.channels as usize;
    let (w, h) = (img.width as usize, img.height as usize);
    let out_w: usize = parts.iter().map(|p| p.1 as usize).sum::<usize>() + gap as usize * parts.len().saturating_sub(1);
    fn run<T: Copy + Default>(src: &[T], w: usize, h: usize, ch: usize, parts: &[(u32, u32)], gap: usize, out_w: usize) -> Option<Vec<T>> {
        let mut out = vec![T::default(); out_w.checked_mul(h)?.checked_mul(ch)?];
        for y in 0..h {
            let mut x_out = 0usize;
            for (i, &(x0, pw)) in parts.iter().enumerate() {
                if i > 0 {
                    x_out += gap;
                }
                let s = (y * w + x0 as usize) * ch;
                let n = pw as usize * ch;
                let d = (y * out_w + x_out) * ch;
                out.get_mut(d..d + n)?.copy_from_slice(src.get(s..s + n)?);
                x_out += pw as usize;
            }
        }
        Some(out)
    }
    let short = || Error::Encode("sample buffer too short for the stereo pair".into());
    let owned = match img.samples {
        Samples::U8(s) => Owned::U8(run(s, w, h, ch, parts, gap as usize, out_w).ok_or_else(short)?),
        Samples::U16(s) => Owned::U16(run(s, w, h, ch, parts, gap as usize, out_w).ok_or_else(short)?),
        Samples::F32(s) => Owned::F32(run(s, w, h, ch, parts, gap as usize, out_w).ok_or_else(short)?),
    };
    Ok((owned, out_w as u32))
}

/// The eye width of a full side-by-side picture (both halves the same width).
fn eye_width(sbs: &EncodeImage) -> Result<u32> {
    if sbs.width < 2 || !sbs.width.is_multiple_of(2) {
        return Err(Error::Encode(format!("a side-by-side pair needs an even width, got {}", sbs.width)));
    }
    Ok(sbs.width / 2)
}

/// Splice `segment` into a JPEG where CIPA DC-007 §4.1 puts the MP Extensions, right after the
/// Exif APP1 segment (else after a leading JFIF APP0, else after SOI). Returns the new JPEG and
/// the segment's offset.
fn splice_after_exif(jpeg: &[u8], segment: &[u8]) -> Result<(Vec<u8>, usize)> {
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return Err(Error::Encode("not a JPEG".into()));
    }
    let mut at = 2usize;
    let mut p = 2usize;
    while let Some(s) = jpeg.get(p..p + 10) {
        if s[0] != 0xFF || !matches!(s[1], 0xE0 | 0xE1) {
            break;
        }
        p += 2 + u16::from_be_bytes([s[2], s[3]]) as usize;
        if s[1] == 0xE0 || &s[4..10] == b"Exif\0\0" {
            at = p;
        }
        if s[1] == 0xE1 && &s[4..10] == b"Exif\0\0" {
            break;
        }
    }
    let at = at.min(jpeg.len());
    let mut out = Vec::with_capacity(jpeg.len() + segment.len());
    out.extend_from_slice(&jpeg[..at]);
    out.extend_from_slice(segment);
    out.extend_from_slice(&jpeg[at..]);
    Ok((out, at))
}

/// A little-endian IFD writer for the MP Extensions (tags in ascending order; values of more
/// than four bytes go after the IFD).
struct Ifd {
    entries: Vec<(u16, u16, u32, Vec<u8>)>,
}

impl Ifd {
    fn new() -> Self {
        Ifd { entries: Vec::new() }
    }
    fn add(&mut self, tag: u16, typ: u16, count: u32, value: Vec<u8>) -> &mut Self {
        self.entries.push((tag, typ, count, value));
        self
    }
    fn long(&mut self, tag: u16, v: u32) -> &mut Self {
        self.add(tag, 4, 1, v.to_le_bytes().to_vec())
    }
    /// The IFD at offset `at` (from the MP header), its next-IFD field set to `next`.
    fn bytes(&self, at: usize, next: u32) -> Vec<u8> {
        let head = 2 + self.entries.len() * 12 + 4;
        let mut extra: Vec<u8> = Vec::new();
        let mut out = (self.entries.len() as u16).to_le_bytes().to_vec();
        for (tag, typ, count, value) in &self.entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&typ.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            if value.len() <= 4 {
                let mut v = value.clone();
                v.resize(4, 0);
                out.extend_from_slice(&v);
            } else {
                out.extend_from_slice(&((at + head + extra.len()) as u32).to_le_bytes());
                extra.extend_from_slice(value);
            }
        }
        out.extend_from_slice(&next.to_le_bytes());
        out.extend_from_slice(&extra);
        out
    }
}

/// MP Attribute IFD of a Disparity image (§5.2.4, Table 8): MP Format Version, MP Individual
/// Image Number (`number`, counted from the leftmost viewpoint, §6.2.2.2) and Base Viewpoint
/// Number 1. Convergence Angle and Baseline Length are only recommended and unknown here, so they
/// are left out.
fn attribute_ifd(number: u32) -> Ifd {
    let mut ifd = Ifd::new();
    ifd.add(MPF_VERSION, 7, 4, b"0100".to_vec()).long(MPF_INDIVIDUAL_NUM, number).long(MPF_BASE_VIEWPOINT, 1);
    ifd
}

/// An APP2 `MPF` segment around an MP header ("II", offset to first IFD 8) and `ifds`.
fn mpf_segment(body: &[u8]) -> Vec<u8> {
    let mut seg = vec![0xFF, 0xE2];
    seg.extend_from_slice(&((2 + 4 + 8 + body.len()) as u16).to_be_bytes());
    seg.extend_from_slice(b"MPF\0II*\0");
    seg.extend_from_slice(&8u32.to_le_bytes());
    seg.extend_from_slice(body);
    seg
}

/// Encode a full side-by-side pair (`sbs`, left eye in the left half) as an MPO (CIPA DC-007): two
/// JPEG Individual Images of the MP Type Disparity, the left eye first (Representative Image,
/// viewpoint 1, with the MP Index IFD) and the right eye second (viewpoint 2). `meta` goes into both.
pub fn encode_mpo(sbs: &EncodeImage, quality: u8, subsampling: ChromaSubsampling, meta: &EncodeMeta) -> Result<Vec<u8>> {
    let eye = eye_width(sbs)?;
    let (l, _) = pack(sbs, &[(0, eye)], 0)?;
    let (r, _) = pack(sbs, &[(eye, eye)], 0)?;
    let left = crate::encode_jpeg(&l.image(eye, sbs.height, sbs.channels), quality, subsampling, meta)?;
    let right = crate::encode_jpeg(&r.image(eye, sbs.height, sbs.channels), quality, subsampling, meta)?;
    // the second image: MP header + MP Attribute IFD (Figure 3)
    let (right, _) = splice_after_exif(&right, &mpf_segment(&attribute_ifd(2).bytes(8, 0)))?;
    // the first image: MP header + MP Index IFD + MP Attribute IFD (Figure 2). Its size is fixed
    // before the offsets are known, so lay it out once with zero sizes and once more for real.
    let first = |sizes: (u32, u32, u32)| -> Vec<u8> {
        let mut entries = Vec::with_capacity(32);
        for (attr, size, offset) in [(MP_REPRESENTATIVE | MP_TYPE_DISPARITY, sizes.0, 0), (MP_TYPE_DISPARITY, sizes.1, sizes.2)] {
            entries.extend_from_slice(&attr.to_le_bytes());
            entries.extend_from_slice(&size.to_le_bytes());
            entries.extend_from_slice(&offset.to_le_bytes());
            entries.extend_from_slice(&[0, 0, 0, 0]);
        }
        let mut index = Ifd::new();
        index.add(MPF_VERSION, 7, 4, b"0100".to_vec()).long(MPF_NUMBER_OF_IMAGES, 2).add(MPF_ENTRY, 7, 32, entries);
        let index_len = index.bytes(8, 0).len();
        let mut body = index.bytes(8, (8 + index_len) as u32);
        body.extend_from_slice(&attribute_ifd(1).bytes(8 + index_len, 0));
        mpf_segment(&body)
    };
    let (probe, at) = splice_after_exif(&left, &first((0, 0, 0)))?;
    let total = probe.len();
    // offsets count from the MP Endian field: segment marker (2) + length (2) + "MPF\0" (4)
    let header = at + 8;
    let offset = u32::try_from(total - header).map_err(|_| Error::Encode("MPO too large".into()))?;
    let size = |n: usize| u32::try_from(n).map_err(|_| Error::Encode("MPO too large".into()));
    let (mut out, _) = splice_after_exif(&left, &first((size(total)?, size(right.len())?, offset)))?;
    out.extend_from_slice(&right);
    Ok(out)
}

/// Encode a full side-by-side pair (`sbs`, left eye in the left half) as a JPS: one JPEG holding
/// the right eye on the left and the left eye on the right, the cross-eyed arrangement readers
/// assume without a descriptor, and an APP3 descriptor saying so (stereo, side by side, full
/// height and width, right field first, separation 0).
pub fn encode_jps(sbs: &EncodeImage, quality: u8, subsampling: ChromaSubsampling, meta: &EncodeMeta) -> Result<Vec<u8>> {
    let eye = eye_width(sbs)?;
    let (cross, w) = pack(sbs, &[(eye, eye), (0, eye)], 0)?;
    let jpeg = crate::encode_jpeg(&cross.image(w, sbs.height, sbs.channels), quality, subsampling, meta)?;
    let descriptor = JPS_STEREO | JPS_SIDE_BY_SIDE << 8;
    let mut seg = vec![0xFF, 0xE3];
    let body_len = 8 + 2 + 4 + 2;
    seg.extend_from_slice(&((2 + body_len) as u16).to_be_bytes());
    seg.extend_from_slice(b"_JPSJPS_");
    seg.extend_from_slice(&4u16.to_be_bytes());
    seg.extend_from_slice(&descriptor.to_be_bytes());
    seg.extend_from_slice(&0u16.to_be_bytes()); // no comment
    Ok(splice_after_exif(&jpeg, &seg)?.0)
}

/// Encode a full side-by-side pair (`sbs`, left eye in the left half) as a PNG with `sTER` mode 1
/// (diverging-fuse, left eye on the left). When the eye width is not a multiple of eight, the
/// padding columns the chunk requires (zero samples) separate the two subimages.
pub fn encode_png(sbs: &EncodeImage, meta: &EncodeMeta) -> Result<Vec<u8>> {
    let eye = eye_width(sbs)?;
    // padding := 7 - ((subimage_width - 1) mod 8)
    let gap = 7 - ((eye - 1) % 8);
    let (packed, w) = pack(sbs, &[(0, eye), (eye, eye)], gap)?;
    let png = crate::encode_png(&packed.image(w, sbs.height, sbs.channels), meta)?;
    // sTER goes before the first IDAT
    let mut p = 8usize;
    while let Some(head) = png.get(p..p + 8) {
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if &head[4..8] == b"IDAT" {
            let mut out = Vec::with_capacity(png.len() + 13);
            out.extend_from_slice(&png[..p]);
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(b"sTER");
            out.push(STER_DIVERGING);
            out.extend_from_slice(&crc32fast::hash(&[b's', b'T', b'E', b'R', STER_DIVERGING]).to_be_bytes());
            out.extend_from_slice(&png[p..]);
            return Ok(out);
        }
        p += 12 + len;
    }
    Err(Error::Encode("PNG without IDAT".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jps_descriptor_values() {
        assert_eq!(jps_layout(0x0000_0201), Some(StereoLayout::SideBySide { first: Eye::Right, gap: 0, squeezed: false }));
        assert_eq!(jps_layout(0x0004_0201), Some(StereoLayout::SideBySide { first: Eye::Left, gap: 0, squeezed: false }));
        assert_eq!(jps_layout(0x0602_0201), Some(StereoLayout::SideBySide { first: Eye::Right, gap: 6, squeezed: true }));
        assert_eq!(jps_layout(0x0005_0301), Some(StereoLayout::OverUnder { first: Eye::Left, gap: 0, squeezed: true }));
        assert_eq!(jps_layout(0x0000_0101), Some(StereoLayout::Rows { first: Eye::Right }));
        // the separation of an interleaved picture is undefined: ignored
        assert_eq!(jps_layout(0x0300_0101), Some(StereoLayout::Rows { first: Eye::Right }));
        // anaglyph: the field order bit names the red field's eye
        assert_eq!(jps_layout(0x0000_0401), Some(StereoLayout::Anaglyph { red: Eye::Right }));
        assert_eq!(jps_layout(0x0004_0401), Some(StereoLayout::Anaglyph { red: Eye::Left }));
        // monoscopic (any eye it is for) and undefined layouts
        assert_eq!(jps_layout(0x0000_0000), None);
        assert_eq!(jps_layout(0x0000_0100), None);
        assert_eq!(jps_layout(0x0000_0901), None);
    }
}
