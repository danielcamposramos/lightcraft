//! Headless tests of stereo photos in the loupe: View ▸ Stereo View shows both eyes side by side
//! (tools on the left eye), off it shows the photo alone, and the before/after views compare the
//! left eye.

use std::time::Duration;

use lightcraft_codecs::{ChromaSubsampling, EncodeImage, EncodeMeta, stereo};
use lightcraft_raster::Rgba8;
use serde_json::json;

use crate::headless::Headless;
use crate::render::Slot;
use crate::{LightcraftApp, Services};

const T: Duration = Duration::from_secs(20);
const SETTLE: Duration = Duration::from_secs(120);

/// The loupe on a stereo MPO of two 300×200 eyes (left red, right blue).
fn stereo_loupe() -> Headless {
    let dir = std::env::temp_dir().join(format!("lc-ui-stereo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pair = Rgba8::from_fn(600, 200, |x, _| if x < 300 { [200, 40, 40, 255] } else { [40, 40, 200, 255] });
    let path = dir.join("pair.mpo");
    std::fs::write(&path, stereo::encode_mpo(&EncodeImage::rgba8(&pair), 90, ChromaSubsampling::S444, &EncodeMeta::default()).unwrap()).unwrap();
    let mut s = lightcraft_engine::Session::new().with_fs();
    s.execute("library.import", &json!({"paths": [path.to_string_lossy()]})).unwrap();
    let id = s.catalog.photos().next().unwrap().id;
    s.execute("library.select", &json!({"ids": [id.0]})).unwrap();
    let app = LightcraftApp::new(s, Services { png: None, ..Default::default() });
    let mut h = Headless::new(app, [1200.0, 800.0], 1.0);
    let r = h.request("ui.set", json!({"view": "detail"}), T);
    assert_eq!(r["ok"], true, "{r}");
    h
}

/// Width / height of the loupe's render.
fn main_aspect(h: &Headless) -> f32 {
    let t = h.app.renderer.textures.get(&Slot::Main).expect("a loupe render");
    t.size[0] as f32 / t.size[1] as f32
}

fn aspect_is(h: &Headless, a: f32) -> bool {
    h.app.renderer.textures.get(&Slot::Main).is_some_and(|t| (t.size[0] as f32 / t.size[1] as f32 - a).abs() < 0.05)
}

#[test]
fn stereo_view_shows_both_eyes_and_tools_use_the_left_one() {
    let mut h = stereo_loupe();
    // off (the default): the photo alone, its 3:2 left eye
    assert!(!h.app.ui.stereo_view);
    assert!(h.step_until(SETTLE, |h| aspect_is(h, 1.5)), "{}", main_aspect(&h));
    // on: both 3:2 eyes side by side (3:1)
    let r = h.request("engine.execute", json!({"command": "view.stereo", "params": {"on": true}}), T);
    assert_eq!(r["ok"], true, "{r}");
    assert!(h.step_until(SETTLE, |h| aspect_is(h, 3.0)), "{}", main_aspect(&h));
    let t = h.app.renderer.textures.get(&Slot::Main).unwrap();
    let px = t.pixels.as_ref().expect("main pixels");
    let [w, hh] = px.size;
    let (l, r) = (px.pixels[hh / 2 * w + w / 4], px.pixels[hh / 2 * w + w * 3 / 4]);
    assert!(l.r() > l.b().saturating_add(60) && r.b() > r.r().saturating_add(60), "left eye red, right eye blue: {l:?} {r:?}");
    // tools and overlays map onto the left eye only
    let img = h.app.image_rect.expect("image on screen");
    assert!((img.width() / img.height() - 1.5).abs() < 0.02, "the left eye's frame, {img:?}");
    // without params the command toggles
    let r = h.request("engine.execute", json!({"command": "view.stereo"}), T);
    assert_eq!(r["result"]["on"], false, "{r}");
    assert!(h.step_until(SETTLE, |h| aspect_is(h, 1.5)), "{}", main_aspect(&h));
}

#[test]
fn before_and_after_compare_the_left_eye() {
    let mut h = stereo_loupe();
    h.app.ui.stereo_view = true;
    let r = h.request("engine.execute", json!({"command": "view.beforeAfter"}), T);
    assert_eq!(r["ok"], true, "{r}");
    assert!(h.settle(SETTLE));
    // side-by-side before/after: one eye per view, no pair
    assert!((main_aspect(&h) - 1.5).abs() < 0.05, "{}", main_aspect(&h));
}
