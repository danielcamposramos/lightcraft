//! Encode → decode round trips on generated fixtures.

use lightcraft_codecs::exif::minimal_exif;
use lightcraft_codecs::icc::{self, write_named};
use lightcraft_codecs::*;
use lightcraft_color::{DISPLAY_P3, REC2020, transfer::srgb_to_linear};
use lightcraft_raster::Rgba8;

fn gradient(w: usize, h: usize) -> Rgba8 {
    Rgba8::from_fn(w, h, |x, y| [(x * 255 / w.max(2).saturating_sub(1).max(1)) as u8, (y * 255 / h.max(2)) as u8, ((x + y) % 256) as u8, 255])
}

fn smooth(w: usize, h: usize) -> Rgba8 {
    Rgba8::from_fn(w, h, |x, y| {
        let fx = x as f32 / w as f32;
        let fy = y as f32 / h as f32;
        [(fx * 255.0) as u8, (fy * 255.0) as u8, ((1.0 - fx) * 200.0) as u8, (((fx + fy) * 0.5) * 255.0) as u8]
    })
}

fn psnr8(a: &Rgba8, b: &Rgba8) -> f64 {
    let mut se = 0.0f64;
    for (p, q) in a.data.iter().zip(&b.data) {
        for c in 0..3 {
            let d = p[c] as f64 - q[c] as f64;
            se += d * d;
        }
    }
    let mse = se / (a.data.len() * 3) as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0 * 255.0 / mse).log10() }
}

#[test]
fn jpeg_roundtrip_with_metadata() {
    let img = smooth(97, 61);
    let icc = write_named(NamedSpace::Srgb);
    let exif = minimal_exif(6);
    let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF/></x:xmpmeta>"#;
    let meta = EncodeMeta { icc: Some(&icc), exif: Some(&exif), xmp: Some(xmp), ..Default::default() };
    for sub in [ChromaSubsampling::S444, ChromaSubsampling::S422, ChromaSubsampling::S420] {
        let bytes = encode_jpeg(&EncodeImage::rgba8(&img), 95, sub, &meta).unwrap();
        assert_eq!(sniff(&bytes), Some(Format::Jpeg));
        let d = decode(&bytes, DecodeOptions::default()).unwrap();
        assert_eq!((d.width, d.height), (97, 61));
        assert_eq!(d.orientation, 6, "orientation passes through, not applied");
        assert_eq!(d.exif.as_deref(), Some(&exif[..]));
        assert_eq!(d.xmp.as_deref(), Some(xmp));
        assert_eq!(d.icc.as_deref(), Some(&icc[..]));
        assert_eq!(d.space.named, Some(NamedSpace::Srgb));
        assert_eq!(d.space.origin, SpaceOrigin::IccMatrixTrc);
        assert!(!d.has_alpha);
        let back = d.to_srgb8();
        let p = psnr8(&img, &back);
        assert!(p > 32.0, "{sub:?} psnr {p}");
    }
}

#[test]
fn jpeg_gray_and_scaled_decode() {
    let g: Vec<u8> = (0..128 * 96).map(|i| (i % 128 * 2) as u8).collect();
    let bytes = encode_jpeg(&EncodeImage::new(128, 96, 1, Samples::U8(&g)), 90, ChromaSubsampling::S444, &EncodeMeta::default()).unwrap();
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert!(d.grayscale);
    assert_eq!(d.space.origin, SpaceOrigin::Untagged);
    let p = d.image.get(64, 10);
    assert!((p[0] - p[1]).abs() < 1e-6 && (p[1] - p[2]).abs() < 1e-6);

    let big = smooth(1000, 600);
    let bytes = encode_jpeg(&EncodeImage::rgba8(&big), 85, ChromaSubsampling::S420, &EncodeMeta::default()).unwrap();
    let d = decode(&bytes, DecodeOptions::fit(100, 100)).unwrap();
    assert_eq!((d.width, d.height), (100, 60));
    assert_eq!((d.source_width, d.source_height), (1000, 600));
    // Colour of the scaled decode matches a box-filtered full decode.
    let full = decode(&bytes, DecodeOptions::default()).unwrap();
    let a = d.image.get(50, 30);
    let b = full.image.get(505, 305);
    for c in 0..3 {
        assert!((a[c] - b[c]).abs() < 0.03, "{a:?} {b:?}");
    }
}

#[test]
fn jpeg_cmyk_converts() {
    // 0 = no ink: white, cyan, black.
    let px: Vec<u8> = [[0u8, 0, 0, 0], [255, 0, 0, 0], [0, 0, 0, 255]].iter().flat_map(|p| p.repeat(16 * 16)).collect();
    let img: Vec<u8> = px;
    let mut out = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut out, 95);
    enc.encode(&img, 16, 48, jpeg_encoder::ColorType::Cmyk).unwrap();
    let d = decode(&out, DecodeOptions::default()).unwrap();
    assert!(d.cmyk);
    assert_eq!(d.space.origin, SpaceOrigin::Naive);
    let white = d.image.get(8, 8);
    let cyan = d.image.get(8, 24);
    let black = d.image.get(8, 40);
    assert!(white.iter().all(|&v| v > 0.9), "{white:?}");
    assert!(cyan[0] < 0.05 && cyan[1] > 0.8 && cyan[2] > 0.8, "{cyan:?}");
    assert!(black.iter().all(|&v| v < 0.02), "{black:?}");
}

#[test]
fn png_8_and_16_bit_exact() {
    let img = gradient(33, 17);
    let bytes = encode_png(&EncodeImage::rgba8(&img), &EncodeMeta::default()).unwrap();
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert!(d.has_alpha);
    assert_eq!(d.bit_depth, 8);
    assert_eq!(d.to_srgb8(), img);

    // 16-bit: precision preserved exactly through linearization.
    let (w, h) = (64u32, 8u32);
    let data: Vec<u16> = (0..w * h * 3).map(|i| (i * 1021 % 65536) as u16).collect();
    let bytes = encode_png(&EncodeImage::new(w, h, 3, Samples::U16(&data)), &EncodeMeta::default()).unwrap();
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert_eq!(d.bit_depth, 16);
    for (i, p) in d.image.data.iter().enumerate() {
        for c in 0..3 {
            let want = Trc::Srgb.to_linear(data[i * 3 + c] as f32 / 65535.0);
            assert!((p[c] - want).abs() < 1e-6);
        }
    }
    // Two adjacent 16-bit codes stay distinct.
    let data = [30000u16, 30000, 30000, 30001, 30001, 30001];
    let bytes = encode_png(&EncodeImage::new(2, 1, 3, Samples::U16(&data)), &EncodeMeta::default()).unwrap();
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert!(d.image.get(1, 0)[0] > d.image.get(0, 0)[0]);
}

#[test]
fn png_metadata_and_gray_alpha() {
    let data: Vec<u8> = (0..20 * 10).flat_map(|i| [(i % 256) as u8, 128]).collect();
    let exif = minimal_exif(3);
    let meta = EncodeMeta { exif: Some(&exif), xmp: Some("<xmp/>"), ..Default::default() };
    let bytes = encode_png(&EncodeImage::new(20, 10, 2, Samples::U8(&data)), &meta).unwrap();
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert!(d.grayscale && d.has_alpha);
    assert_eq!(d.orientation, 3);
    assert_eq!(d.xmp.as_deref(), Some("<xmp/>"));
    let a = d.alpha.as_ref().unwrap().get(0, 0);
    assert!((a - 128.0 / 255.0).abs() < 1e-6);
}

#[test]
fn p3_icc_to_working() {
    let icc = write_named(NamedSpace::DisplayP3);
    let data = [255u8, 0, 0, 0, 255, 0, 128, 128, 128];
    let bytes = encode_png(&EncodeImage::new(3, 1, 3, Samples::U8(&data)), &EncodeMeta { icc: Some(&icc), ..Default::default() }).unwrap();
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert_eq!(d.space.named, Some(NamedSpace::DisplayP3));
    assert_eq!(d.space.origin, SpaceOrigin::IccMatrixTrc);
    let wk = to_working(&d);
    let m = DISPLAY_P3.to_space(&REC2020);
    for (i, px) in [[1.0f32, 0.0, 0.0], [0.0, 1.0, 0.0], [srgb_to_linear(128.0 / 255.0); 3]].iter().enumerate() {
        let want = m.apply_f32(*px);
        let got = wk.get(i, 0);
        for c in 0..3 {
            assert!((got[c] - want[c]).abs() < 1e-3, "{i}: {got:?} vs {want:?}");
        }
    }
    // Gray stays neutral in the working space.
    let g = wk.get(2, 0);
    assert!((g[0] - g[1]).abs() < 1e-4 && (g[1] - g[2]).abs() < 1e-4);
}

#[test]
fn custom_matrix_icc_is_exact() {
    // A non-standard space: goes through the generic matrix/TRC path, not the named one.
    let space = lightcraft_color::RgbSpace {
        name: "Custom",
        r: lightcraft_color::Xy::new(0.66, 0.32),
        g: lightcraft_color::Xy::new(0.25, 0.65),
        b: lightcraft_color::Xy::new(0.14, 0.07),
        white: lightcraft_color::D65,
    };
    let icc = icc::write_matrix_trc(&space, &Trc::Gamma(2.0));
    let data = [255u8, 0, 0];
    let bytes = encode_png(&EncodeImage::new(1, 1, 3, Samples::U8(&data)), &EncodeMeta { icc: Some(&icc), ..Default::default() }).unwrap();
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert_eq!(d.space.named, None);
    let got = to_working(&d).get(0, 0);
    let want = space.to_space(&REC2020).apply_f32([1.0, 0.0, 0.0]);
    for c in 0..3 {
        assert!((got[c] - want[c]).abs() < 2e-3, "{got:?} vs {want:?}");
    }
}

#[test]
fn tiff_depths_and_compressions() {
    let (w, h) = (40u32, 24u32);
    let n = (w * h) as usize;
    let u8s: Vec<u8> = (0..n * 3).map(|i| (i * 7 % 256) as u8).collect();
    let u16s: Vec<u16> = (0..n * 4).map(|i| (i * 977 % 65536) as u16).collect();
    let f32s: Vec<f32> = (0..n * 3).map(|i| i as f32 / 100.0 - 3.0).collect();
    let icc = write_named(NamedSpace::AdobeRgb);
    for comp in [TiffCompression::None, TiffCompression::Lzw, TiffCompression::Deflate, TiffCompression::PackBits] {
        let b = encode_tiff(&EncodeImage::new(w, h, 3, Samples::U8(&u8s)), comp, &EncodeMeta::default()).unwrap();
        assert_eq!(sniff(&b), Some(Format::Tiff));
        let d = decode(&b, DecodeOptions::default()).unwrap();
        assert_eq!(d.bit_depth, 8);
        for (i, p) in d.image.data.iter().enumerate() {
            assert!((p[1] - srgb_to_linear(u8s[i * 3 + 1] as f32 / 255.0)).abs() < 1e-6);
        }

        let meta = EncodeMeta { icc: Some(&icc), xmp: Some("<x/>"), ..Default::default() };
        let b = encode_tiff(&EncodeImage::new(w, h, 4, Samples::U16(&u16s)), comp, &meta).unwrap();
        let d = decode(&b, DecodeOptions::default()).unwrap();
        assert_eq!(d.bit_depth, 16);
        assert!(d.has_alpha);
        assert_eq!(d.space.named, Some(NamedSpace::AdobeRgb));
        assert_eq!(d.xmp.as_deref(), Some("<x/>"));
        let a = d.alpha.as_ref().unwrap();
        for i in 0..n {
            assert!((a.data[i] - u16s[i * 4 + 3] as f32 / 65535.0).abs() < 1e-6);
            let want = Trc::Gamma(563.0 / 256.0).to_linear(u16s[i * 4] as f32 / 65535.0);
            assert!((d.image.data[i][0] - want).abs() < 1e-4, "{} vs {want}", d.image.data[i][0]);
        }

        // Float: exact (untagged float is taken as linear), including out-of-range values.
        let b = encode_tiff(&EncodeImage::new(w, h, 3, Samples::F32(&f32s)), comp, &EncodeMeta::default()).unwrap();
        let d = decode(&b, DecodeOptions::default()).unwrap();
        assert!(d.float);
        assert_eq!(d.bit_depth, 32);
        for (i, p) in d.image.data.iter().enumerate() {
            assert_eq!(p[2], f32s[i * 3 + 2]);
        }
    }
}

#[test]
fn tiff_cmyk_and_orientation() {
    use tiff::encoder::{TiffEncoder, colortype};
    let mut cur = std::io::Cursor::new(Vec::new());
    {
        let mut enc = TiffEncoder::new(&mut cur).unwrap();
        let mut im = enc.new_image::<colortype::CMYK8>(2, 1).unwrap();
        im.encoder().write_tag(tiff::tags::Tag::Orientation, 8u16).unwrap();
        im.write_data(&[0u8, 0, 0, 0, 0, 255, 255, 0]).unwrap();
    }
    let d = decode(&cur.into_inner(), DecodeOptions::default()).unwrap();
    assert!(d.cmyk);
    assert_eq!(d.orientation, 8);
    let white = d.image.get(0, 0);
    let red = d.image.get(1, 0);
    assert!(white.iter().all(|&v| (v - 1.0).abs() < 1e-5), "{white:?}");
    assert!(red[0] > 0.99 && red[1] < 0.01 && red[2] < 0.01, "{red:?}");
}

#[test]
fn webp_lossless_exact() {
    let img = gradient(31, 19);
    let icc = write_named(NamedSpace::Srgb);
    let exif = minimal_exif(2);
    let meta = EncodeMeta { icc: Some(&icc), exif: Some(&exif), xmp: Some("<w/>"), ..Default::default() };
    let bytes = encode_webp_lossless(&EncodeImage::rgba8(&img), &meta).unwrap();
    assert_eq!(sniff(&bytes), Some(Format::WebP));
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert_eq!(d.to_srgb8(), img);
    assert_eq!(d.orientation, 2);
    assert_eq!(d.icc.as_deref(), Some(&icc[..]));
    assert_eq!(d.xmp.as_deref(), Some("<w/>"));
}

#[test]
fn gif_and_bmp_decode() {
    use image::ImageEncoder;
    let img = gradient(16, 8);
    let rgba = img.as_bytes();
    let mut bmp = Vec::new();
    image::codecs::bmp::BmpEncoder::new(&mut bmp).write_image(&rgba, 16, 8, image::ExtendedColorType::Rgba8).unwrap();
    assert_eq!(sniff(&bmp), Some(Format::Bmp));
    let d = decode(&bmp, DecodeOptions::default()).unwrap();
    assert_eq!(d.to_srgb8(), img);

    let mut gif = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new(&mut gif);
        let flat = Rgba8::filled(16, 8, [10, 200, 30, 255]);
        let buf = image::RgbaImage::from_raw(16, 8, flat.as_bytes()).unwrap();
        enc.encode_frame(image::Frame::new(buf)).unwrap();
    }
    assert_eq!(sniff(&gif), Some(Format::Gif));
    let d = decode(&gif, DecodeOptions::default()).unwrap();
    let p = d.to_srgb8().get(3, 3);
    assert!((p[0] as i32 - 10).abs() <= 2 && (p[1] as i32 - 200).abs() <= 2, "{p:?}");
}

/// Hand-built PSD: RGB 8-bit, raw and RLE merged data, ICC resource, negative layer count (alpha).
fn build_psd(rle: bool, with_alpha: bool, icc: &[u8]) -> Vec<u8> {
    let (w, h) = (3u32, 2u32);
    let channels: u16 = if with_alpha { 4 } else { 3 };
    let mut b = b"8BPS".to_vec();
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&[0; 6]);
    b.extend_from_slice(&channels.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&8u16.to_be_bytes());
    b.extend_from_slice(&3u16.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // colour mode data
    // resources: ICC
    let mut res = b"8BIM".to_vec();
    res.extend_from_slice(&1039u16.to_be_bytes());
    res.extend_from_slice(&[0, 0]); // empty pascal name, padded
    res.extend_from_slice(&(icc.len() as u32).to_be_bytes());
    res.extend_from_slice(icc);
    if icc.len() % 2 == 1 {
        res.push(0);
    }
    b.extend_from_slice(&(res.len() as u32).to_be_bytes());
    b.extend_from_slice(&res);
    // layer & mask info: layer info length + negative layer count
    if with_alpha {
        let mut lm = Vec::new();
        lm.extend_from_slice(&2u32.to_be_bytes());
        lm.extend_from_slice(&(-1i16).to_be_bytes());
        b.extend_from_slice(&(lm.len() as u32).to_be_bytes());
        b.extend_from_slice(&lm);
    } else {
        b.extend_from_slice(&0u32.to_be_bytes());
    }
    let planes: Vec<[u8; 6]> = vec![[255, 0, 0, 10, 20, 30], [0, 255, 0, 40, 50, 60], [0, 0, 255, 70, 80, 90], [255, 255, 128, 0, 64, 255]];
    let planes = &planes[..channels as usize];
    if rle {
        b.extend_from_slice(&1u16.to_be_bytes());
        // Each row: literal run of 3 bytes = [2, a, b, c] (4 bytes)
        for _ in 0..channels as usize * h as usize {
            b.extend_from_slice(&4u16.to_be_bytes());
        }
        for p in planes {
            for row in p.chunks(3) {
                b.push(2);
                b.extend_from_slice(row);
            }
        }
    } else {
        b.extend_from_slice(&0u16.to_be_bytes());
        for p in planes {
            b.extend_from_slice(p);
        }
    }
    b
}

#[test]
fn psd_composite() {
    let icc = write_named(NamedSpace::ProPhoto);
    for rle in [false, true] {
        for alpha in [false, true] {
            let b = build_psd(rle, alpha, &icc);
            assert_eq!(sniff(&b), Some(Format::Psd));
            let d = decode(&b, DecodeOptions::default()).unwrap();
            assert_eq!((d.width, d.height), (3, 2));
            assert_eq!(d.has_alpha, alpha);
            assert_eq!(d.space.named, Some(NamedSpace::ProPhoto));
            let p = d.image.get(0, 0);
            assert!((p[0] - 1.0).abs() < 1e-5 && p[1].abs() < 1e-6 && p[2].abs() < 1e-6);
            let q = d.image.get(1, 1);
            assert!((q[0] - Trc::Gamma(1.8).to_linear(20.0 / 255.0)).abs() < 1e-4);
            if alpha {
                assert!((d.alpha.as_ref().unwrap().get(2, 1) - 1.0).abs() < 1e-6);
            }
        }
    }
}

#[test]
fn thumbnail_prefers_embedded_exif() {
    // Main image 800x600 with a 200x150 EXIF thumbnail of a different colour.
    let main = Rgba8::filled(800, 600, [200, 30, 30, 255]);
    let thumb = Rgba8::filled(200, 150, [30, 30, 200, 255]);
    let thumb_jpeg = encode_jpeg(&EncodeImage::rgba8(&thumb), 80, ChromaSubsampling::S420, &EncodeMeta::default()).unwrap();
    let exif = exif_with_thumbnail(&thumb_jpeg, 6);
    let bytes =
        encode_jpeg(&EncodeImage::rgba8(&main), 80, ChromaSubsampling::S420, &EncodeMeta { exif: Some(&exif), ..Default::default() }).unwrap();

    let t = decode_thumbnail(&bytes, 128).unwrap();
    assert_eq!(t.source, ThumbnailSource::ExifThumbnail);
    assert_eq!(t.orientation, 6);
    assert_eq!((t.image.width, t.image.height), (128, 96));
    assert!(t.image.get(10, 10)[2] > 150);
    assert_eq!((t.source_width, t.source_height), (800, 600));

    // Asking for more than the thumbnail offers falls back to a scaled decode.
    let t = decode_thumbnail(&bytes, 256).unwrap();
    assert_eq!(t.source, ThumbnailSource::Scaled);
    assert_eq!((t.image.width, t.image.height), (256, 192));
    assert!(t.image.get(10, 10)[0] > 150);
}

/// Little-endian EXIF with IFD0 (orientation) → IFD1 (JPEGInterchangeFormat/Length) + the thumbnail.
fn exif_with_thumbnail(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    let mut v = b"II*\0".to_vec();
    v.extend_from_slice(&8u32.to_le_bytes());
    // IFD0 at 8: 1 entry, next IFD at 26
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&0x0112u16.to_le_bytes());
    v.extend_from_slice(&3u16.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&orientation.to_le_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(&26u32.to_le_bytes());
    // IFD1 at 26: 2 entries (30 bytes), data at 56
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&0x0201u16.to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&56u32.to_le_bytes());
    v.extend_from_slice(&0x0202u16.to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(v.len(), 56);
    v.extend_from_slice(jpeg);
    v
}

#[test]
fn working_space_identity_for_srgb_white() {
    let img = Rgba8::filled(4, 4, [255, 255, 255, 255]);
    let b = encode_png(&EncodeImage::rgba8(&img), &EncodeMeta::default()).unwrap();
    let d = decode(&b, DecodeOptions::default()).unwrap();
    let w = to_working(&d).get(1, 1);
    for c in w {
        assert!((c - 1.0).abs() < 1e-5);
    }
}

#[test]
fn avif_encode_native() {
    let img = smooth(64, 48);
    let r = encode_avif(&EncodeImage::rgba8(&img), 70, 10, &EncodeMeta::default());
    if cfg!(feature = "avif") {
        let b = r.unwrap();
        assert_eq!(sniff(&b), Some(Format::Avif));
        assert!(matches!(decode(&b, DecodeOptions::default()), Err(Error::Unsupported(Format::Avif, _))));
    } else {
        assert!(r.is_err());
    }
}

#[test]
fn raw_routing() {
    let mut b = b"II*\0\x08\0\0\0".to_vec();
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&0xC612u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&4u32.to_le_bytes());
    b.extend_from_slice(&[1, 4, 0, 0]);
    b.extend_from_slice(&0u32.to_le_bytes());
    assert!(matches!(decode(&b, DecodeOptions::default()), Err(Error::Unsupported(Format::RawTiffLike, _))));
}

#[test]
fn thumbnail_accepts_smaller_preview_when_asked() {
    let main = Rgba8::filled(800, 600, [200, 30, 30, 255]);
    let thumb = Rgba8::filled(160, 120, [30, 30, 200, 255]);
    let thumb_jpeg = encode_jpeg(&EncodeImage::rgba8(&thumb), 80, ChromaSubsampling::S420, &EncodeMeta::default()).unwrap();
    let exif = exif_with_thumbnail(&thumb_jpeg, 1);
    let bytes =
        encode_jpeg(&EncodeImage::rgba8(&main), 80, ChromaSubsampling::S420, &EncodeMeta { exif: Some(&exif), ..Default::default() }).unwrap();
    let t = decode_thumbnail(&bytes, 256).unwrap();
    assert_eq!(t.source, ThumbnailSource::Scaled);
    let t = decode_thumbnail_with(&bytes, &ThumbnailOptions { min_embedded_edge: 160, ..ThumbnailOptions::new(256) }).unwrap();
    assert_eq!(t.source, ThumbnailSource::ExifThumbnail);
    assert_eq!((t.image.width, t.image.height), (160, 120));
}

#[test]
fn unusable_icc_falls_back_to_srgb_with_flag() {
    let bogus = vec![0x42u8; 300];
    let data = [128u8, 64, 32];
    let bytes = encode_png(&EncodeImage::new(1, 1, 3, Samples::U8(&data)), &EncodeMeta { icc: Some(&bogus), ..Default::default() }).unwrap();
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert_eq!(d.space.origin, SpaceOrigin::IccUnsupported);
    assert_eq!(d.space.named, Some(NamedSpace::Srgb));
    assert_eq!(d.icc.as_deref(), Some(&bogus[..]));
    assert!((d.image.get(0, 0)[0] - srgb_to_linear(128.0 / 255.0)).abs() < 1e-6);
}

// Individual Image Attributes (CIPA DC-007-2009 §5.2.3.3.1, Figure 8): flags in bits 31–29, the
// MP Type Code (Table 4) in bits 23–0.
const MP_PARENT: u32 = 0x8000_0000;
const MP_CHILD: u32 = 0x4000_0000;
const MP_REPRESENTATIVE: u32 = 0x2000_0000;
const MP_PRIMARY: u32 = 0x03_0000;
const MP_LARGE_THUMBNAIL_VGA: u32 = 0x01_0001;
const MP_LARGE_THUMBNAIL_FULL_HD: u32 = 0x01_0002;
const MP_PANORAMA: u32 = 0x02_0001;
const MP_DISPARITY: u32 = 0x02_0002;
const MP_MULTI_ANGLE: u32 = 0x02_0003;
const MP_UNDEFINED: u32 = 0x00_0000;

/// One Individual Image of an MP file: its JPEG, Individual Image Attribute and Dependent Image 1
/// and 2 Entry Numbers.
struct MpImage<'a> {
    jpeg: &'a [u8],
    attr: u32,
    deps: [u16; 2],
}

/// Splice an MPF APP2 segment (MP Index IFD with one MP Entry per image) right after the first
/// image's SOI and append the other images after it.
fn with_mpf(images: &[MpImage]) -> Vec<u8> {
    // MPF segment payload: "MPF\0" + TIFF (LE) with one IFD holding 3 entries + the MP entries.
    let n = images.len();
    let mut t = b"II*\0".to_vec();
    t.extend_from_slice(&8u32.to_le_bytes());
    t.extend_from_slice(&3u16.to_le_bytes());
    let ifd_end = 8 + 2 + 3 * 12 + 4; // 50
    let entries_off = ifd_end as u32;
    let push = |t: &mut Vec<u8>, tag: u16, typ: u16, count: u32, val: [u8; 4]| {
        t.extend_from_slice(&tag.to_le_bytes());
        t.extend_from_slice(&typ.to_le_bytes());
        t.extend_from_slice(&count.to_le_bytes());
        t.extend_from_slice(&val);
    };
    push(&mut t, 0xB000, 7, 4, *b"0100");
    push(&mut t, 0xB001, 4, 1, (n as u32).to_le_bytes());
    push(&mut t, 0xB002, 7, 16 * n as u32, entries_off.to_le_bytes());
    t.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(t.len(), ifd_end);
    let seg_len = 2 + 4 + t.len() + 16 * n;
    let mpf_header_pos = 2 + 4 + 4; // SOI, FFE2+len, "MPF\0"
    let first = images[0].jpeg;
    let first_len = first.len() + 2 + seg_len;
    let mut next = first_len;
    for (i, img) in images.iter().enumerate() {
        // Data offsets are counted from the MP header; the first image's is 0 (§5.2.3.3.3).
        let (size, off) = if i == 0 { (first_len, 0) } else { (img.jpeg.len(), next - mpf_header_pos) };
        if i > 0 {
            next += img.jpeg.len();
        }
        t.extend_from_slice(&img.attr.to_le_bytes());
        t.extend_from_slice(&(size as u32).to_le_bytes());
        t.extend_from_slice(&(off as u32).to_le_bytes());
        t.extend_from_slice(&img.deps[0].to_le_bytes());
        t.extend_from_slice(&img.deps[1].to_le_bytes());
    }
    let mut out = first[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE2]);
    out.extend_from_slice(&(seg_len as u16).to_be_bytes());
    out.extend_from_slice(b"MPF\0");
    out.extend_from_slice(&t);
    out.extend_from_slice(&first[2..]);
    assert_eq!(out.len(), first_len);
    for img in &images[1..] {
        out.extend_from_slice(img.jpeg);
    }
    out
}

/// A Baseline MP file (CIPA DC-007-2009 §6.1): the primary image and its Class 1 Large Thumbnail.
fn with_mpf_preview(primary: &[u8], preview: &[u8]) -> Vec<u8> {
    with_mpf(&[
        MpImage { jpeg: primary, attr: MP_PARENT | MP_REPRESENTATIVE | MP_PRIMARY, deps: [2, 0] },
        MpImage { jpeg: preview, attr: MP_CHILD | MP_LARGE_THUMBNAIL_VGA, deps: [0, 0] },
    ])
}

#[test]
fn thumbnail_from_mpf_preview() {
    let main = Rgba8::filled(1600, 1200, [200, 30, 30, 255]);
    let prev = Rgba8::filled(640, 480, [30, 200, 30, 255]);
    let enc = |i: &Rgba8| encode_jpeg(&EncodeImage::rgba8(i), 80, ChromaSubsampling::S420, &EncodeMeta::default()).unwrap();
    let bytes = with_mpf_preview(&enc(&main), &enc(&prev));
    // Still a valid JPEG for the full decoder.
    let d = decode(&bytes, DecodeOptions::default()).unwrap();
    assert_eq!((d.width, d.height), (1600, 1200));
    let t = decode_thumbnail(&bytes, 256).unwrap();
    assert_eq!(t.source, ThumbnailSource::MpfPreview);
    assert_eq!((t.image.width, t.image.height), (256, 192));
    assert!(t.image.get(5, 5)[1] > 150);
}

#[test]
fn thumbnail_from_mpf_large_thumbnail_without_dependency_links() {
    // Dependent Image Entry Numbers left at 0: with no other image in the file, the Large
    // Thumbnail can only be the primary image's.
    let main = Rgba8::filled(2560, 1440, [200, 30, 30, 255]);
    let prev = Rgba8::filled(1920, 1080, [30, 200, 30, 255]);
    let enc = |i: &Rgba8| encode_jpeg(&EncodeImage::rgba8(i), 80, ChromaSubsampling::S420, &EncodeMeta::default()).unwrap();
    let (main, prev) = (enc(&main), enc(&prev));
    let bytes = with_mpf(&[
        MpImage { jpeg: &main, attr: MP_REPRESENTATIVE | MP_PRIMARY, deps: [0, 0] },
        MpImage { jpeg: &prev, attr: MP_LARGE_THUMBNAIL_FULL_HD, deps: [0, 0] },
    ]);
    let t = decode_thumbnail(&bytes, 256).unwrap();
    assert_eq!(t.source, ThumbnailSource::MpfPreview);
    assert!(t.image.get(5, 5)[1] > 150);
}

#[test]
fn thumbnail_never_shows_another_mpo_view() {
    // CIPA DC-007-2009 Table 4: only a Large Thumbnail is a preview. The second image of a stereo
    // MPO (disparity) is the other eye; multi-angle and panorama images are other views; an
    // undefined image is unknown. The thumbnail is the first image, scaled.
    let first = Rgba8::filled(640, 480, [200, 30, 30, 255]);
    let other = Rgba8::filled(640, 480, [30, 200, 30, 255]);
    let enc = |i: &Rgba8| encode_jpeg(&EncodeImage::rgba8(i), 80, ChromaSubsampling::S420, &EncodeMeta::default()).unwrap();
    let (first, other) = (enc(&first), enc(&other));
    for mp_type in [MP_DISPARITY, MP_MULTI_ANGLE, MP_PANORAMA, MP_UNDEFINED] {
        let bytes = with_mpf(&[
            MpImage { jpeg: &first, attr: MP_REPRESENTATIVE | mp_type, deps: [0, 0] },
            MpImage { jpeg: &other, attr: mp_type, deps: [0, 0] },
        ]);
        let t = decode_thumbnail(&bytes, 256).unwrap();
        assert_eq!(t.source, ThumbnailSource::Scaled, "MP Type {mp_type:06X}");
        assert!(t.image.get(5, 5)[0] > 150, "MP Type {mp_type:06X}: the first image, got {:?}", t.image.get(5, 5));
    }
}

#[test]
fn thumbnail_takes_the_first_views_own_large_thumbnail() {
    // A stereo MPO with a Large Thumbnail for each eye (§6.2.3): each belongs to the image that
    // lists it as a Dependent Image. Entry 3 is the right eye's and is listed first in the file.
    let left = Rgba8::filled(1280, 960, [200, 30, 30, 255]);
    let right = Rgba8::filled(1280, 960, [30, 200, 30, 255]);
    let right_thumb = Rgba8::filled(640, 480, [30, 30, 200, 255]);
    let left_thumb = Rgba8::filled(640, 480, [200, 200, 30, 255]);
    let enc = |i: &Rgba8| encode_jpeg(&EncodeImage::rgba8(i), 80, ChromaSubsampling::S420, &EncodeMeta::default()).unwrap();
    let (left, right, right_thumb, left_thumb) = (enc(&left), enc(&right), enc(&right_thumb), enc(&left_thumb));
    let stereo = |left_deps: [u16; 2], right_deps: [u16; 2]| {
        with_mpf(&[
            MpImage { jpeg: &left, attr: MP_PARENT | MP_REPRESENTATIVE | MP_DISPARITY, deps: left_deps },
            MpImage { jpeg: &right, attr: MP_PARENT | MP_DISPARITY, deps: right_deps },
            MpImage { jpeg: &right_thumb, attr: MP_CHILD | MP_LARGE_THUMBNAIL_VGA, deps: [0, 0] },
            MpImage { jpeg: &left_thumb, attr: MP_CHILD | MP_LARGE_THUMBNAIL_VGA, deps: [0, 0] },
        ])
    };
    let t = decode_thumbnail(&stereo([4, 0], [3, 0]), 256).unwrap();
    assert_eq!(t.source, ThumbnailSource::MpfPreview);
    let p = t.image.get(5, 5);
    assert!(p[0] > 150 && p[1] > 150 && p[2] < 100, "the left eye's Large Thumbnail, got {p:?}");
    // Without the links neither thumbnail can be told to belong to the left eye: scaled decode.
    let t = decode_thumbnail(&stereo([0, 0], [0, 0]), 256).unwrap();
    assert_eq!(t.source, ThumbnailSource::Scaled);
    assert!(t.image.get(5, 5)[0] > 150 && t.image.get(5, 5)[1] < 100);
}

#[test]
fn print_resolution_is_written() {
    let data = vec![128u8; 4 * 4 * 3];
    let img = EncodeImage::new(4, 4, 3, Samples::U8(&data));
    let meta = EncodeMeta { ppi: Some(300), ..Default::default() };
    for sub in [ChromaSubsampling::S444, ChromaSubsampling::S422] {
        let j = encode_jpeg(&img, 90, sub, &meta).unwrap();
        // SOI, APP0 "JFIF\0" v1.2, units = 1 (dpi), Xdensity, Ydensity
        assert_eq!(&j[2..4], &[0xFF, 0xE0], "{sub:?}");
        assert_eq!(&j[6..11], b"JFIF\0");
        assert_eq!(&j[13..18], &[1, 1, 44, 1, 44], "{sub:?}");
        assert_eq!(j.windows(4).filter(|w| *w == [0xFF, 0xE0, 0, 16]).count(), 1, "one APP0");
    }
    let png = encode_png(&img, &meta).unwrap();
    let i = png.windows(4).position(|w| w == b"pHYs").expect("pHYs chunk");
    assert_eq!(u32::from_be_bytes(png[i + 4..i + 8].try_into().unwrap()), 11811, "300 ppi in pixels per metre");
    assert_eq!(png[i + 12], 1, "unit = metre");
    let tif = encode_tiff(&img, TiffCompression::Deflate, &meta).unwrap();
    let mut d = tiff::decoder::Decoder::new(std::io::Cursor::new(tif)).unwrap();
    let x = d.get_tag(tiff::tags::Tag::XResolution).unwrap();
    assert_eq!(x.into_u32_vec().ok(), Some(vec![300, 1]));
    assert_eq!(d.get_tag_u32(tiff::tags::Tag::ResolutionUnit).unwrap(), 2, "inch");
    // without a ppi: JFIF density stays an aspect ratio
    let j = encode_jpeg(&img, 90, ChromaSubsampling::S444, &EncodeMeta::default()).unwrap();
    assert_eq!(&j[13..18], &[0, 0, 1, 0, 1]);
}
