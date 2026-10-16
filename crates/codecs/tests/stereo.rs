//! Stereo files: where the eyes are (HEIF stereo pairs, MPO, JPS, PNG `sTER`), decoding both, and
//! writing them back. Fixtures are built here byte by byte from the specifications (CIPA
//! DC-007-2009, the VRex stereoscopic descriptor, PNG Extensions 1.4.0 §3.6) or by the HEIF test
//! builder (ISO/IEC 23008-12 `ster` group); each eye has its own colour (left red, right blue) so
//! a swapped pair shows.

use lightcraft_codecs::stereo::{self, Eye, StereoLayout};
use lightcraft_codecs::*;
use lightcraft_raster::Rgba8;

const RED: [u8; 4] = [200, 30, 30, 255];
const BLUE: [u8; 4] = [30, 30, 200, 255];
const GREEN: [u8; 4] = [30, 200, 30, 255];

fn jpeg(img: &Rgba8) -> Vec<u8> {
    encode_jpeg(&EncodeImage::rgba8(img), 92, ChromaSubsampling::S444, &EncodeMeta::default()).unwrap()
}

fn png(img: &Rgba8) -> Vec<u8> {
    encode_png(&EncodeImage::rgba8(img), &EncodeMeta::default()).unwrap()
}

/// Two pictures side by side, `gap` black columns between them.
fn side_by_side(a: &Rgba8, b: &Rgba8, gap: usize) -> Rgba8 {
    Rgba8::from_fn(a.width + gap + b.width, a.height, |x, y| {
        if x < a.width {
            a.get(x, y)
        } else if x < a.width + gap {
            [0, 0, 0, 255]
        } else {
            b.get(x - a.width - gap, y)
        }
    })
}

/// Mean display colour of a decoded image (sRGB 8-bit).
fn mean(d: &Decoded) -> [u32; 3] {
    let s = d.to_srgb8();
    let n = s.data.len().max(1) as u64;
    let sum = s.data.iter().fold([0u64; 3], |a, p| [a[0] + p[0] as u64, a[1] + p[1] as u64, a[2] + p[2] as u64]);
    [(sum[0] / n) as u32, (sum[1] / n) as u32, (sum[2] / n) as u32]
}

fn is_red(d: &Decoded) -> bool {
    let m = mean(d);
    m[0] > 150 && m[2] < 80
}

fn is_blue(d: &Decoded) -> bool {
    let m = mean(d);
    m[2] > 150 && m[0] < 80
}

// --- MPO (CIPA DC-007-2009) ---------------------------------------------------------------

/// An IFD (little endian) at `at`: entries (tag, type, count, value bytes); values over 4 bytes
/// follow the IFD.
fn ifd(at: usize, entries: &[(u16, u16, u32, Vec<u8>)], next: u32) -> Vec<u8> {
    let head = 2 + entries.len() * 12 + 4;
    let mut out = (entries.len() as u16).to_le_bytes().to_vec();
    let mut extra = Vec::new();
    for (tag, typ, count, v) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        if v.len() <= 4 {
            let mut v = v.clone();
            v.resize(4, 0);
            out.extend_from_slice(&v);
        } else {
            out.extend_from_slice(&((at + head + extra.len()) as u32).to_le_bytes());
            extra.extend_from_slice(v);
        }
    }
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&extra);
    out
}

/// MP Attribute IFD with an MP Individual Image Number (viewpoint) when given.
fn attributes(at: usize, number: Option<u32>) -> Vec<u8> {
    let mut e = vec![(0xB000u16, 7u16, 4u32, b"0100".to_vec())];
    if let Some(n) = number {
        e.push((0xB101, 4, 1, n.to_le_bytes().to_vec()));
        e.push((0xB204, 4, 1, 1u32.to_le_bytes().to_vec()));
    }
    ifd(at, &e, 0)
}

fn app2(body: &[u8]) -> Vec<u8> {
    let mut s = vec![0xFF, 0xE2];
    s.extend_from_slice(&((2 + 4 + 8 + body.len()) as u16).to_be_bytes());
    s.extend_from_slice(b"MPF\0II*\0");
    s.extend_from_slice(&8u32.to_le_bytes());
    s.extend_from_slice(body);
    s
}

/// Insert `seg` right after SOI.
fn after_soi(jpeg: &[u8], seg: &[u8]) -> Vec<u8> {
    [&jpeg[..2], seg, &jpeg[2..]].concat()
}

/// An MP file from (jpeg, Individual Image Attribute, MP Individual Image Number) per image.
fn mpo(images: &[(&[u8], u32, Option<u32>)]) -> Vec<u8> {
    let n = images.len();
    let first = |sizes: &[u32], offsets: &[u32]| -> Vec<u8> {
        let mut entries = Vec::new();
        for (i, img) in images.iter().enumerate() {
            entries.extend_from_slice(&img.1.to_le_bytes());
            entries.extend_from_slice(&sizes[i].to_le_bytes());
            entries.extend_from_slice(&offsets[i].to_le_bytes());
            entries.extend_from_slice(&[0, 0, 0, 0]);
        }
        let index_entries =
            [(0xB000u16, 7u16, 4u32, b"0100".to_vec()), (0xB001, 4, 1, (n as u32).to_le_bytes().to_vec()), (0xB002, 7, 16 * n as u32, entries)];
        let len = ifd(8, &index_entries, 0).len();
        let mut body = ifd(8, &index_entries, (8 + len) as u32);
        body.extend_from_slice(&attributes(8 + len, images[0].2));
        app2(&body)
    };
    let others: Vec<Vec<u8>> = images[1..].iter().map(|img| after_soi(img.0, &app2(&attributes(8, img.2)))).collect();
    let zeros = vec![0u32; n];
    let first_len = images[0].0.len() + first(&zeros, &zeros).len();
    let header = 2 + 8; // SOI, then the segment's FFE2 + length + "MPF\0"
    let mut sizes = vec![first_len as u32];
    let mut offsets = vec![0u32];
    let mut at = first_len;
    for o in &others {
        sizes.push(o.len() as u32);
        offsets.push((at - header) as u32);
        at += o.len();
    }
    let mut out = after_soi(images[0].0, &first(&sizes, &offsets));
    assert_eq!(out.len(), first_len);
    for o in &others {
        out.extend_from_slice(o);
    }
    out
}

const DISPARITY: u32 = 0x02_0002;
const REPRESENTATIVE: u32 = 0x2000_0000;

#[test]
fn mpo_eyes_follow_the_viewpoint_numbers() {
    let left = jpeg(&Rgba8::filled(64, 48, RED));
    let right = jpeg(&Rgba8::filled(64, 48, BLUE));
    // the usual order: viewpoint 1 (left) first
    let bytes = mpo(&[(&left, REPRESENTATIVE | DISPARITY, Some(1)), (&right, DISPARITY, Some(2))]);
    let layout = stereo::layout(&bytes, false).expect("stereo MPO");
    let (l, r) = stereo::decode_pair(&bytes, &layout, DecodeOptions::default()).unwrap();
    assert!(is_red(&l) && is_blue(&r), "left {:?} right {:?}", mean(&l), mean(&r));
    // §6.2.2.2: numbered from the leftmost viewpoint, whatever the order of the images
    let bytes = mpo(&[(&right, REPRESENTATIVE | DISPARITY, Some(2)), (&left, DISPARITY, Some(1))]);
    let StereoLayout::Mpo { left: lr, right: rr } = stereo::layout(&bytes, false).expect("stereo MPO") else { panic!("not an MPO layout") };
    assert!(lr.start > 0 && rr.start == 0, "the second image is viewpoint 1: {lr:?} {rr:?}");
    let (l, r) = stereo::decode_pair(&bytes, &StereoLayout::Mpo { left: lr, right: rr }, DecodeOptions::default()).unwrap();
    assert!(is_red(&l) && is_blue(&r));
    // no numbers: the order of the MP Index
    let bytes = mpo(&[(&left, REPRESENTATIVE | DISPARITY, None), (&right, DISPARITY, None)]);
    let (l, r) = stereo::decode_pair(&bytes, &stereo::layout(&bytes, false).unwrap(), DecodeOptions::default()).unwrap();
    assert!(is_red(&l) && is_blue(&r));
    // number 0 is a composite of several views, not an eye
    let composite = jpeg(&Rgba8::filled(128, 48, GREEN));
    let bytes = mpo(&[(&composite, REPRESENTATIVE | DISPARITY, Some(0)), (&left, DISPARITY, Some(1)), (&right, DISPARITY, Some(2))]);
    let (l, r) = stereo::decode_pair(&bytes, &stereo::layout(&bytes, false).unwrap(), DecodeOptions::default()).unwrap();
    assert!(is_red(&l) && is_blue(&r), "left {:?} right {:?}", mean(&l), mean(&r));
    let bytes = mpo(&[(&left, REPRESENTATIVE | DISPARITY, Some(1)), (&composite, DISPARITY, Some(0))]);
    assert_eq!(stereo::layout(&bytes, false), None, "one eye and a composite are not a pair");
}

#[test]
fn only_disparity_images_make_a_stereo_mpo() {
    let a = jpeg(&Rgba8::filled(64, 48, RED));
    let b = jpeg(&Rgba8::filled(64, 48, BLUE));
    // Baseline MP file (primary + Large Thumbnail), a Pillow-style MPO (primary + undefined),
    // multi-angle and panorama sets, a single disparity image: not a stereo pair
    for (t1, t2) in [(0x03_0000, 0x01_0001), (0x03_0000, 0x00_0000), (0x02_0003, 0x02_0003), (0x02_0001, 0x02_0001), (DISPARITY, 0x01_0002)] {
        let bytes = mpo(&[(&a, REPRESENTATIVE | t1, Some(1)), (&b, t2, Some(2))]);
        assert_eq!(stereo::layout(&bytes, false), None, "{t1:06X} + {t2:06X}");
    }
    assert_eq!(stereo::layout(&a, false), None, "a plain JPEG");
}

#[test]
fn mpo_eyes_of_different_sizes_share_the_first_eyes_frame() {
    let left = jpeg(&Rgba8::filled(64, 48, RED));
    let right = jpeg(&Rgba8::filled(80, 60, BLUE));
    let bytes = mpo(&[(&left, REPRESENTATIVE | DISPARITY, Some(1)), (&right, DISPARITY, Some(2))]);
    let (l, r) = stereo::decode_pair(&bytes, &stereo::layout(&bytes, false).unwrap(), DecodeOptions::default()).unwrap();
    assert_eq!((l.width, l.height), (64, 48));
    assert_eq!((r.width, r.height), (64, 48));
    assert!(is_blue(&r));
}

#[test]
fn stereo_pair_fits_the_requested_box() {
    let left = jpeg(&Rgba8::filled(640, 480, RED));
    let right = jpeg(&Rgba8::filled(640, 480, BLUE));
    let bytes = mpo(&[(&left, REPRESENTATIVE | DISPARITY, Some(1)), (&right, DISPARITY, Some(2))]);
    let (l, r) = stereo::decode_pair(&bytes, &stereo::layout(&bytes, false).unwrap(), DecodeOptions::fit(100, 100)).unwrap();
    assert!(l.width <= 100 && l.height <= 100, "{}×{}", l.width, l.height);
    assert_eq!((l.width, l.height), (r.width, r.height));
    let packed = jpeg(&side_by_side(&Rgba8::filled(640, 480, BLUE), &Rgba8::filled(640, 480, RED), 0));
    let (l, r) = stereo::decode_pair(&packed, &stereo::layout(&packed, true).unwrap(), DecodeOptions::fit(100, 100)).unwrap();
    assert!(l.width <= 100 && l.height <= 100, "{}×{}", l.width, l.height);
    assert_eq!((l.width, l.height), (r.width, r.height));
    assert!(is_red(&l) && is_blue(&r));
}

// --- JPS (VRex stereoscopic descriptor) --------------------------------------------------------

/// A JPS: `picture` with an APP3 `_JPSJPS_` descriptor (most significant byte first).
fn with_descriptor(picture: &[u8], descriptor: u32) -> Vec<u8> {
    let mut seg = vec![0xFF, 0xE3, 0, 18];
    seg.extend_from_slice(b"_JPSJPS_");
    seg.extend_from_slice(&4u16.to_be_bytes());
    seg.extend_from_slice(&descriptor.to_be_bytes());
    seg.extend_from_slice(&0u16.to_be_bytes());
    after_soi(picture, &seg)
}

#[test]
fn jps_without_descriptor_is_cross_eyed_side_by_side() {
    // "If a JPEG file is thought to contain a stereo image, but a stereoscopic descriptor is not
    // present, ... Side-by-Side, Full Height, Right Field First."
    let cross = jpeg(&side_by_side(&Rgba8::filled(64, 48, BLUE), &Rgba8::filled(64, 48, RED), 0));
    let layout = stereo::layout(&cross, true).expect("a .jps");
    assert_eq!(layout, StereoLayout::SideBySide { first: Eye::Right, gap: 0, squeezed: false });
    let (l, r) = stereo::decode_pair(&cross, &layout, DecodeOptions::default()).unwrap();
    assert_eq!((l.width, l.height), (64, 48));
    assert!(is_red(&l) && is_blue(&r));
    // the same picture named .jpg is an ordinary image
    assert_eq!(stereo::layout(&cross, false), None);
}

#[test]
fn jps_descriptor_layouts() {
    let (l, r) = (Rgba8::filled(64, 48, RED), Rgba8::filled(64, 48, BLUE));
    // stereo (01), side by side (02), left field first (bit 18): 00 04 02 01
    let parallel = with_descriptor(&jpeg(&side_by_side(&l, &r, 0)), 0x0004_0201);
    // over/under (03), left field first, on top
    let over = with_descriptor(&jpeg(&Rgba8::from_fn(64, 96, |_, y| if y < 48 { RED } else { BLUE })), 0x0004_0301);
    // separation 8 pixels between the fields (bits 24-31), right field first
    let separated = with_descriptor(&jpeg(&side_by_side(&r, &l, 8)), 0x0800_0201);
    // half width (bit 17): two squeezed 32-pixel eyes in a 64-pixel picture
    let squeezed = with_descriptor(&jpeg(&side_by_side(&Rgba8::filled(32, 48, RED), &Rgba8::filled(32, 48, BLUE), 0)), 0x0006_0201);
    // interleaved rows (01), left field first: even rows left
    let rows = with_descriptor(&jpeg(&Rgba8::from_fn(64, 96, |_, y| if y % 2 == 0 { RED } else { BLUE })), 0x0004_0101);
    for (name, bytes, size) in
        [("parallel", parallel, (64, 48)), ("over/under", over, (64, 48)), ("separated", separated, (64, 48)), ("squeezed", squeezed, (64, 48))]
    {
        let layout = stereo::layout(&bytes, false).unwrap_or_else(|| panic!("{name}: no layout"));
        let (le, re) = stereo::decode_pair(&bytes, &layout, DecodeOptions::default()).unwrap();
        assert_eq!((le.width, le.height), size, "{name}");
        assert!(is_red(&le) && is_blue(&re), "{name}: left {:?} right {:?}", mean(&le), mean(&re));
    }
    // interleaved: each eye gets every other row, stretched back to the picture's height
    let layout = stereo::layout(&rows, false).unwrap();
    assert_eq!(layout, StereoLayout::Rows { first: Eye::Left });
    let (le, re) = stereo::decode_pair(&rows, &layout, DecodeOptions::default()).unwrap();
    assert_eq!((le.width, le.height), (64, 96));
    assert!(mean(&le)[0] > mean(&re)[0] && mean(&re)[2] > mean(&le)[2], "left {:?} right {:?}", mean(&le), mean(&re));
    // monoscopic descriptors: one picture, whichever eye it is for (both, left, right)
    let flat = jpeg(&l);
    for d in [0x0000_0000, 0x0000_0100, 0x0000_0200] {
        assert_eq!(stereo::layout(&with_descriptor(&flat, d), true), None, "{d:08X}");
    }
}

#[test]
fn jps_anaglyph_eyes_are_rebuilt_from_their_colours() {
    // one picture: the left eye's scene in red, the right eye's in green and blue; the field
    // order bit says which eye the red field is for (1: left)
    let picture = Rgba8::from_fn(64, 48, |x, _| if x < 32 { [220, 20, 20, 255] } else { [20, 200, 200, 255] });
    for (descriptor, red) in [(0x0004_0401, Eye::Left), (0x0000_0401, Eye::Right)] {
        let bytes = with_descriptor(&jpeg(&picture), descriptor);
        let layout = stereo::layout(&bytes, false).unwrap();
        assert_eq!(layout, StereoLayout::Anaglyph { red });
        let (le, re) = stereo::decode_pair(&bytes, &layout, DecodeOptions::default()).unwrap();
        assert_eq!((le.width, le.height, re.width, re.height), (64, 48, 64, 48));
        let (red_eye, cyan_eye) = if red == Eye::Left { (&le, &re) } else { (&re, &le) };
        // the red eye sees the red half bright and the cyan half dark; the cyan eye the reverse
        let px = |d: &Decoded, x: usize| d.to_srgb8().get(x, 24);
        assert!(px(red_eye, 8)[0] > 150 && px(red_eye, 56)[0] < 80, "{:?} {:?}", px(red_eye, 8), px(red_eye, 56));
        assert!(px(cyan_eye, 56)[1] > 150 && px(cyan_eye, 8)[1] < 80, "{:?} {:?}", px(cyan_eye, 8), px(cyan_eye, 56));
    }
}

// --- HEIF (ISO/IEC 23008-12 stereo pair entity group) -------------------------------------------

/// A HEIF like a phone's spatial photo: a primary image (green) and, apart from it, a left (red)
/// and a right (blue) eye of their own size named by a `ster` group.
fn spatial_heif() -> Vec<u8> {
    use lightcraft_heif::testdata::{Spec, build};
    // YCbCr (limited range) of the colours
    let green = |_: u32, _: u32| [145u16, 54, 34];
    let red = |_: u32, _: u32| [81u16, 90, 240];
    let blue = |_: u32, _: u32| [41u16, 240, 110];
    let mut spec = Spec::new(96, 64, &green);
    spec.eyes = Some((64, 32, &red, &blue));
    build(&spec)
}

#[test]
fn heif_stereo_pair_names_the_eyes_items() {
    let bytes = spatial_heif();
    let Some(StereoLayout::Heif { left, right }) = stereo::layout(&bytes, false) else { panic!("no HEIF pair") };
    assert_ne!(left, right);
    // the pair is read from the container alone, with or without the decoder
    assert_eq!(lightcraft_meta::stereo_pair(&bytes).map(|p| (p.left, p.right)), Some((left, right)));
    // an ordinary HEIF has no pair
    let grey = |_: u32, _: u32| [128u16, 128, 128];
    let plain = lightcraft_heif::testdata::build(&lightcraft_heif::testdata::Spec::new(32, 32, &grey));
    assert_eq!(stereo::layout(&plain, false), None);
}

#[cfg(feature = "heif")]
#[test]
fn heif_eyes_decode_from_their_items_and_the_primary_stays_the_photo() {
    let bytes = spatial_heif();
    let layout = stereo::layout(&bytes, false).unwrap();
    let (l, r) = stereo::decode_pair(&bytes, &layout, DecodeOptions::default()).unwrap();
    assert_eq!((l.width, l.height, r.width, r.height), (64, 32, 64, 32));
    assert!(is_red(&l) && is_blue(&r), "left {:?} right {:?}", mean(&l), mean(&r));
    // the 2D picture is the primary image
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert_eq!((d.width, d.height), (96, 64));
    let m = mean(&d);
    assert!(m[1] > m[0] && m[1] > m[2], "{m:?}");
}

#[cfg(not(feature = "heif"))]
#[test]
fn heif_eyes_need_the_decoder() {
    let bytes = spatial_heif();
    let layout = stereo::layout(&bytes, false).unwrap();
    assert!(matches!(stereo::decode_pair(&bytes, &layout, DecodeOptions::default()), Err(Error::Unsupported(Format::Heif, _))));
}

// --- PNG sTER (PNG Extensions 1.4.0 §3.6) ---------------------------------------------------------

fn crc(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for b in data {
        c ^= *b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// Insert a `sTER` chunk before the chunk named `before`.
fn with_ster(png: &[u8], mode: u8, before: &[u8; 4]) -> Vec<u8> {
    let mut p = 8;
    loop {
        let len = u32::from_be_bytes(png[p..p + 4].try_into().unwrap()) as usize;
        if &png[p + 4..p + 8] == before {
            let mut chunk = 1u32.to_be_bytes().to_vec();
            chunk.extend_from_slice(b"sTER");
            chunk.push(mode);
            chunk.extend_from_slice(&crc(&[b"sTER".as_slice(), &[mode]].concat()).to_be_bytes());
            return [&png[..p], &chunk, &png[p..]].concat();
        }
        p += 12 + len;
    }
}

#[test]
fn png_ster_modes_and_padding() {
    let (l, r) = (Rgba8::filled(16, 8, RED), Rgba8::filled(16, 8, BLUE));
    // mode 1, diverging-fuse: the left eye on the left
    let diverging = with_ster(&png(&side_by_side(&l, &r, 0)), 1, b"IDAT");
    // mode 0, cross-fuse: the right eye on the left
    let cross = with_ster(&png(&side_by_side(&r, &l, 0)), 0, b"IDAT");
    for (name, bytes) in [("mode 1", diverging), ("mode 0", cross)] {
        let layout = stereo::layout(&bytes, false).unwrap_or_else(|| panic!("{name}"));
        let (le, re) = stereo::decode_pair(&bytes, &layout, DecodeOptions::default()).unwrap();
        assert_eq!((le.width, le.height), (16, 8), "{name}");
        assert!(is_red(&le) && is_blue(&re), "{name}");
    }
    // 13-pixel eyes: padding = 7 - ((13 - 1) mod 8) = 3 columns, so W = 29 and the right
    // subimage starts at column 16; the decoder recovers padding = 15 - ((29 - 1) mod 16) = 3
    let odd = with_ster(&png(&side_by_side(&Rgba8::filled(13, 8, RED), &Rgba8::filled(13, 8, BLUE), 3)), 1, b"IDAT");
    let layout = stereo::layout(&odd, false).unwrap();
    assert_eq!(layout, StereoLayout::SideBySide { first: Eye::Left, gap: 3, squeezed: false });
    let (le, re) = stereo::decode_pair(&odd, &layout, DecodeOptions::default()).unwrap();
    assert_eq!((le.width, re.width), (13, 13));
    assert!(is_red(&le) && is_blue(&re));
    // a width that would need more than seven padding columns is an error: not a stereo pair
    let bad = with_ster(&png(&Rgba8::filled(33, 8, RED)), 1, b"IDAT");
    assert_eq!(stereo::layout(&bad, false), None);
    // sTER must come before the first IDAT
    let late = with_ster(&png(&side_by_side(&l, &r, 0)), 1, b"IEND");
    assert_eq!(stereo::layout(&late, false), None);
    assert_eq!(stereo::layout(&png(&l), false), None);
}

// --- writing ---------------------------------------------------------------------------------------

fn sbs(eye_w: usize, h: usize) -> Rgba8 {
    side_by_side(&Rgba8::filled(eye_w, h, RED), &Rgba8::filled(eye_w, h, BLUE), 0)
}

#[test]
fn mpo_written_and_read_back() {
    let pair = sbs(64, 48);
    let exif = lightcraft_codecs::exif::minimal_exif(1);
    let meta = EncodeMeta { exif: Some(&exif), ..Default::default() };
    let bytes = stereo::encode_mpo(&EncodeImage::rgba8(&pair), 92, ChromaSubsampling::S444, &meta).unwrap();
    let layout = stereo::layout(&bytes, false).expect("our MPO is a stereo MPO");
    let StereoLayout::Mpo { left, right } = &layout else { panic!("{layout:?}") };
    assert_eq!(left.start, 0, "the left eye is the first image");
    let (l, r) = stereo::decode_pair(&bytes, &layout, DecodeOptions::default()).unwrap();
    assert!(is_red(&l) && is_blue(&r));
    // the file as ordinary JPEG readers see it: the left eye
    assert!(is_red(&decode(&bytes, DecodeOptions::default()).unwrap()));
    // CIPA DC-007 bytes: the MP Extensions follow the Exif APP1; MP Entry 1 is a Representative
    // Disparity image (20020002.H), entry 2 a Disparity image (00020002.H) starting where the
    // first image ends; each image carries its MP Individual Image Number (1, 2)
    let exif_at = bytes.windows(6).position(|w| w == b"Exif\0\0").unwrap();
    let exif_end = exif_at - 2 + u16::from_be_bytes([bytes[exif_at - 2], bytes[exif_at - 1]]) as usize;
    assert_eq!(&bytes[exif_end..exif_end + 2], &[0xFF, 0xE2], "APP2 right after the Exif APP1");
    assert_eq!(&bytes[exif_end + 4..exif_end + 8], b"MPF\0");
    let le = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    let header = exif_end + 8;
    let entries = header + 8 + 2 + 3 * 12 + 4;
    assert_eq!(le(entries), 0x2002_0002);
    assert_eq!(le(entries + 16), 0x0002_0002);
    assert_eq!(le(entries + 16 + 8) as usize + header, right.start);
    assert_eq!(le(entries + 4) as usize, right.start, "the first image's size covers it whole");
    let mpfs: Vec<usize> = bytes.windows(4).enumerate().filter(|(_, w)| *w == b"MPF\0").map(|(i, _)| i).collect();
    assert_eq!(mpfs.len(), 2, "one MPF segment per image");
    assert!(mpfs[1] > right.start);
    let num_at = |from: usize| {
        let p = from + bytes[from..].windows(2).position(|w| w == [0x01, 0xB1]).unwrap();
        le(p + 8)
    };
    assert_eq!(num_at(entries + 32), 1);
    assert_eq!(num_at(mpfs[1]), 2);
}

#[test]
fn jps_written_and_read_back() {
    let pair = sbs(64, 48);
    let bytes = stereo::encode_jps(&EncodeImage::rgba8(&pair), 92, ChromaSubsampling::S444, &EncodeMeta::default()).unwrap();
    // APP3 "_JPSJPS_", block length 4, descriptor 00 00 02 01 (stereo, side by side, full size,
    // right field first, no separation), most significant byte first
    let at = bytes.windows(8).position(|w| w == b"_JPSJPS_").expect("JPS descriptor");
    assert_eq!(&bytes[at - 4..at - 2], &[0xFF, 0xE3]);
    assert_eq!(&bytes[at + 8..at + 14], &[0, 4, 0, 0, 2, 1]);
    // the picture itself is cross-eyed: right eye on the left
    let whole = decode(&bytes, DecodeOptions::default()).unwrap();
    assert_eq!((whole.width, whole.height), (128, 48));
    let left_half = whole.to_srgb8().get(10, 10);
    assert!(left_half[2] > 150, "right eye first, got {left_half:?}");
    let layout = stereo::layout(&bytes, false).expect("descriptor");
    let (l, r) = stereo::decode_pair(&bytes, &layout, DecodeOptions::default()).unwrap();
    assert!(is_red(&l) && is_blue(&r));
}

#[test]
fn png_written_and_read_back() {
    for eye in [16usize, 13] {
        let pair = sbs(eye, 8);
        let bytes = stereo::encode_png(&EncodeImage::rgba8(&pair), &EncodeMeta::default()).unwrap();
        // sTER mode 1 before the first IDAT, with a valid CRC
        let s = bytes.windows(4).position(|w| w == b"sTER").unwrap();
        let idat = bytes.windows(4).position(|w| w == b"IDAT").unwrap();
        assert!(s < idat);
        assert_eq!(&bytes[s - 4..s], &1u32.to_be_bytes());
        assert_eq!(bytes[s + 4], 1);
        assert_eq!(&bytes[s + 5..s + 9], &crc(&bytes[s..s + 5]).to_be_bytes());
        // the padding the chunk needs: none for 16-pixel eyes, 3 columns for 13-pixel ones
        let whole = decode(&bytes, DecodeOptions::default()).unwrap();
        let gap = 7 - ((eye - 1) % 8);
        assert_eq!(whole.width as usize, 2 * eye + gap, "eye {eye}");
        let (l, r) = stereo::decode_pair(&bytes, &stereo::layout(&bytes, false).unwrap(), DecodeOptions::default()).unwrap();
        assert_eq!(l.width as usize, eye);
        assert!(is_red(&l) && is_blue(&r), "eye {eye}");
    }
}

#[test]
fn writers_need_a_side_by_side_pair() {
    let odd = Rgba8::filled(9, 4, GREEN);
    let img = EncodeImage::rgba8(&odd);
    assert!(stereo::encode_mpo(&img, 90, ChromaSubsampling::S444, &EncodeMeta::default()).is_err());
    assert!(stereo::encode_jps(&img, 90, ChromaSubsampling::S444, &EncodeMeta::default()).is_err());
    assert!(stereo::encode_png(&img, &EncodeMeta::default()).is_err());
}

#[test]
fn truncated_stereo_files_never_panic() {
    let pair = sbs(32, 16);
    let files = [
        stereo::encode_mpo(&EncodeImage::rgba8(&pair), 90, ChromaSubsampling::S444, &EncodeMeta::default()).unwrap(),
        stereo::encode_jps(&EncodeImage::rgba8(&pair), 90, ChromaSubsampling::S444, &EncodeMeta::default()).unwrap(),
        stereo::encode_png(&EncodeImage::rgba8(&pair), &EncodeMeta::default()).unwrap(),
    ];
    for f in &files {
        for cut in (0..f.len()).step_by(7) {
            let b = &f[..cut];
            if let Some(l) = stereo::layout(b, true) {
                let _ = stereo::decode_pair(b, &l, DecodeOptions::default());
            }
        }
    }
}
