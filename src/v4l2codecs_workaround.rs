//! Workaround for a `GStreamer` `v4l2codecs` (gst-plugins-bad) bug on the
//! Raspberry Pi 4 HEVC decoder.
//!
//! `v4l2slh265dec` on `rpi-hevc-dec` keeps its capture queue on the
//! two-plane `Nc12` format (separate Y and UV planes with column heights `H`
//! and `H/2`) but advertises the output as `DMA_DRM`
//! `NV12:0x0700000000000004`, the single-plane Broadcom SAND128 layout with
//! column height 0. GPU importers follow the modifier and read the wrong
//! chroma, so colors are scrambled.
//!
//! The `syncplayersanduntile` element defined here refuses that mislabelled
//! DMA-BUF output, which makes the decoder fall back to system-memory
//! `NV12_128C8`, and converts those column-tiled frames to linear `NV12` on
//! the CPU. Every other format passes through untouched. Remove the
//! workaround once the decoder is fixed upstream.

use anyhow::{Result, anyhow, bail, ensure};
use gst::glib;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_video::prelude::VideoFrameExt;
use std::sync::OnceLock;

/// Factory name of the in-process untiling element.
pub const ELEMENT_NAME: &str = "syncplayersanduntile";

/// Column-tiled caps format produced by the Raspberry Pi `GStreamer` builds.
pub const SAND_FORMAT: &str = "NV12_128C8";

/// Width in bytes of one SAND128 column.
pub const COLUMN_BYTES: usize = 128;

const LINEAR_FORMAT: &str = "NV12";
const DMA_DRM_FORMAT: &str = "DMA_DRM";
const DMABUF_FEATURE: &str = "memory:DMABuf";
const SYSTEM_MEMORY_FEATURE: &str = "memory:SystemMemory";
const BROADCOM_VENDOR: u64 = 0x07;
const BROADCOM_SAND128: u64 = 4;
/// Planes larger than this many bytes are untiled by two threads.
const PARALLEL_PLANE_BYTES: usize = 1920 * 1200;

/// Registers the untiling element with `GStreamer` for this process.
///
/// # Errors
///
/// Returns an error if `GStreamer` rejects the registration.
pub fn register() -> Result<()> {
    static REGISTERED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    REGISTERED
        .get_or_init(|| {
            gst::Element::register(
                None,
                ELEMENT_NAME,
                gst::Rank::NONE,
                SandUntile::static_type(),
            )
            .map_err(|error| error.to_string())
        })
        .clone()
        .map_err(|error| anyhow!("cannot register {ELEMENT_NAME}: {error}"))
}

/// Returns true for a DRM modifier in the Broadcom SAND128 family, whatever
/// its column-height parameter.
#[must_use]
pub fn is_sand128_modifier(modifier: u64) -> bool {
    modifier >> 56 == BROADCOM_VENDOR && modifier & 0xff == BROADCOM_SAND128
}

/// Returns true if a caps `drm-format` string such as
/// `NV12:0x0700000000000004` names a SAND128 modifier.
#[must_use]
pub fn is_sand128_drm_format(drm_format: &str) -> bool {
    drm_format
        .split_once(':')
        .and_then(|(_, modifier)| {
            let digits = modifier
                .strip_prefix("0x")
                .or_else(|| modifier.strip_prefix("0X"))
                .unwrap_or(modifier);
            u64::from_str_radix(digits, 16).ok()
        })
        .is_some_and(is_sand128_modifier)
}

/// Returns true if negotiated caps carry the mislabelled SAND128 DMA-BUF
/// output that this workaround exists for.
#[must_use]
pub fn caps_have_sand128_dmabuf(caps: &gst::CapsRef) -> bool {
    caps.iter().any(|structure| {
        drm_formats(structure)
            .iter()
            .any(|value| is_sand128_drm_format(value))
    })
}

/// Number of SAND128 columns needed for a row of `row_bytes` bytes.
#[must_use]
pub const fn column_count(row_bytes: usize) -> usize {
    row_bytes.div_ceil(COLUMN_BYTES)
}

/// Copies one SAND128 column-tiled plane into a linear destination.
///
/// The source holds `column_count(row_bytes)` columns stored one after the
/// other, each `COLUMN_BYTES` wide and `column_height` rows tall; only the
/// first `rows` rows of each column are visible. A final partial column is
/// copied up to `row_bytes`. Destination rows start every `dst_stride` bytes.
///
/// # Errors
///
/// Returns an error when the column height is smaller than the visible rows
/// or either buffer is too small for the described geometry.
pub fn untile_plane(
    src: &[u8],
    column_height: usize,
    row_bytes: usize,
    rows: usize,
    dst: &mut [u8],
    dst_stride: usize,
) -> Result<()> {
    untile_plane_threaded(src, column_height, row_bytes, rows, dst, dst_stride, 1)
}

fn untile_plane_threaded(
    src: &[u8],
    column_height: usize,
    row_bytes: usize,
    rows: usize,
    dst: &mut [u8],
    dst_stride: usize,
    threads: usize,
) -> Result<()> {
    if rows == 0 || row_bytes == 0 {
        return Ok(());
    }
    ensure!(
        column_height >= rows,
        "SAND128 column height {column_height} is smaller than the {rows} visible rows"
    );
    ensure!(
        dst_stride >= row_bytes,
        "destination stride {dst_stride} is smaller than the {row_bytes}-byte row"
    );
    let columns = column_count(row_bytes);
    let column_stride = COLUMN_BYTES * column_height;
    let last_width = row_bytes - (columns - 1) * COLUMN_BYTES;
    let src_needed = (columns - 1) * column_stride + (rows - 1) * COLUMN_BYTES + last_width;
    ensure!(
        src.len() >= src_needed,
        "SAND128 plane holds {} bytes; {src_needed} are needed",
        src.len()
    );
    let dst_needed = (rows - 1) * dst_stride + row_bytes;
    ensure!(
        dst.len() >= dst_needed,
        "linear plane holds {} bytes; {dst_needed} are needed",
        dst.len()
    );

    let dst = &mut dst[..dst_needed];
    let threads = threads.clamp(1, rows);
    if threads == 1 {
        copy_rows(src, column_stride, row_bytes, 0, rows, dst, dst_stride);
        return Ok(());
    }
    // Bands of whole destination rows are disjoint, so each thread owns its
    // slice; within a band, reads still walk each column sequentially.
    let band_rows = rows.div_ceil(threads);
    std::thread::scope(|scope| {
        let mut remaining = dst;
        let mut first_row = 0;
        while first_row < rows {
            let band = band_rows.min(rows - first_row);
            let split = (band * dst_stride).min(remaining.len());
            let (head, tail) = std::mem::take(&mut remaining).split_at_mut(split);
            remaining = tail;
            let start = first_row;
            scope.spawn(move || {
                copy_rows(src, column_stride, row_bytes, start, band, head, dst_stride);
            });
            first_row += band;
        }
    });
    Ok(())
}

/// Copies `rows` rows starting at `first_row` into `dst`, whose first byte is
/// that row's first pixel. Bounds were validated by the caller.
fn copy_rows(
    src: &[u8],
    column_stride: usize,
    row_bytes: usize,
    first_row: usize,
    rows: usize,
    dst: &mut [u8],
    dst_stride: usize,
) {
    for column in 0..column_count(row_bytes) {
        let x = column * COLUMN_BYTES;
        let width = COLUMN_BYTES.min(row_bytes - x);
        let column_start = column * column_stride + first_row * COLUMN_BYTES;
        for row in 0..rows {
            let from = column_start + row * COLUMN_BYTES;
            let to = row * dst_stride + x;
            dst[to..to + width].copy_from_slice(&src[from..from + width]);
        }
    }
}

/// Infers a plane's column height from the size of the memory holding it.
///
/// The decoder's column height is padded beyond the visible rows, and is not
/// described anywhere in the caps, so it is derived rather than assumed.
#[must_use]
pub const fn column_height_from_size(plane_bytes: usize, row_bytes: usize) -> usize {
    plane_bytes / (column_count(row_bytes) * COLUMN_BYTES)
}

fn structure_formats(structure: &gst::StructureRef) -> Option<Vec<String>> {
    if let Ok(format) = structure.get::<String>("format") {
        return Some(vec![format]);
    }
    structure.get::<gst::List>("format").ok().map(|list| {
        list.iter()
            .filter_map(|value| value.get::<String>().ok())
            .collect()
    })
}

fn drm_formats(structure: &gst::StructureRef) -> Vec<String> {
    if let Ok(format) = structure.get::<String>("drm-format") {
        return vec![format];
    }
    structure
        .get::<gst::List>("drm-format")
        .map(|list| {
            list.iter()
                .filter_map(|value| value.get::<String>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// True when the structure may carry `format`; an absent field accepts all.
fn accepts_format(structure: &gst::StructureRef, format: &str) -> bool {
    structure_formats(structure).is_none_or(|formats| formats.iter().any(|value| value == format))
}

fn set_string_field(structure: &mut gst::StructureRef, field: &str, values: &[String]) {
    match values {
        [single] => structure.set(field, single.as_str()),
        _ => structure.set(
            field,
            gst::List::new(values.iter().map(String::as_str).collect::<Vec<_>>()),
        ),
    }
}

/// Copy of `structure` without `format` among its formats, or `None` when
/// nothing else remains. An absent format field is left as it is.
fn without_format(structure: &gst::StructureRef, format: &str) -> Option<gst::Structure> {
    let mut copy = structure.to_owned();
    if let Some(formats) = structure_formats(structure) {
        let kept: Vec<String> = formats
            .into_iter()
            .filter(|value| value != format)
            .collect();
        if kept.is_empty() {
            return None;
        }
        set_string_field(&mut copy, "format", &kept);
    }
    Some(copy)
}

fn with_format(structure: &gst::StructureRef, format: &str) -> gst::Structure {
    let mut copy = structure.to_owned();
    copy.set("format", format);
    copy
}

fn is_dma_drm(structure: &gst::StructureRef, features: &gst::CapsFeaturesRef) -> bool {
    !features.is_any()
        && features.contains(DMABUF_FEATURE)
        && structure_formats(structure)
            .is_some_and(|formats| formats.iter().any(|value| value == DMA_DRM_FORMAT))
}

/// Copy of a `DMA_DRM` structure without SAND128 `drm-format` entries, or
/// `None` when none remain.
fn without_sand128_drm(structure: &gst::StructureRef) -> Option<gst::Structure> {
    let formats = drm_formats(structure);
    if formats.is_empty() {
        return Some(structure.to_owned());
    }
    let kept: Vec<String> = formats
        .into_iter()
        .filter(|value| !is_sand128_drm_format(value))
        .collect();
    if kept.is_empty() {
        return None;
    }
    let mut copy = structure.to_owned();
    set_string_field(&mut copy, "drm-format", &kept);
    Some(copy)
}

fn is_system_memory(features: &gst::CapsFeaturesRef) -> bool {
    features.is_any()
        || features.contains(SYSTEM_MEMORY_FEATURE)
        || !features
            .iter()
            .any(|feature| feature.starts_with("memory:"))
}

/// Computes the caps on the other side of the element.
///
/// Toward the source (`direction == Sink`), system-memory `NV12_128C8` turns
/// into `NV12`. Toward the decoder (`direction == Src`), every structure that
/// accepts system-memory `NV12` also accepts `NV12_128C8`, which then appears
/// nowhere else, so downstream elements that could take the tiled format
/// themselves (slowly) never receive it. In both directions SAND128 is
/// removed from `DMA_DRM` structures; everything else passes through.
#[must_use]
pub fn transform_caps(direction: gst::PadDirection, caps: &gst::Caps) -> gst::Caps {
    if caps.is_any() {
        return caps.clone();
    }
    let mut result = gst::Caps::new_empty();
    {
        let result = result.make_mut();
        for (structure, features) in caps.iter_with_features() {
            if is_dma_drm(structure, features) {
                if let Some(filtered) = without_sand128_drm(structure) {
                    result.append_structure_full(filtered, Some(features.to_owned()));
                }
                continue;
            }
            if is_system_memory(features) {
                let (from, to) = match direction {
                    gst::PadDirection::Sink => (SAND_FORMAT, LINEAR_FORMAT),
                    _ => (LINEAR_FORMAT, SAND_FORMAT),
                };
                if accepts_format(structure, from) {
                    let mut converted = with_format(structure, to);
                    converted.remove_field("drm-format");
                    result.append_structure_full(
                        converted,
                        Some(gst::CapsFeatures::new([SYSTEM_MEMORY_FEATURE])),
                    );
                }
            }
            if let Some(passthrough) = without_format(structure, SAND_FORMAT) {
                result.append_structure_full(passthrough, Some(features.to_owned()));
            }
        }
    }
    result
}

/// Linear output geometry selected during negotiation.
struct Untiling {
    width: usize,
    height: usize,
    output: gstreamer_video::VideoInfo,
}

impl Untiling {
    fn from_caps(incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<Option<Self>> {
        let input = incaps
            .structure(0)
            .ok_or_else(|| anyhow!("input caps are empty"))?;
        if input.get::<String>("format").ok().as_deref() != Some(SAND_FORMAT) {
            return Ok(None);
        }
        let dimension = |field: &str| -> Result<usize> {
            let value = input
                .get::<i32>(field)
                .map_err(|_| anyhow!("{SAND_FORMAT} caps have no fixed {field}"))?;
            usize::try_from(value)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| anyhow!("{SAND_FORMAT} caps have an invalid {field} {value}"))
        };
        let output = gstreamer_video::VideoInfo::from_caps(outcaps)
            .map_err(|error| anyhow!("unusable output caps {outcaps}: {error}"))?;
        if output.format() != gstreamer_video::VideoFormat::Nv12 {
            bail!("{SAND_FORMAT} input requires NV12 output, not {outcaps}");
        }
        Ok(Some(Self {
            width: dimension("width")?,
            height: dimension("height")?,
            output,
        }))
    }

    fn convert(&self, input: &gst::BufferRef, output: &mut gst::BufferRef) -> Result<()> {
        let chroma_rows = self.height.div_ceil(2);
        let chroma_bytes = self.width.div_ceil(2) * 2;
        let mut frame =
            gstreamer_video::VideoFrameRef::from_buffer_ref_writable(output, &self.output)
                .map_err(|_| anyhow!("cannot map the output frame for writing"))?;
        let strides = frame.plane_stride().to_vec();
        let stride = |plane: usize| {
            usize::try_from(strides[plane]).map_err(|_| anyhow!("negative output stride"))
        };
        let (luma_stride, chroma_stride) = (stride(0)?, stride(1)?);
        let threads = if self.width * self.height > PARALLEL_PLANE_BYTES {
            2
        } else {
            1
        };

        match input.n_memory() {
            2 => {
                let luma = input
                    .peek_memory(0)
                    .map_readable()
                    .map_err(|_| anyhow!("cannot map the luma plane"))?;
                let chroma = input
                    .peek_memory(1)
                    .map_readable()
                    .map_err(|_| anyhow!("cannot map the chroma plane"))?;
                untile_plane_threaded(
                    &luma,
                    column_height_from_size(luma.len(), self.width),
                    self.width,
                    self.height,
                    frame.plane_data_mut(0)?,
                    luma_stride,
                    threads,
                )?;
                untile_plane_threaded(
                    &chroma,
                    column_height_from_size(chroma.len(), chroma_bytes),
                    chroma_bytes,
                    chroma_rows,
                    frame.plane_data_mut(1)?,
                    chroma_stride,
                    threads,
                )?;
            }
            1 => {
                // Not observed on hardware: both planes in one memory at the
                // offsets in the video meta. A layout that is really the
                // interleaved single-plane NC12 yields a column height below
                // the visible rows and is rejected instead of drawn wrong.
                let meta = input.meta::<gstreamer_video::VideoMeta>().ok_or_else(|| {
                    anyhow!("single-memory {SAND_FORMAT} buffer has no video meta")
                })?;
                let offsets = meta.offset();
                ensure!(
                    offsets.len() >= 2 && offsets[1] > offsets[0],
                    "single-memory {SAND_FORMAT} buffer has unusable plane offsets {offsets:?}"
                );
                let (luma_offset, chroma_offset) = (offsets[0], offsets[1]);
                let memory = input
                    .map_readable()
                    .map_err(|_| anyhow!("cannot map the input frame"))?;
                ensure!(
                    chroma_offset <= memory.len(),
                    "chroma offset {chroma_offset} lies beyond the {}-byte buffer",
                    memory.len()
                );
                let luma = &memory[luma_offset..chroma_offset];
                let chroma = &memory[chroma_offset..];
                untile_plane_threaded(
                    luma,
                    column_height_from_size(luma.len(), self.width),
                    self.width,
                    self.height,
                    frame.plane_data_mut(0)?,
                    luma_stride,
                    threads,
                )?;
                untile_plane_threaded(
                    chroma,
                    column_height_from_size(chroma.len(), chroma_bytes),
                    chroma_bytes,
                    chroma_rows,
                    frame.plane_data_mut(1)?,
                    chroma_stride,
                    threads,
                )?;
            }
            count => bail!("{SAND_FORMAT} buffer has {count} memories; expected 1 or 2"),
        }
        Ok(())
    }
}

mod imp {
    use super::{COLUMN_BYTES, SAND_FORMAT, Untiling, column_height_from_size, transform_caps};
    use gst::glib;
    use gstreamer as gst;
    use gstreamer_base as gst_base;
    use gstreamer_base::subclass::prelude::*;
    use std::sync::{LazyLock, Mutex};

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            super::ELEMENT_NAME,
            gst::DebugColorFlags::empty(),
            Some("Syncplayer v4l2codecs SAND128 workaround"),
        )
    });

    #[derive(Default)]
    pub struct SandUntile {
        state: Mutex<Option<Untiling>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SandUntile {
        const NAME: &'static str = "SyncplayerSandUntile";
        type Type = super::SandUntile;
        type ParentType = gst_base::BaseTransform;
    }

    impl ObjectImpl for SandUntile {}
    impl GstObjectImpl for SandUntile {}

    impl ElementImpl for SandUntile {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "Syncplayer SAND128 untiler (v4l2codecs workaround)",
                    "Filter/Converter/Video",
                    "Works around mislabelled Raspberry Pi HEVC decoder output by converting \
                     NV12_128C8 to linear NV12 and refusing SAND128 DMA-BUF caps",
                    "Syncplayer",
                )
            });
            Some(&*METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let caps = gst::Caps::new_any();
                vec![
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .expect("valid src template"),
                    gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .expect("valid sink template"),
                ]
            });
            TEMPLATES.as_ref()
        }
    }

    impl BaseTransformImpl for SandUntile {
        const MODE: gst_base::subclass::BaseTransformMode =
            gst_base::subclass::BaseTransformMode::NeverInPlace;
        const PASSTHROUGH_ON_SAME_CAPS: bool = true;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

        fn transform_caps(
            &self,
            direction: gst::PadDirection,
            caps: &gst::Caps,
            filter: Option<&gst::Caps>,
        ) -> Option<gst::Caps> {
            let transformed = transform_caps(direction, caps);
            gst::trace!(CAT, imp = self, "{direction:?} {caps} -> {transformed}");
            Some(match filter {
                Some(filter) => {
                    filter.intersect_with_mode(&transformed, gst::CapsIntersectMode::First)
                }
                None => transformed,
            })
        }

        fn set_caps(
            &self,
            incaps: &gst::Caps,
            outcaps: &gst::Caps,
        ) -> Result<(), gst::LoggableError> {
            let untiling = Untiling::from_caps(incaps, outcaps)
                .map_err(|error| gst::loggable_error!(CAT, "{error:#}"))?;
            if let Some(untiling) = &untiling {
                gst::info!(
                    CAT,
                    imp = self,
                    "untiling {}x{} {SAND_FORMAT} to NV12 on the CPU",
                    untiling.width,
                    untiling.height
                );
                tracing::info!(
                    width = untiling.width,
                    height = untiling.height,
                    "v4l2codecs SAND128 workaround active: untiling {SAND_FORMAT} frames to NV12 on the CPU"
                );
            } else {
                gst::debug!(CAT, imp = self, "passing {incaps} through");
            }
            *self.state.lock().expect("untile state lock") = untiling;
            Ok(())
        }

        fn transform_size(
            &self,
            _direction: gst::PadDirection,
            _caps: &gst::Caps,
            size: usize,
            _othercaps: &gst::Caps,
        ) -> Option<usize> {
            let state = self.state.lock().ok()?;
            Some(
                state
                    .as_ref()
                    .map_or(size, |untiling| untiling.output.size()),
            )
        }

        fn transform(
            &self,
            inbuf: &gst::Buffer,
            outbuf: &mut gst::BufferRef,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let state = self.state.lock().map_err(|_| gst::FlowError::Error)?;
            let Some(untiling) = state.as_ref() else {
                gst::element_imp_error!(
                    self,
                    gst::CoreError::Negotiation,
                    ("received a buffer to convert without {} caps", SAND_FORMAT)
                );
                return Err(gst::FlowError::NotNegotiated);
            };
            untiling.convert(inbuf, outbuf).map_err(|error| {
                let geometry = (0..inbuf.n_memory())
                    .map(|index| {
                        let size = inbuf.peek_memory(index).size();
                        format!(
                            "memory {index}: {size} bytes, column height {}",
                            column_height_from_size(size, untiling.width)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                gst::element_imp_error!(
                    self,
                    gst::StreamError::Format,
                    ("cannot untile {} frame: {:#}", SAND_FORMAT, error),
                    [
                        "{}x{}, {}-byte columns; {}",
                        untiling.width,
                        untiling.height,
                        COLUMN_BYTES,
                        geometry
                    ]
                );
                gst::FlowError::Error
            })?;
            Ok(gst::FlowSuccess::Ok)
        }
    }
}

glib::wrapper! {
    /// In-process `BaseTransform` that untiles Raspberry Pi SAND128 video.
    pub struct SandUntile(ObjectSubclass<imp::SandUntile>)
        @extends gstreamer_base::BaseTransform, gst::Element, gst::Object;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a tiled plane whose visible bytes encode their linear position.
    fn tiled(row_bytes: usize, rows: usize, column_height: usize) -> (Vec<u8>, Vec<u8>) {
        let columns = column_count(row_bytes);
        let mut src = vec![0xEE; columns * COLUMN_BYTES * column_height];
        let mut expected = vec![0; row_bytes * rows];
        for row in 0..rows {
            for x in 0..row_bytes {
                #[allow(clippy::cast_possible_truncation)]
                let value = (row * 7 + x * 13 + x / COLUMN_BYTES) as u8;
                expected[row * row_bytes + x] = value;
                let column = x / COLUMN_BYTES;
                src[column * COLUMN_BYTES * column_height
                    + row * COLUMN_BYTES
                    + x % COLUMN_BYTES] = value;
            }
        }
        (src, expected)
    }

    #[test]
    fn untiles_full_and_partial_columns_with_padded_heights() {
        for (row_bytes, rows, column_height) in [
            (256, 4, 4),
            (300, 5, 8),
            (128, 1, 1),
            (1, 3, 3),
            (1920, 12, 16),
        ] {
            let (src, expected) = tiled(row_bytes, rows, column_height);
            let mut dst = vec![0; row_bytes * rows];
            untile_plane(&src, column_height, row_bytes, rows, &mut dst, row_bytes).unwrap();
            assert_eq!(
                dst, expected,
                "{row_bytes}x{rows} in columns of {column_height}"
            );
        }
    }

    #[test]
    fn untiles_into_a_padded_stride_without_touching_padding() {
        let (src, expected) = tiled(200, 3, 4);
        let stride = 208;
        let mut dst = vec![0xAA; stride * 3];
        untile_plane(&src, 4, 200, 3, &mut dst, stride).unwrap();
        for row in 0..3 {
            assert_eq!(
                &dst[row * stride..row * stride + 200],
                &expected[row * 200..(row + 1) * 200]
            );
            assert!(
                dst[row * stride + 200..(row + 1) * stride]
                    .iter()
                    .all(|b| *b == 0xAA)
            );
        }
    }

    #[test]
    fn threaded_untile_matches_single_threaded() {
        let (src, expected) = tiled(1000, 37, 40);
        for threads in [2, 3, 37, 64] {
            let mut dst = vec![0; 1000 * 37];
            untile_plane_threaded(&src, 40, 1000, 37, &mut dst, 1000, threads).unwrap();
            assert_eq!(dst, expected, "{threads} threads");
        }
    }

    #[test]
    fn rejects_inconsistent_geometry() {
        let (src, _) = tiled(256, 4, 4);
        let mut dst = vec![0; 256 * 4];
        assert!(untile_plane(&src, 3, 256, 4, &mut dst, 256).is_err());
        assert!(untile_plane(&src[..src.len() - 1], 4, 256, 4, &mut dst, 256).is_err());
        assert!(untile_plane(&src, 4, 256, 4, &mut dst[..100], 256).is_err());
        assert!(untile_plane(&src, 4, 256, 4, &mut dst, 255).is_err());
    }

    #[test]
    fn derives_the_observed_raspberry_pi_column_heights() {
        // Sizes reported by v4l2slh265dec for 1920x1200 on rpi-hevc-dec.
        assert_eq!(column_height_from_size(2_304_000, 1920), 1200);
        assert_eq!(column_height_from_size(1_152_000, 1920), 600);
    }

    #[test]
    fn recognizes_sand128_modifiers() {
        assert!(is_sand128_drm_format("NV12:0x0700000000000004"));
        assert!(is_sand128_drm_format("NV12:0x070000000004b004"));
        assert!(!is_sand128_drm_format("NV12"));
        assert!(!is_sand128_drm_format("NV12:0x0700000000000006"));
        assert!(!is_sand128_drm_format("NV12:0x0100000000000001"));
        assert!(!is_sand128_drm_format("NV12:garbage"));
    }

    fn caps(text: &str) -> gst::Caps {
        gst::init().unwrap();
        text.parse().unwrap()
    }

    #[test]
    fn decoder_side_refuses_sand128_dmabuf_and_accepts_tiled_system_memory() {
        let downstream = caps(
            "video/x-raw(memory:DMABuf), format=DMA_DRM, \
               drm-format={ NV12:0x0700000000000004, NV12, YU12 }; \
             video/x-raw(memory:DMABuf), format=DMA_DRM, drm-format=NV12:0x0700000000000004; \
             video/x-raw(memory:DMABuf), format=I420; \
             video/x-raw, format={ NV12_128C8, NV12, I420 }, width=1920, height=1200",
        );
        let upstream = transform_caps(gst::PadDirection::Src, &downstream);
        let expected = caps(
            "video/x-raw(memory:DMABuf), format=DMA_DRM, drm-format={ NV12, YU12 }; \
             video/x-raw(memory:DMABuf), format=I420; \
             video/x-raw, format=NV12_128C8, width=1920, height=1200; \
             video/x-raw, format={ NV12, I420 }, width=1920, height=1200",
        );
        assert!(upstream.is_strictly_equal(&expected), "{upstream}");
        assert!(!caps_have_sand128_dmabuf(&upstream));
    }

    #[test]
    fn source_side_turns_tiled_system_memory_into_nv12() {
        let upstream = caps(
            "video/x-raw(memory:DMABuf), format=DMA_DRM, drm-format=NV12:0x0700000000000004; \
             video/x-raw, format=NV12_128C8, width=1920, height=1200, framerate=25/1",
        );
        let downstream = transform_caps(gst::PadDirection::Sink, &upstream);
        let expected = caps("video/x-raw, format=NV12, width=1920, height=1200, framerate=25/1");
        assert!(downstream.is_strictly_equal(&expected), "{downstream}");
    }

    #[test]
    fn other_formats_and_memories_pass_through() {
        let h264 = caps(
            "video/x-raw(memory:DMABuf, meta:GstVideoOverlayComposition), format=I420, \
             width=1920, height=1080",
        );
        assert!(transform_caps(gst::PadDirection::Sink, &h264).is_strictly_equal(&h264));
        assert!(transform_caps(gst::PadDirection::Src, &h264).is_strictly_equal(&h264));
        let any = gst::Caps::new_any();
        assert!(transform_caps(gst::PadDirection::Src, &any).is_any());
    }

    #[test]
    fn detects_mislabelled_negotiated_caps() {
        assert!(caps_have_sand128_dmabuf(&caps(
            "video/x-raw(memory:DMABuf), format=DMA_DRM, width=1920, height=1200, \
             drm-format=NV12:0x0700000000000004"
        )));
        assert!(!caps_have_sand128_dmabuf(&caps(
            "video/x-raw, format=NV12, width=1920, height=1200"
        )));
    }

    #[test]
    fn element_registers_and_untiles_a_buffer() {
        gst::init().unwrap();
        register().unwrap();
        let element = gst::ElementFactory::make(ELEMENT_NAME).build().unwrap();
        assert!(element.is::<SandUntile>());

        let (width, height) = (200_usize, 6_usize);
        let (luma_src, luma) = tiled(width, height, 8);
        let (chroma_src, chroma) = tiled(width, height / 2, 4);
        let mut input = gst::Buffer::new();
        {
            let input = input.get_mut().unwrap();
            input.append_memory(gst::Memory::from_slice(luma_src));
            input.append_memory(gst::Memory::from_slice(chroma_src));
        }
        let incaps = caps("video/x-raw, format=NV12_128C8, width=200, height=6, framerate=25/1");
        let outcaps = caps("video/x-raw, format=NV12, width=200, height=6, framerate=25/1");
        let untiling = Untiling::from_caps(&incaps, &outcaps).unwrap().unwrap();
        let mut output = gst::Buffer::with_size(untiling.output.size()).unwrap();
        untiling.convert(&input, output.get_mut().unwrap()).unwrap();

        let map = output.map_readable().unwrap();
        let info = &untiling.output;
        let stride = |plane: usize| usize::try_from(info.stride()[plane]).unwrap();
        for row in 0..height {
            let start = info.offset()[0] + row * stride(0);
            assert_eq!(
                &map[start..start + width],
                &luma[row * width..(row + 1) * width]
            );
        }
        for row in 0..height / 2 {
            let start = info.offset()[1] + row * stride(1);
            assert_eq!(
                &map[start..start + width],
                &chroma[row * width..(row + 1) * width]
            );
        }
        assert!(Untiling::from_caps(&outcaps, &outcaps).unwrap().is_none());
    }

    /// Pushes `input` through the element between test pads and returns
    /// what reaches a downstream pad that only accepts `downstream_caps`.
    fn push_through(
        upstream_caps: &gst::Caps,
        downstream_caps: &gst::Caps,
        input: gst::Buffer,
    ) -> (gst::Caps, gst::Buffer) {
        use std::sync::{Arc, Mutex};
        register().unwrap();
        let element = gst::ElementFactory::make(ELEMENT_NAME).build().unwrap();
        let received = Arc::new(Mutex::new(None));
        let template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            downstream_caps,
        )
        .unwrap();
        let sink_store = received.clone();
        let sink = gst::Pad::builder_from_template(&template)
            .chain_function(move |_, _, buffer| {
                *sink_store.lock().unwrap() = Some(buffer);
                Ok(gst::FlowSuccess::Ok)
            })
            .build();
        sink.set_active(true).unwrap();
        element.static_pad("src").unwrap().link(&sink).unwrap();
        let src = gst::Pad::builder(gst::PadDirection::Src).build();
        src.set_active(true).unwrap();
        src.link(&element.static_pad("sink").unwrap()).unwrap();
        element.set_state(gst::State::Playing).unwrap();

        assert!(src.push_event(gst::event::StreamStart::new("untile-test")));
        assert!(src.push_event(gst::event::Caps::new(upstream_caps)));
        assert!(
            src.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                gst::ClockTime,
            >::new()))
        );
        src.push(input).unwrap();
        element.set_state(gst::State::Null).unwrap();
        let caps = sink.current_caps().unwrap();
        let buffer = received.lock().unwrap().take().unwrap();
        (caps, buffer)
    }

    #[test]
    fn element_negotiates_nv12_and_untiles_in_a_pipeline() {
        gst::init().unwrap();
        let (width, height) = (300_usize, 4_usize);
        let (luma_src, luma) = tiled(width, height, 6);
        let (chroma_src, chroma) = tiled(width, height / 2, 3);
        let mut input = gst::Buffer::new();
        {
            let input = input.get_mut().unwrap();
            input.append_memory(gst::Memory::from_slice(luma_src));
            input.append_memory(gst::Memory::from_slice(chroma_src));
            input.set_pts(gst::ClockTime::from_mseconds(40));
        }
        let (caps, output) = push_through(
            &caps("video/x-raw, format=NV12_128C8, width=300, height=4, framerate=25/1"),
            &caps("video/x-raw, format={ NV12_128C8, NV12, I420 }"),
            input,
        );
        assert_eq!(
            caps.structure(0).unwrap().get::<&str>("format").unwrap(),
            "NV12"
        );
        assert_eq!(output.pts(), Some(gst::ClockTime::from_mseconds(40)));
        let info = gstreamer_video::VideoInfo::from_caps(&caps).unwrap();
        let map = output.map_readable().unwrap();
        let stride = |plane: usize| usize::try_from(info.stride()[plane]).unwrap();
        for row in 0..height {
            let start = info.offset()[0] + row * stride(0);
            assert_eq!(
                &map[start..start + width],
                &luma[row * width..(row + 1) * width]
            );
        }
        for row in 0..height / 2 {
            let start = info.offset()[1] + row * stride(1);
            assert_eq!(
                &map[start..start + width],
                &chroma[row * width..(row + 1) * width]
            );
        }
    }

    #[test]
    fn element_passes_other_formats_through_unchanged() {
        gst::init().unwrap();
        let input = gst::Buffer::from_slice(vec![7_u8; 16 * 16 * 3 / 2]);
        let original = input.clone();
        let (caps, output) = push_through(
            &caps("video/x-raw, format=I420, width=16, height=16, framerate=25/1"),
            &caps("video/x-raw, format={ NV12, I420 }"),
            input,
        );
        assert_eq!(
            caps.structure(0).unwrap().get::<&str>("format").unwrap(),
            "I420"
        );
        assert_eq!(output.as_ptr(), original.as_ptr());
    }
}
