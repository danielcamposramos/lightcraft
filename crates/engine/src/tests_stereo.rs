//! Stereo photos (MPO, JPS, PNG `sTER`, spatial HEICs) end to end: import as one photo, both eyes
//! developed with the same settings, side by side, and export as pairs. Each eye has its own
//! colour (left red, right blue) so a swapped or lost eye shows.

use std::path::Path;

use lightcraft_codecs::stereo::{self, StereoLayout};
use lightcraft_codecs::{ChromaSubsampling, DecodeOptions, EncodeImage, EncodeMeta, encode_jpeg};
use lightcraft_raster::Rgba8;
use serde_json::json;

use crate::Session;
use crate::export::{ExportFormat, ExportOptions, Watermark, export_photo};
use crate::tests_xmp::temp_dir;

const RED: [u8; 4] = [200, 40, 40, 255];
const BLUE: [u8; 4] = [40, 40, 200, 255];

/// A full side-by-side pair: `w`×`h` eyes, left red, right blue.
fn pair(w: usize, h: usize) -> Rgba8 {
    Rgba8::from_fn(2 * w, h, |x, _| if x < w { RED } else { BLUE })
}

/// Mean colour of `img` over columns `x0..x1`.
fn mean(img: &Rgba8, x0: usize, x1: usize) -> [f32; 3] {
    let mut sum = [0f64; 3];
    let mut n = 0f64;
    for y in 0..img.height {
        for x in x0..x1.min(img.width) {
            let p = img.get(x, y);
            for c in 0..3 {
                sum[c] += p[c] as f64;
            }
            n += 1.0;
        }
    }
    sum.map(|v| (v / n.max(1.0)) as f32)
}

fn reddish(c: [f32; 3]) -> bool {
    c[0] > c[2] + 60.0
}

fn bluish(c: [f32; 3]) -> bool {
    c[2] > c[0] + 60.0
}

/// The stereo files of one pair, written into `dir`: MPO, JPS (with and without a descriptor),
/// PNG with `sTER`, and a plain JPEG for comparison.
fn write_files(dir: &Path) {
    let p = pair(48, 32);
    let e = EncodeImage::rgba8(&p);
    let m = EncodeMeta::default();
    std::fs::write(dir.join("pair.mpo"), stereo::encode_mpo(&e, 92, ChromaSubsampling::S444, &m).unwrap()).unwrap();
    std::fs::write(dir.join("pair.jps"), stereo::encode_jps(&e, 92, ChromaSubsampling::S444, &m).unwrap()).unwrap();
    std::fs::write(dir.join("pair.png"), stereo::encode_png(&e, &m).unwrap()).unwrap();
    // a JPS without a descriptor: cross-eyed, right eye on the left
    let cross = Rgba8::from_fn(96, 32, |x, _| if x < 48 { BLUE } else { RED });
    std::fs::write(dir.join("cross.jps"), encode_jpeg(&EncodeImage::rgba8(&cross), 92, ChromaSubsampling::S444, &m).unwrap()).unwrap();
    std::fs::write(dir.join("flat.jpg"), encode_jpeg(&e, 92, ChromaSubsampling::S444, &m).unwrap()).unwrap();
}

fn import_one(s: &mut Session, path: &Path) -> lightcraft_catalog::PhotoId {
    let r = s.execute("library.import", &json!({"paths": [path.to_string_lossy()]})).unwrap();
    let id = lightcraft_catalog::PhotoId(r["imported"][0].as_u64().expect("imported"));
    s.execute("library.select", &json!({"ids": [id.0]})).unwrap();
    id
}

#[test]
fn stereo_files_import_as_one_photo_with_both_eyes() {
    let dir = temp_dir("stereo-import");
    write_files(&dir);
    let mut s = Session::new().with_fs();
    s.execute("library.import", &json!({"paths": [dir.to_string_lossy()]})).unwrap();
    let mut seen: Vec<(String, bool, u32, u32)> = s.catalog.photos().map(|p| (p.file_name.clone(), p.is_stereo(), p.width, p.height)).collect();
    seen.sort();
    // a folder brings in .mpo and .jps too; a pair is one photo, one eye's size
    assert_eq!(
        seen,
        vec![
            ("cross.jps".into(), true, 48, 32),
            ("flat.jpg".into(), false, 96, 32),
            ("pair.jps".into(), true, 48, 32),
            ("pair.mpo".into(), true, 48, 32),
            ("pair.png".into(), true, 48, 32),
        ]
    );
}

#[test]
fn stereo_view_develops_both_eyes_with_the_same_settings() {
    let dir = temp_dir("stereo-view");
    write_files(&dir);
    for name in ["pair.mpo", "pair.jps", "pair.png", "cross.jps"] {
        let mut s = Session::new().with_fs();
        let id = import_one(&mut s, &dir.join(name));
        // the photo itself (grid, tools): the left eye
        let one = s.render_job(id, 48, 48, false, true).unwrap().run().rendered.unwrap().image;
        assert_eq!((one.width, one.height), (48, 32), "{name}");
        assert!(reddish(mean(&one, 0, 48)), "{name}: the left eye {:?}", mean(&one, 0, 48));
        // both eyes, full side by side, each eye the size the photo renders at
        let before = s.render_job(id, 48, 48, false, true).unwrap().with_stereo().run().rendered.unwrap().image;
        assert_eq!((before.width, before.height), (96, 32), "{name}");
        assert!(reddish(mean(&before, 0, 48)) && bluish(mean(&before, 48, 96)), "{name}: {:?} | {:?}", mean(&before, 0, 48), mean(&before, 48, 96));
        // an edit reaches both eyes
        let mut s2 = s;
        s2.execute("develop.set", &json!({"control": "light.exposure", "value": 1.0})).unwrap();
        let after = s2.render_job(id, 48, 48, false, true).unwrap().with_stereo().run().rendered.unwrap().image;
        let sum = |c: [f32; 3]| c.iter().sum::<f32>();
        assert!(sum(mean(&after, 0, 48)) > sum(mean(&before, 0, 48)) + 30.0, "{name}: left eye brighter");
        assert!(sum(mean(&after, 48, 96)) > sum(mean(&before, 48, 96)) + 30.0, "{name}: right eye brighter");
        // and so does a crop: each eye is cropped the same way and stays on its side
        s2.execute("crop.set", &json!({"rect": [0.0, 0.0, 0.5, 1.0]})).unwrap();
        let eye = s2.render_job(id, 48, 48, false, true).unwrap().run().rendered.unwrap().image;
        let cropped = s2.render_job(id, 48, 48, false, true).unwrap().with_stereo().run().rendered.unwrap().image;
        assert!(eye.width < eye.height, "{name}: the left half of a 3:2 eye is upright, got {}×{}", eye.width, eye.height);
        assert_eq!((cropped.width, cropped.height), (2 * eye.width, eye.height), "{name}");
        let w = eye.width;
        assert!(reddish(mean(&cropped, 0, w)) && bluish(mean(&cropped, w, 2 * w)), "{name}: cropped eyes");
    }
}

#[test]
fn a_photo_without_eyes_refuses_a_stereo_render() {
    let dir = temp_dir("stereo-flat");
    write_files(&dir);
    let mut s = Session::new().with_fs();
    let id = import_one(&mut s, &dir.join("flat.jpg"));
    let r = s.render_job(id, 48, 48, false, true).unwrap().with_stereo().run().rendered;
    assert!(r.as_ref().is_err_and(|e| e.contains("two eyes")), "{:?}", r.map(|r| r.image.width));
}

/// The eyes of an exported stereo file, by the codecs' reader.
fn eyes(bytes: &[u8], jps: bool) -> (lightcraft_codecs::Decoded, lightcraft_codecs::Decoded) {
    let layout = stereo::layout(bytes, jps).expect("a stereo file");
    stereo::decode_pair(bytes, &layout, DecodeOptions::default()).unwrap()
}

#[test]
fn stereo_photos_export_both_eyes_as_pairs() {
    let dir = temp_dir("stereo-export");
    write_files(&dir);
    let mut s = Session::new().with_fs();
    let id = import_one(&mut s, &dir.join("pair.mpo"));
    let full = |format: &str| ExportOptions::from_json(&json!({"format": format, "longEdge": 0, "bothEyes": true}));
    // without Both Eyes a stereo photo exports as the photo, its left eye
    let e = export_photo(&mut s, id, &ExportOptions::from_json(&json!({"format": "jpeg", "longEdge": 0})), 1).unwrap();
    assert_eq!((e.width, e.height), (48, 32));
    let d = lightcraft_codecs::decode(&e.bytes, DecodeOptions::default()).unwrap().to_srgb8();
    assert!(reddish(mean(&d, 0, 48)), "{:?}", mean(&d, 0, 48));
    let e = export_photo(&mut s, id, &ExportOptions::from_json(&json!({"format": "png", "longEdge": 0})), 1).unwrap();
    assert!(!e.bytes.windows(4).any(|w| w == b"sTER"), "a photo alone carries no stereo mark");
    // MPO: two JPEGs, the left eye first (Both Eyes or not)
    let e = export_photo(&mut s, id, &ExportOptions::from_json(&json!({"format": "mpo", "longEdge": 0})), 1).unwrap();
    assert!(e.file_name.ends_with(".mpo"));
    let StereoLayout::Mpo { left, .. } = stereo::layout(&e.bytes, false).unwrap() else { panic!("not an MPO") };
    assert_eq!(left.start, 0);
    let (l, r) = eyes(&e.bytes, false);
    assert_eq!((l.width, l.height), (48, 32));
    assert!(reddish(mean(&l.to_srgb8(), 0, 48)) && bluish(mean(&r.to_srgb8(), 0, 48)));
    // JPS: one cross-eyed JPEG with a descriptor
    let e = export_photo(&mut s, id, &full("jps"), 1).unwrap();
    assert!(e.file_name.ends_with(".jps"));
    assert_eq!((e.width, e.height), (96, 32));
    let (l, r) = eyes(&e.bytes, false);
    assert!(reddish(mean(&l.to_srgb8(), 0, 48)) && bluish(mean(&r.to_srgb8(), 0, 48)));
    // PNG: side by side with sTER (48-pixel eyes need no padding)
    let e = export_photo(&mut s, id, &full("png"), 1).unwrap();
    assert!(e.bytes.windows(4).any(|w| w == b"sTER"));
    let (l, r) = eyes(&e.bytes, false);
    assert!(reddish(mean(&l.to_srgb8(), 0, 48)) && bluish(mean(&r.to_srgb8(), 0, 48)));
    // 16-bit PNG keeps both eyes too
    let e =
        export_photo(&mut s, id, &ExportOptions::from_json(&json!({"format": "png", "bitDepth": 16, "longEdge": 0, "bothEyes": true})), 1).unwrap();
    let (l, r) = eyes(&e.bytes, false);
    assert_eq!(l.bit_depth, 16);
    assert!(reddish(mean(&l.to_srgb8(), 0, 48)) && bluish(mean(&r.to_srgb8(), 0, 48)));
    // formats without a stereo mark: full side by side, left eye first
    for f in ["jpeg", "tiff", "webp"] {
        let e = export_photo(&mut s, id, &full(f), 1).unwrap();
        assert_eq!((e.width, e.height), (96, 32), "{f}");
        let d = lightcraft_codecs::decode(&e.bytes, DecodeOptions::default()).unwrap().to_srgb8();
        assert!(reddish(mean(&d, 0, 48)) && bluish(mean(&d, 48, 96)), "{f}: {:?} | {:?}", mean(&d, 0, 48), mean(&d, 48, 96));
    }
    // an ordinary photo can't become an MPO or a JPS
    let flat = import_one(&mut s, &dir.join("flat.jpg"));
    for f in [ExportFormat::Mpo, ExportFormat::Jps] {
        let o = ExportOptions { format: f, ..Default::default() };
        let err = export_photo(&mut s, flat, &o, 1).err().expect("refused");
        assert!(err.contains("stereo photo"), "{err}");
    }
}

#[test]
fn export_finishing_goes_into_each_eye() {
    // a watermark anchored bottom right lands in both eyes, not only at the pair's right edge
    let dir = temp_dir("stereo-watermark");
    write_files(&dir);
    let mut s = Session::new().with_fs();
    let id = import_one(&mut s, &dir.join("pair.mpo"));
    let mut o = ExportOptions::from_json(&json!({"format": "png", "longEdge": 0, "bothEyes": true}));
    o.watermark = Some(Watermark { text: "3D".into(), size: 0.4, opacity: 1.0, color: [255, 255, 255], shadow: false, ..Default::default() });
    let e = export_photo(&mut s, id, &o, 1).unwrap();
    let (l, r) = eyes(&e.bytes, false);
    let white = |d: &lightcraft_codecs::Decoded| d.to_srgb8().data.iter().filter(|p| p[0] > 220 && p[1] > 220 && p[2] > 220).count();
    let (wl, wr) = (white(&l), white(&r));
    assert!(wl > 10 && wr > 10, "watermark pixels: left {wl}, right {wr}");
    assert!(wl.abs_diff(wr) * 4 <= wl.max(wr), "the same watermark in both eyes: {wl} vs {wr}");
}

/// A spatial HEIC: the photo stays its primary image (green), and both eyes come from the stereo
/// pair's items (left red, right blue). The name holds "heic" so `cargo xtask ci` runs it with the
/// HEIF decoder.
#[test]
fn spatial_heic_shows_and_exports_its_eyes_beside_its_primary_image() {
    use lightcraft_heif::testdata::{Spec, build};
    if !lightcraft_codecs::Format::Heif.can_decode() {
        return; // importing a HEIC needs the decoder
    }
    let (green, red, blue) = (|_: u32, _: u32| [145u16, 54, 34], |_: u32, _: u32| [81u16, 90, 240], |_: u32, _: u32| [41u16, 240, 110]);
    let mut spec = Spec::new(96, 64, &green);
    spec.eyes = Some((96, 64, &red, &blue));
    let dir = temp_dir("stereo-heic");
    std::fs::write(dir.join("spatial.heic"), build(&spec)).unwrap();
    let mut s = Session::new().with_fs();
    let id = import_one(&mut s, &dir.join("spatial.heic"));
    let p = s.catalog.photo(id).unwrap();
    assert_eq!((p.stereo, p.width, p.height), (Some(lightcraft_catalog::Stereo::Heif), 96, 64));
    let one = s.render_job(id, 96, 96, false, true).unwrap().run().rendered.unwrap().image;
    let c = mean(&one, 0, one.width);
    assert!(c[1] > c[0] + 40.0 && c[1] > c[2] + 40.0, "the photo is the primary image: {c:?}");
    let both = s.render_job(id, 96, 96, false, true).unwrap().with_stereo().run().rendered.unwrap().image;
    assert_eq!((both.width, both.height), (192, 64));
    assert!(reddish(mean(&both, 0, 96)) && bluish(mean(&both, 96, 192)), "{:?} | {:?}", mean(&both, 0, 96), mean(&both, 96, 192));
    let e = export_photo(&mut s, id, &ExportOptions::from_json(&json!({"format": "mpo", "longEdge": 0})), 1).unwrap();
    let (l, r) = eyes(&e.bytes, false);
    assert!(reddish(mean(&l.to_srgb8(), 0, 96)) && bluish(mean(&r.to_srgb8(), 0, 96)));
}
