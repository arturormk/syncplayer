//! Attaches overlay layers to the video stream around the diagnostic OSD element.
//!
//! The OSD `timeoverlay` either attaches its text as `GstVideoOverlayCompositionMeta` (when the
//! sink composites overlays itself, as `glimagesink` does on `--kms-gl`) or blends the text into
//! the frame. Layers must stay below the OSD in both cases:
//!
//! - When the OSD attaches metadata, layers are merged into its output buffers' composition on its
//!   source pad, layers first. Adding a meta before the OSD instead would leave two metas on the
//!   buffer, because `timeoverlay` merges an upstream composition into its own *and* keeps the
//!   original meta, so a compositing sink would draw the layers twice.
//! - When the OSD blends, layers are blended into the frame on its sink pad, before the text.

use super::store::LayerStore;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_video as gst_video;
use std::sync::{Arc, Mutex};

/// Installs the layer probes around `osd`, the diagnostic OSD overlay element.
///
/// # Errors
///
/// Returns an error when the element lacks the expected static pads.
pub fn attach_layers_around_osd(osd: &gst::Element, store: Arc<LayerStore>) -> Result<(), String> {
    let sink = osd
        .static_pad("video_sink")
        .or_else(|| osd.static_pad("sink"))
        .ok_or("the OSD element has no video sink pad")?;
    let src = osd
        .static_pad("src")
        .ok_or("the OSD element has no source pad")?;

    let blend_store = store.clone();
    let blend_src = src.clone();
    let video_info: Mutex<Option<(gst::Caps, gst_video::VideoInfo)>> = Mutex::new(None);
    sink.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if attaches_composition(&blend_src) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(composition) = blend_store.composition() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(info_for_caps) = current_video_info(pad, &video_info) else {
            return gst::PadProbeReturn::Ok;
        };
        if let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_mut() {
            let buffer = buffer.make_mut();
            match gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, &info_for_caps) {
                Ok(mut frame) => {
                    if let Err(error) = composition.blend(&mut frame) {
                        gst::warning!(gst::CAT_DEFAULT, "overlay layers not blended: {error}");
                    }
                }
                Err(error) => {
                    gst::warning!(gst::CAT_DEFAULT, "overlay frame not writable: {error}");
                }
            }
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or("could not install the overlay blend probe")?;

    src.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if !attaches_composition(pad) {
            return gst::PadProbeReturn::Ok;
        }
        let Some(layers) = store.composition() else {
            return gst::PadProbeReturn::Ok;
        };
        if let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_mut() {
            merge_below_existing(buffer.make_mut(), &layers);
        }
        gst::PadProbeReturn::Ok
    })
    .ok_or("could not install the overlay attach probe")?;
    Ok(())
}

fn merge_below_existing(buffer: &mut gst::BufferRef, layers: &gst_video::VideoOverlayComposition) {
    if let Some(mut meta) = buffer.meta_mut::<gst_video::VideoOverlayCompositionMeta>() {
        // Build a new composition from the existing rectangle objects. Copying a composition
        // would copy its rectangles too and force a texture upload on every frame.
        let rectangles: Vec<_> = layers.iter().chain(meta.overlay().iter()).collect();
        match gst_video::VideoOverlayComposition::new(&rectangles) {
            Ok(merged) => meta.set_overlay(&merged),
            Err(error) => {
                gst::warning!(gst::CAT_DEFAULT, "overlay layers not merged: {error}");
            }
        }
    } else {
        gst_video::VideoOverlayCompositionMeta::add(buffer, layers);
    }
}

/// Reports whether the OSD output negotiated overlay-composition metadata, i.e. whether the sink
/// composites overlays rather than expecting them blended into the pixels.
fn attaches_composition(osd_src: &gst::Pad) -> bool {
    osd_src.current_caps().is_some_and(|caps| {
        caps.iter_with_features().any(|(_, features)| {
            features.contains(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION)
        })
    })
}

fn current_video_info(
    pad: &gst::Pad,
    cache: &Mutex<Option<(gst::Caps, gst_video::VideoInfo)>>,
) -> Option<gst_video::VideoInfo> {
    let caps = pad.current_caps()?;
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached_caps, info)) = cache.as_ref()
        && cached_caps.as_ptr() == caps.as_ptr()
    {
        return Some(info.clone());
    }
    let info = gst_video::VideoInfo::from_caps(&caps).ok()?;
    *cache = Some((caps, info.clone()));
    Some(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::protocol::Geometry;
    use crate::overlay::store::DecodedImage;

    fn opaque_red_layer(store: &LayerStore, x: i32, y: i32, width: u32, height: u32) {
        let red = if cfg!(target_endian = "little") {
            [0, 0, 255, 255]
        } else {
            [255, 255, 0, 0]
        };
        store
            .put(
                "red",
                Geometry {
                    x,
                    y,
                    width,
                    height,
                    z: 0,
                    opacity: 255,
                },
                DecodedImage {
                    width: 1,
                    height: 1,
                    pixels: red.to_vec(),
                },
            )
            .unwrap();
    }

    /// Runs a short black RGBA stream through a silent OSD and returns the last frame's pixels.
    fn render_one_frame(store: Arc<LayerStore>) -> Vec<u8> {
        let pipeline = gst::parse::launch(
            "videotestsrc pattern=black num-buffers=1 ! video/x-raw,format=RGBA,width=8,height=4 \
             ! timeoverlay name=osd silent=true ! fakesink name=out sync=false",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let osd = pipeline.by_name("osd").unwrap();
        attach_layers_around_osd(&osd, store).unwrap();
        let out = pipeline.by_name("out").unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let sink_pad = out.static_pad("sink").unwrap();
        let captured = frames.clone();
        sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                *captured.lock().unwrap() = buffer.map_readable().unwrap().to_vec();
            }
            gst::PadProbeReturn::Ok
        });
        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        let message = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(10),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("pipeline finished");
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(
            matches!(message.view(), gst::MessageView::Eos(_)),
            "pipeline failed: {message:?}"
        );
        frames.lock().unwrap().clone()
    }

    #[test]
    fn blends_layers_into_frames_when_the_sink_does_not_composite() {
        gst::init().unwrap();
        if gst::ElementFactory::find("timeoverlay").is_none()
            || gst::ElementFactory::find("videotestsrc").is_none()
        {
            eprintln!("skipping: timeoverlay or videotestsrc unavailable");
            return;
        }
        let store = Arc::new(LayerStore::new());
        opaque_red_layer(&store, 2, 1, 3, 2);
        let frame = render_one_frame(store);
        assert_eq!(frame.len(), 8 * 4 * 4);
        let pixel = |x: usize, y: usize| &frame[(y * 8 + x) * 4..(y * 8 + x) * 4 + 3];
        assert_eq!(pixel(2, 1), [255, 0, 0], "inside the layer rectangle");
        assert_eq!(pixel(4, 2), [255, 0, 0], "inside the layer rectangle");
        assert_eq!(pixel(1, 1), [0, 0, 0], "left of the layer rectangle");
        assert_eq!(pixel(5, 2), [0, 0, 0], "right of the layer rectangle");
        assert_eq!(pixel(2, 3), [0, 0, 0], "below the layer rectangle");
    }

    #[test]
    fn merges_layers_beneath_an_existing_composition() {
        gst::init().unwrap();
        let store = LayerStore::new();
        opaque_red_layer(&store, 0, 0, 4, 4);
        let layers = store.composition().unwrap();
        let osd_store = LayerStore::new();
        opaque_red_layer(&osd_store, 10, 10, 2, 2);
        let osd = osd_store.composition().unwrap();

        let mut buffer = gst::Buffer::new();
        gst_video::VideoOverlayCompositionMeta::add(buffer.get_mut().unwrap(), &osd);
        merge_below_existing(buffer.get_mut().unwrap(), &layers);
        let metas = buffer
            .iter_meta::<gst_video::VideoOverlayCompositionMeta>()
            .count();
        assert_eq!(metas, 1, "layers must not add a second composition meta");
        let merged = buffer
            .meta::<gst_video::VideoOverlayCompositionMeta>()
            .unwrap()
            .overlay_owned();
        assert_eq!(merged.n_rectangles(), 2);
        assert_eq!(
            merged.rectangle(0).unwrap().seqnum(),
            layers.rectangle(0).unwrap().seqnum(),
            "the layer is drawn first and keeps its rectangle object"
        );
        assert_eq!(
            merged.rectangle(1).unwrap().seqnum(),
            osd.rectangle(0).unwrap().seqnum()
        );

        let mut bare = gst::Buffer::new();
        merge_below_existing(bare.get_mut().unwrap(), &layers);
        assert_eq!(
            bare.meta::<gst_video::VideoOverlayCompositionMeta>()
                .unwrap()
                .overlay()
                .n_rectangles(),
            1
        );
    }
}
