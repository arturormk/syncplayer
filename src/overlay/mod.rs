//! Per-player overlay layers (ADR 0014).
//!
//! A player that enables `--overlay-bind` accepts named, positioned RGBA layers over a small TCP
//! protocol and draws them over its video, beneath the diagnostic OSD. Layers are host-local:
//! they are not part of the shared program, are not synchronized between players, and do not
//! survive a restart.

pub mod protocol;
pub mod render;
pub mod server;
pub mod store;

pub use protocol::{DEFAULT_OVERLAY_PORT, Geometry, LayerInfo, Request, Response};
pub use render::attach_layers_around_osd;
pub use server::{OverlayServer, send_request};
pub use store::{DecodedImage, LayerStore, decode_png};
