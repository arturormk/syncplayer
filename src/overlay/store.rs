//! Decoded overlay layers and the `GStreamer` overlay composition built from them.

use super::protocol::{Geometry, LayerInfo};
use gstreamer as gst;
use gstreamer_video as gst_video;
use std::collections::BTreeMap;
use std::fmt;
use std::io::Cursor;
use std::sync::Mutex;

/// Largest accepted decoded image width or height.
pub const MAX_IMAGE_DIMENSION: u32 = 4_096;
/// Largest number of simultaneous layers on one player.
pub const MAX_LAYERS: usize = 32;
/// Largest number of decoded pixels held by all layers together (128 MiB of RGBA). A layer drawn
/// below full opacity holds a second, faded copy of its pixels in addition to this budget.
pub const MAX_TOTAL_PIXELS: u64 = 32 * 1024 * 1024;

/// Pixel layout `GStreamer` expects for overlay rectangles: ARGB in native-endian 32-bit words.
const OVERLAY_FORMAT: gst_video::VideoFormat = if cfg!(target_endian = "little") {
    gst_video::VideoFormat::Bgra
} else {
    gst_video::VideoFormat::Argb
};

#[derive(Debug, Eq, PartialEq)]
pub enum StoreError {
    Png(String),
    ImageTooLarge { width: u32, height: u32 },
    TooManyLayers,
    PixelBudget,
    UnknownLayer(String),
    Gstreamer(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Png(error) => write!(f, "PNG could not be decoded: {error}"),
            Self::ImageTooLarge { width, height } => write!(
                f,
                "image is {width}x{height}; the limit is {MAX_IMAGE_DIMENSION}x{MAX_IMAGE_DIMENSION}"
            ),
            Self::TooManyLayers => write!(f, "a player holds at most {MAX_LAYERS} layers"),
            Self::PixelBudget => write!(
                f,
                "layers would exceed the player's budget of {MAX_TOTAL_PIXELS} decoded pixels"
            ),
            Self::UnknownLayer(id) => write!(f, "layer '{id}' does not exist"),
            Self::Gstreamer(error) => write!(f, "GStreamer rejected the layer: {error}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// An image converted to premultiplied-alpha pixels in `GStreamer` overlay byte order (ARGB in
/// native-endian 32-bit words).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Decodes PNG bytes of any color type and bit depth into overlay pixels.
///
/// # Errors
///
/// Returns an error for malformed PNG data or images larger than [`MAX_IMAGE_DIMENSION`].
pub fn decode_png(bytes: &[u8]) -> Result<DecodedImage, StoreError> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|error| StoreError::Png(error.to_string()))?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(StoreError::ImageTooLarge { width, height });
    }
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| StoreError::Png("image size overflows memory".to_owned()))?;
    let mut raw = vec![0; size];
    let frame = reader
        .next_frame(&mut raw)
        .map_err(|error| StoreError::Png(error.to_string()))?;
    let channels = match frame.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => {
            return Err(StoreError::Png("palette was not expanded".to_owned()));
        }
    };
    if frame.bit_depth != png::BitDepth::Eight {
        return Err(StoreError::Png(
            "samples were not normalized to 8 bits".to_owned(),
        ));
    }
    let row_pixels = usize::try_from(frame.width).unwrap_or(usize::MAX);
    let rows = usize::try_from(frame.height).unwrap_or(usize::MAX);
    let mut pixels = Vec::with_capacity(row_pixels * rows * 4);
    for row in raw.chunks(frame.line_size).take(rows) {
        for sample in row[..row_pixels * channels].chunks_exact(channels) {
            let (r, g, b, a) = match *sample {
                [v] => (v, v, v, 255),
                [v, a] => (v, v, v, a),
                [r, g, b] => (r, g, b, 255),
                [r, g, b, a] => (r, g, b, a),
                _ => unreachable!("chunks_exact yields whole pixels"),
            };
            push_overlay_pixel(&mut pixels, r, g, b, a);
        }
    }
    Ok(DecodedImage {
        width: frame.width,
        height: frame.height,
        pixels,
    })
}

fn premultiply(channel: u8, alpha: u8) -> u8 {
    let value = (u16::from(channel) * u16::from(alpha) + 127) / 255;
    u8::try_from(value).expect("a premultiplied 8-bit channel fits in a byte")
}

fn push_overlay_pixel(out: &mut Vec<u8>, r: u8, g: u8, b: u8, a: u8) {
    let (r, g, b) = (premultiply(r, a), premultiply(g, a), premultiply(b, a));
    if cfg!(target_endian = "little") {
        out.extend_from_slice(&[b, g, r, a]);
    } else {
        out.extend_from_slice(&[a, r, g, b]);
    }
}

struct Layer {
    geometry: Geometry,
    image_width: u32,
    image_height: u32,
    /// Fully opaque decoded pixels as received.
    source: gst::Buffer,
    /// Pixels drawn for the current opacity; shares `source` while the layer is fully opaque.
    pixels: gst::Buffer,
    rectangle: gst_video::VideoOverlayRectangle,
}

#[derive(Default)]
struct Inner {
    layers: BTreeMap<String, Layer>,
    composition: Option<gst_video::VideoOverlayComposition>,
    total_pixels: u64,
}

/// The layers currently shown by one player.
///
/// Every change rebuilds one immutable composition. The streaming thread only clones that
/// reference, so frames always see either the previous or the new complete layer set. Unchanged
/// layers keep their rectangle objects, which lets GPU sinks keep their uploaded textures.
#[derive(Default)]
pub struct LayerStore {
    inner: Mutex<Inner>,
}

impl LayerStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the composition to draw, or `None` when there are no layers.
    #[must_use]
    pub fn composition(&self) -> Option<gst_video::VideoOverlayComposition> {
        self.lock().composition.clone()
    }

    /// Creates or replaces a layer. A zero-sized geometry renders the image at its own size.
    ///
    /// # Errors
    ///
    /// Returns an error when a limit would be exceeded or `GStreamer` rejects the pixels.
    ///
    /// # Panics
    ///
    /// Does not panic in practice: freshly created buffers are always writable.
    pub fn put(&self, id: &str, geometry: Geometry, image: DecodedImage) -> Result<(), StoreError> {
        let geometry = if geometry.uses_natural_size() {
            Geometry {
                width: image.width,
                height: image.height,
                ..geometry
            }
        } else {
            geometry
        };
        let new_pixels = u64::from(image.width) * u64::from(image.height);
        let mut inner = self.lock();
        let old_pixels = inner.layers.get(id).map_or(0, |layer| {
            u64::from(layer.image_width) * u64::from(layer.image_height)
        });
        if old_pixels == 0 && inner.layers.len() >= MAX_LAYERS {
            return Err(StoreError::TooManyLayers);
        }
        let total = inner.total_pixels - old_pixels + new_pixels;
        if total > MAX_TOTAL_PIXELS {
            return Err(StoreError::PixelBudget);
        }
        let source = overlay_buffer(image.pixels, image.width, image.height)?;
        let pixels = faded(&source, image.width, image.height, geometry.opacity)?;
        let rectangle = make_rectangle(&pixels, &geometry);
        inner.layers.insert(
            id.to_owned(),
            Layer {
                geometry,
                image_width: image.width,
                image_height: image.height,
                source,
                pixels,
                rectangle,
            },
        );
        inner.total_pixels = total;
        rebuild(&mut inner)
    }

    /// Changes a layer's geometry, stacking, and opacity while keeping its pixels.
    ///
    /// # Errors
    ///
    /// Returns an error when the layer does not exist.
    pub fn move_layer(&self, id: &str, geometry: Geometry) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let layer = inner
            .layers
            .get_mut(id)
            .ok_or_else(|| StoreError::UnknownLayer(id.to_owned()))?;
        if geometry.opacity != layer.geometry.opacity {
            layer.pixels = faded(
                &layer.source,
                layer.image_width,
                layer.image_height,
                geometry.opacity,
            )?;
        }
        layer.geometry = geometry;
        layer.rectangle = make_rectangle(&layer.pixels, &geometry);
        rebuild(&mut inner)
    }

    /// Removes one layer.
    ///
    /// # Errors
    ///
    /// Returns an error when the layer does not exist.
    pub fn delete(&self, id: &str) -> Result<(), StoreError> {
        let mut inner = self.lock();
        let layer = inner
            .layers
            .remove(id)
            .ok_or_else(|| StoreError::UnknownLayer(id.to_owned()))?;
        inner.total_pixels -= u64::from(layer.image_width) * u64::from(layer.image_height);
        rebuild(&mut inner)
    }

    /// Removes every layer.
    pub fn clear(&self) {
        let mut inner = self.lock();
        *inner = Inner::default();
    }

    /// Describes the layers in drawing order, bottom first.
    #[must_use]
    pub fn list(&self) -> Vec<LayerInfo> {
        let inner = self.lock();
        drawing_order(&inner)
            .into_iter()
            .map(|(id, layer)| LayerInfo {
                id: id.clone(),
                geometry: layer.geometry,
                image_width: layer.image_width,
                image_height: layer.image_height,
            })
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock cannot leave the map half-updated in a way that matters
        // for drawing, so keep serving the last state rather than losing the overlay.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Wraps overlay pixels in a buffer carrying the video meta `GStreamer` requires.
fn overlay_buffer(pixels: Vec<u8>, width: u32, height: u32) -> Result<gst::Buffer, StoreError> {
    let mut buffer = gst::Buffer::from_mut_slice(pixels);
    gst_video::VideoMeta::add(
        buffer
            .get_mut()
            .expect("a newly created buffer is writable"),
        gst_video::VideoFrameFlags::empty(),
        OVERLAY_FORMAT,
        width,
        height,
    )
    .map_err(|error| StoreError::Gstreamer(error.to_string()))?;
    Ok(buffer)
}

/// Returns `source` scaled by a whole-layer opacity.
///
/// Opacity is baked into a separate copy instead of using the rectangle's global alpha:
/// `GStreamer` applies global alpha by rewriting the pixels of a rectangle that needs no scaling or
/// conversion in place, which fails on shared rectangles and would corrupt `source`.
fn faded(
    source: &gst::Buffer,
    width: u32,
    height: u32,
    opacity: u8,
) -> Result<gst::Buffer, StoreError> {
    if opacity == u8::MAX {
        return Ok(source.clone());
    }
    let map = source
        .map_readable()
        .map_err(|error| StoreError::Gstreamer(error.to_string()))?;
    // Every channel is premultiplied, so scaling all four keeps the pixels consistent.
    let pixels = map
        .iter()
        .map(|&channel| premultiply(channel, opacity))
        .collect();
    overlay_buffer(pixels, width, height)
}

fn make_rectangle(pixels: &gst::Buffer, geometry: &Geometry) -> gst_video::VideoOverlayRectangle {
    gst_video::VideoOverlayRectangle::new_raw(
        pixels,
        geometry.x,
        geometry.y,
        geometry.width,
        geometry.height,
        gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
    )
}

fn drawing_order(inner: &Inner) -> Vec<(&String, &Layer)> {
    let mut layers: Vec<_> = inner.layers.iter().collect();
    // BTreeMap iteration is already id-ordered, and the sort is stable.
    layers.sort_by_key(|(_, layer)| layer.geometry.z);
    layers
}

fn rebuild(inner: &mut Inner) -> Result<(), StoreError> {
    let rectangles: Vec<_> = drawing_order(inner)
        .into_iter()
        .map(|(_, layer)| layer.rectangle.clone())
        .collect();
    inner.composition = if rectangles.is_empty() {
        None
    } else {
        Some(
            gst_video::VideoOverlayComposition::new(&rectangles)
                .map_err(|error| StoreError::Gstreamer(error.to_string()))?,
        )
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_png(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    }

    fn native(b: u8, g: u8, r: u8, a: u8) -> [u8; 4] {
        if cfg!(target_endian = "little") {
            [b, g, r, a]
        } else {
            [a, r, g, b]
        }
    }

    fn image(width: u32, height: u32) -> DecodedImage {
        DecodedImage {
            width,
            height,
            pixels: vec![0; usize::try_from(width * height * 4).unwrap()],
        }
    }

    fn geometry(z: i16) -> Geometry {
        Geometry {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            z,
            opacity: 255,
        }
    }

    #[test]
    fn decodes_rgba_with_premultiplied_alpha() {
        let png = encode_png(
            2,
            1,
            png::ColorType::Rgba,
            &[255, 0, 0, 255, 0, 255, 0, 128],
        );
        let decoded = decode_png(&png).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        let mut expected = native(0, 0, 255, 255).to_vec();
        expected.extend(native(0, 128, 0, 128));
        assert_eq!(decoded.pixels, expected);
    }

    #[test]
    fn decodes_rgb_and_grayscale_as_opaque() {
        let rgb = decode_png(&encode_png(1, 1, png::ColorType::Rgb, &[10, 20, 30])).unwrap();
        assert_eq!(rgb.pixels, native(30, 20, 10, 255));
        let gray = decode_png(&encode_png(1, 1, png::ColorType::Grayscale, &[77])).unwrap();
        assert_eq!(gray.pixels, native(77, 77, 77, 255));
        let gray_alpha =
            decode_png(&encode_png(1, 1, png::ColorType::GrayscaleAlpha, &[200, 0])).unwrap();
        assert_eq!(gray_alpha.pixels, native(0, 0, 0, 0));
    }

    #[test]
    fn rejects_garbage_and_oversized_images() {
        assert!(matches!(decode_png(b"not a png"), Err(StoreError::Png(_))));
        let wide = encode_png(
            MAX_IMAGE_DIMENSION + 1,
            1,
            png::ColorType::Grayscale,
            &vec![0; usize::try_from(MAX_IMAGE_DIMENSION + 1).unwrap()],
        );
        assert!(matches!(
            decode_png(&wide),
            Err(StoreError::ImageTooLarge { .. })
        ));
    }

    #[test]
    fn layers_are_listed_and_composed_in_z_order() {
        gst::init().unwrap();
        let store = LayerStore::new();
        assert!(store.composition().is_none());
        store.put("top", geometry(5), image(4, 2)).unwrap();
        store.put("bottom", geometry(-1), image(2, 2)).unwrap();
        store.put("middle", geometry(0), image(2, 2)).unwrap();
        let ids: Vec<_> = store.list().into_iter().map(|layer| layer.id).collect();
        assert_eq!(ids, ["bottom", "middle", "top"]);
        assert_eq!(store.composition().unwrap().n_rectangles(), 3);
        let top = store.list().pop().unwrap();
        assert_eq!((top.geometry.width, top.geometry.height), (4, 2));
    }

    #[test]
    fn move_keeps_pixels_and_unchanged_rectangles() {
        gst::init().unwrap();
        let store = LayerStore::new();
        store.put("a", geometry(0), image(2, 2)).unwrap();
        store.put("b", geometry(1), image(2, 2)).unwrap();
        let before = store.composition().unwrap();
        let moved = Geometry {
            x: 100,
            y: -50,
            width: 400,
            height: 300,
            z: 1,
            opacity: 128,
        };
        store.move_layer("b", moved).unwrap();
        let after = store.composition().unwrap();
        let untouched_before = before.rectangle(0).unwrap();
        let untouched_after = after.rectangle(0).unwrap();
        assert_eq!(untouched_before.seqnum(), untouched_after.seqnum());
        let moved_rectangle = after.rectangle(1).unwrap();
        assert_eq!(moved_rectangle.render_rectangle(), (100, -50, 400, 300));
        assert!((moved_rectangle.global_alpha() - 1.0).abs() < f32::EPSILON);
        let listed = store.list().pop().unwrap();
        assert_eq!(listed.geometry, moved);
        assert_eq!((listed.image_width, listed.image_height), (2, 2));
        assert_eq!(
            store.move_layer("missing", moved),
            Err(StoreError::UnknownLayer("missing".to_owned()))
        );
    }

    #[test]
    fn opacity_is_baked_into_a_copy_and_never_compounds() {
        gst::init().unwrap();
        let store = LayerStore::new();
        let opaque_white = native(255, 255, 255, 255).to_vec();
        store
            .put(
                "a",
                geometry(0),
                DecodedImage {
                    width: 1,
                    height: 1,
                    pixels: opaque_white.clone(),
                },
            )
            .unwrap();
        let first_pixels = |store: &LayerStore| {
            let rectangle = store.composition().unwrap().rectangle(0).unwrap();
            let flags = gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA;
            rectangle
                .pixels_unscaled_raw(flags)
                .map_readable()
                .unwrap()
                .to_vec()
        };
        let mut half = store.list()[0].geometry;
        half.opacity = 128;
        store.move_layer("a", half).unwrap();
        assert_eq!(first_pixels(&store), [128, 128, 128, 128]);
        half.x = 10;
        half.opacity = 64;
        store.move_layer("a", half).unwrap();
        assert_eq!(first_pixels(&store), [64, 64, 64, 64]);
        half.opacity = 255;
        store.move_layer("a", half).unwrap();
        assert_eq!(first_pixels(&store), opaque_white);
    }

    #[test]
    fn delete_and_clear_release_layers() {
        gst::init().unwrap();
        let store = LayerStore::new();
        store.put("a", geometry(0), image(2, 2)).unwrap();
        store.put("b", geometry(0), image(2, 2)).unwrap();
        store.delete("a").unwrap();
        assert_eq!(store.list().len(), 1);
        assert!(store.delete("a").is_err());
        store.clear();
        assert!(store.list().is_empty());
        assert!(store.composition().is_none());
    }

    #[test]
    fn enforces_layer_count_and_pixel_budget() {
        gst::init().unwrap();
        let store = LayerStore::new();
        for index in 0..MAX_LAYERS {
            store
                .put(&format!("layer-{index}"), geometry(0), image(1, 1))
                .unwrap();
        }
        assert_eq!(
            store.put("one-too-many", geometry(0), image(1, 1)),
            Err(StoreError::TooManyLayers)
        );
        // Replacing an existing layer is still allowed at the count limit.
        store.put("layer-0", geometry(0), image(2, 1)).unwrap();

        let store = LayerStore::new();
        let side = MAX_IMAGE_DIMENSION;
        let per_layer = u64::from(side) * u64::from(side);
        let fitting = usize::try_from(MAX_TOTAL_PIXELS / per_layer).unwrap();
        for index in 0..fitting {
            store
                .put(&format!("big-{index}"), geometry(0), image(side, side))
                .unwrap();
        }
        assert_eq!(
            store.put("over", geometry(0), image(side, side)),
            Err(StoreError::PixelBudget)
        );
    }
}
