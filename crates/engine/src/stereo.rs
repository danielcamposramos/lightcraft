//! Stereo photos: a HEIF stereo pair, an MPO, a JPS or a PNG with `sTER` (read by
//! [`lightcraft_codecs::stereo`]) is one catalog photo with two eyes. The loader decodes the eyes
//! with the photo's picture ([`lightcraft_pipeline::SourceInfo::stereo`]); the picture is what
//! everything else sees (dimensions, tools, thumbnails): the left eye, or a HEIF's primary image.
//! A render job asks for both eyes with [`crate::media::RenderJob::with_stereo`]: each is developed
//! with the photo's settings, so every edit applies to both, and the pair is shown or exported full
//! side by side, the left eye on the left.

use lightcraft_pipeline::{DeepImage, DeepSamples, Rendered};
use lightcraft_raster::{Histogram, Image};

/// Two images of the same height side by side (`a` left).
pub fn side_by_side<T: Copy + Default>(a: &Image<T>, b: &Image<T>) -> Image<T> {
    let (w, h) = (a.width + b.width, a.height.min(b.height));
    let mut data = Vec::with_capacity(w * h);
    for y in 0..h {
        data.extend_from_slice(a.row(y));
        data.extend_from_slice(b.row(y));
    }
    Image { width: w, height: h, data }
}

/// The two halves of a side-by-side pair (left, right).
pub fn halves<T: Copy + Default>(pair: &Image<T>) -> (Image<T>, Image<T>) {
    let eye = pair.width / 2;
    (pair.crop(0, 0, eye, pair.height), pair.crop(eye, 0, eye, pair.height))
}

/// Rows of `a` (`aw` samples each) followed by the rows of `b` (`bw` samples each), `h` rows.
fn join_rows<T: Copy>(a: &[T], b: &[T], aw: usize, bw: usize, h: usize) -> Option<Vec<T>> {
    let mut out = Vec::with_capacity((aw + bw) * h);
    for y in 0..h {
        out.extend_from_slice(a.get(y * aw..(y + 1) * aw)?);
        out.extend_from_slice(b.get(y * bw..(y + 1) * bw)?);
    }
    Some(out)
}

/// Samples `x0..x0 + n` of each of the `h` rows (`w` samples each) of `s`.
fn take_columns<T: Copy>(s: &[T], w: usize, h: usize, x0: usize, n: usize) -> Option<Vec<T>> {
    let mut out = Vec::with_capacity(n * h);
    for y in 0..h {
        out.extend_from_slice(s.get(y * w + x0..y * w + x0 + n)?);
    }
    Some(out)
}

/// A deep side-by-side pair from two eyes of the same height and space.
pub fn deep_side_by_side(a: &DeepImage, b: &DeepImage) -> Option<DeepImage> {
    if a.height != b.height || a.space != b.space {
        return None;
    }
    let (aw, bw, h) = (a.width * 3, b.width * 3, a.height);
    let samples = match (&a.samples, &b.samples) {
        (DeepSamples::U16(sa), DeepSamples::U16(sb)) => DeepSamples::U16(join_rows(sa, sb, aw, bw, h)?),
        (DeepSamples::F32(sa), DeepSamples::F32(sb)) => DeepSamples::F32(join_rows(sa, sb, aw, bw, h)?),
        _ => return None,
    };
    Some(DeepImage { width: a.width + b.width, height: h, space: a.space, samples })
}

/// The eyes of a deep side-by-side render (left, right), for finishing each eye of an export.
pub fn deep_halves(pair: &DeepImage) -> Option<(DeepImage, DeepImage)> {
    let eye = pair.width / 2;
    let (w, h) = (pair.width * 3, pair.height);
    let pick = |x0: usize| -> Option<DeepSamples> {
        Some(match &pair.samples {
            DeepSamples::U16(s) => DeepSamples::U16(take_columns(s, w, h, x0 * 3, eye * 3)?),
            DeepSamples::F32(s) => DeepSamples::F32(take_columns(s, w, h, x0 * 3, eye * 3)?),
        })
    };
    let eye_img = |samples| DeepImage { width: eye, height: h, space: pair.space, samples };
    Some((eye_img(pick(0)?), eye_img(pick(eye)?)))
}

/// The render of both eyes, full side by side, from each eye's render.
pub fn compose(left: Rendered, right: Rendered) -> Rendered {
    let image = side_by_side(&left.image, &right.image);
    let deep = match (&left.deep, &right.deep) {
        (Some(a), Some(b)) => deep_side_by_side(a, b),
        _ => None,
    };
    let histogram = Histogram::of_srgb8(&image);
    Rendered { image, histogram, deep }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_raster::Rgba8;

    #[test]
    fn pairs_split_and_join() {
        let a = Rgba8::filled(3, 2, [1, 2, 3, 4]);
        let b = Rgba8::filled(3, 2, [5, 6, 7, 8]);
        let p = side_by_side(&a, &b);
        assert_eq!((p.width, p.height), (6, 2));
        assert_eq!(p.get(2, 1), [1, 2, 3, 4]);
        assert_eq!(p.get(3, 0), [5, 6, 7, 8]);
        let (l, r) = halves(&p);
        assert_eq!((l, r), (a, b));
    }

    #[test]
    fn deep_pairs_split_and_join() {
        let eye = |v: u16| DeepImage { width: 2, height: 2, space: lightcraft_pipeline::OutputSpace::Srgb, samples: DeepSamples::U16(vec![v; 12]) };
        let p = deep_side_by_side(&eye(1), &eye(9)).unwrap();
        assert_eq!(p.width, 4);
        let DeepSamples::U16(s) = &p.samples else { panic!() };
        assert_eq!(&s[..12], &[1, 1, 1, 1, 1, 1, 9, 9, 9, 9, 9, 9]);
        let (l, r) = deep_halves(&p).unwrap();
        assert_eq!((l, r), (eye(1), eye(9)));
    }
}
