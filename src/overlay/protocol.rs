//! Versioned, bounded binary protocol for the per-player overlay layer channel (ADR 0014).
//!
//! Every frame starts with a 12-byte network-byte-order envelope: magic `SYNO`, a two-byte
//! protocol version, a one-byte message kind, a reserved zero flags byte, and a four-byte payload
//! length. Each request payload begins with the shared token string (empty when the player has no
//! token). Strings carry a two-byte length followed by UTF-8.

use std::fmt;
use std::io::{self, Read, Write};

pub const OVERLAY_PROTOCOL_VERSION: u16 = 1;
/// Default TCP port suggested for `--overlay-bind` and assumed by the client when none is given.
pub const DEFAULT_OVERLAY_PORT: u16 = 46_002;
/// Largest request payload, including the PNG bytes of a `PUT`.
pub const MAX_REQUEST_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
/// Largest response payload; a full `LAYERS` listing stays far below it.
pub const MAX_RESPONSE_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_LAYER_ID_BYTES: usize = 64;
pub const MAX_TOKEN_BYTES: usize = 256;
pub const MAX_ERROR_BYTES: usize = 1_024;
/// Largest render width or height, and the largest distance of a render origin from the frame
/// origin, in output pixels.
pub const MAX_RENDER_EXTENT: u32 = 16_384;
pub const MAX_LAYERS_LISTED: usize = 1_024;

const MAGIC: [u8; 4] = *b"SYNO";
const HEADER_BYTES: usize = 12;

const KIND_PUT: u8 = 1;
const KIND_MOVE: u8 = 2;
const KIND_DELETE: u8 = 3;
const KIND_CLEAR: u8 = 4;
const KIND_LIST: u8 = 5;
const KIND_OK: u8 = 64;
const KIND_ERROR: u8 = 65;
const KIND_LAYERS: u8 = 66;

/// Placement of one layer in output-frame pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    /// Render width. `0` together with a zero `height` means "the image's own size" in `PUT`.
    pub width: u32,
    pub height: u32,
    /// Stacking order; higher values are drawn later (on top). Ties are broken by layer id.
    pub z: i16,
    /// Whole-layer opacity, `0` transparent to `255` opaque, applied on top of per-pixel alpha.
    pub opacity: u8,
}

impl Geometry {
    #[must_use]
    pub const fn uses_natural_size(&self) -> bool {
        self.width == 0 && self.height == 0
    }

    fn validate(&self, allow_natural_size: bool) -> Result<(), ParseError> {
        let limit = i64::from(MAX_RENDER_EXTENT);
        if i64::from(self.x).abs() > limit || i64::from(self.y).abs() > limit {
            return Err(ParseError::InvalidValue("layer origin"));
        }
        if allow_natural_size && self.uses_natural_size() {
            return Ok(());
        }
        if !(1..=MAX_RENDER_EXTENT).contains(&self.width)
            || !(1..=MAX_RENDER_EXTENT).contains(&self.height)
        {
            return Err(ParseError::InvalidValue("layer size"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Create or replace a layer from PNG bytes.
    Put {
        id: String,
        geometry: Geometry,
        png: Vec<u8>,
    },
    /// Change an existing layer's geometry, stacking, and opacity without new pixels.
    Move {
        id: String,
        geometry: Geometry,
    },
    Delete {
        id: String,
    },
    Clear,
    List,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayerInfo {
    pub id: String,
    pub geometry: Geometry,
    pub image_width: u32,
    pub image_height: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Response {
    Ok,
    Error(String),
    Layers(Vec<LayerInfo>),
}

#[derive(Debug, Eq, PartialEq)]
pub enum ParseError {
    InvalidMagic,
    UnsupportedVersion(u16),
    UnknownMessage(u8),
    InvalidLength,
    InvalidValue(&'static str),
    InvalidUtf8,
    TextTooLong,
    TooLong,
    TrailingBytes,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagic => f.write_str("not an overlay protocol frame"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported overlay protocol version {version}")
            }
            Self::UnknownMessage(kind) => write!(f, "unknown overlay message kind {kind}"),
            Self::InvalidLength => f.write_str("truncated overlay message"),
            Self::InvalidValue(field) => write!(f, "invalid {field}"),
            Self::InvalidUtf8 => f.write_str("text is not valid UTF-8"),
            Self::TextTooLong => f.write_str("text field is too long"),
            Self::TooLong => f.write_str("overlay message exceeds its size limit"),
            Self::TrailingBytes => f.write_str("overlay message has trailing bytes"),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    Parse(ParseError),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "overlay connection I/O failed: {error}"),
            Self::Parse(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ReadError {}

impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ParseError> for ReadError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

/// Checks that a layer id is 1-64 bytes of ASCII letters, digits, `.`, `_`, or `-`.
///
/// # Errors
///
/// Returns an error describing why the id is unacceptable.
pub fn validate_layer_id(id: &str) -> Result<(), ParseError> {
    if id.is_empty() || id.len() > MAX_LAYER_ID_BYTES {
        return Err(ParseError::InvalidValue("layer id length"));
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ParseError::InvalidValue("layer id characters"));
    }
    Ok(())
}

/// Encodes one request frame.
///
/// # Errors
///
/// Returns an error when a field violates the protocol limits.
pub fn encode_request(token: &str, request: &Request) -> Result<Vec<u8>, ParseError> {
    let mut payload = Vec::new();
    string(&mut payload, token, MAX_TOKEN_BYTES)?;
    let kind = match request {
        Request::Put { id, geometry, png } => {
            layer_id(&mut payload, id)?;
            geometry.validate(true)?;
            encode_geometry(&mut payload, geometry);
            if png.is_empty() {
                return Err(ParseError::InvalidValue("empty PNG payload"));
            }
            payload.extend_from_slice(png);
            KIND_PUT
        }
        Request::Move { id, geometry } => {
            layer_id(&mut payload, id)?;
            geometry.validate(false)?;
            encode_geometry(&mut payload, geometry);
            KIND_MOVE
        }
        Request::Delete { id } => {
            layer_id(&mut payload, id)?;
            KIND_DELETE
        }
        Request::Clear => KIND_CLEAR,
        Request::List => KIND_LIST,
    };
    frame(kind, &payload, MAX_REQUEST_PAYLOAD_BYTES)
}

/// Decodes one request payload of the given kind into its token and request.
///
/// # Errors
///
/// Returns an error for unknown kinds, malformed fields, or trailing bytes.
pub fn decode_request(kind: u8, payload: &[u8]) -> Result<(String, Request), ParseError> {
    let mut decoder = Decoder::new(payload);
    let token = decoder.string(MAX_TOKEN_BYTES)?;
    let request = match kind {
        KIND_PUT => {
            let id = decoder.layer_id()?;
            let geometry = decoder.geometry()?;
            geometry.validate(true)?;
            let png = decoder.rest().to_vec();
            if png.is_empty() {
                return Err(ParseError::InvalidValue("empty PNG payload"));
            }
            Request::Put { id, geometry, png }
        }
        KIND_MOVE => {
            let id = decoder.layer_id()?;
            let geometry = decoder.geometry()?;
            geometry.validate(false)?;
            Request::Move { id, geometry }
        }
        KIND_DELETE => Request::Delete {
            id: decoder.layer_id()?,
        },
        KIND_CLEAR => Request::Clear,
        KIND_LIST => Request::List,
        other => return Err(ParseError::UnknownMessage(other)),
    };
    decoder.finish()?;
    Ok((token, request))
}

/// Encodes one response frame.
///
/// # Errors
///
/// Returns an error when a field violates the protocol limits.
pub fn encode_response(response: &Response) -> Result<Vec<u8>, ParseError> {
    let mut payload = Vec::new();
    let kind = match response {
        Response::Ok => KIND_OK,
        Response::Error(message) => {
            string(
                &mut payload,
                truncate(message, MAX_ERROR_BYTES),
                MAX_ERROR_BYTES,
            )?;
            KIND_ERROR
        }
        Response::Layers(layers) => {
            if layers.len() > MAX_LAYERS_LISTED {
                return Err(ParseError::TooLong);
            }
            payload.extend_from_slice(
                &u16::try_from(layers.len())
                    .map_err(|_| ParseError::TooLong)?
                    .to_be_bytes(),
            );
            for layer in layers {
                layer_id(&mut payload, &layer.id)?;
                encode_geometry(&mut payload, &layer.geometry);
                payload.extend_from_slice(&layer.image_width.to_be_bytes());
                payload.extend_from_slice(&layer.image_height.to_be_bytes());
            }
            KIND_LAYERS
        }
    };
    frame(kind, &payload, MAX_RESPONSE_PAYLOAD_BYTES)
}

/// Decodes one response payload of the given kind.
///
/// # Errors
///
/// Returns an error for unknown kinds, malformed fields, or trailing bytes.
pub fn decode_response(kind: u8, payload: &[u8]) -> Result<Response, ParseError> {
    let mut decoder = Decoder::new(payload);
    let response = match kind {
        KIND_OK => Response::Ok,
        KIND_ERROR => Response::Error(decoder.string(MAX_ERROR_BYTES)?),
        KIND_LAYERS => {
            let count = usize::from(decoder.u16()?);
            if count > MAX_LAYERS_LISTED {
                return Err(ParseError::TooLong);
            }
            let mut layers = Vec::with_capacity(count);
            for _ in 0..count {
                layers.push(LayerInfo {
                    id: decoder.layer_id()?,
                    geometry: decoder.geometry()?,
                    image_width: decoder.u32()?,
                    image_height: decoder.u32()?,
                });
            }
            Response::Layers(layers)
        }
        other => return Err(ParseError::UnknownMessage(other)),
    };
    decoder.finish()?;
    Ok(response)
}

/// Reads one request frame.
///
/// # Errors
///
/// Returns I/O failures or protocol violations.
pub fn read_request(reader: &mut impl Read) -> Result<(String, Request), ReadError> {
    let (kind, payload) = read_frame(reader, MAX_REQUEST_PAYLOAD_BYTES)?;
    Ok(decode_request(kind, &payload)?)
}

/// Reads one response frame.
///
/// # Errors
///
/// Returns I/O failures or protocol violations.
pub fn read_response(reader: &mut impl Read) -> Result<Response, ReadError> {
    let (kind, payload) = read_frame(reader, MAX_RESPONSE_PAYLOAD_BYTES)?;
    Ok(decode_response(kind, &payload)?)
}

/// Writes one request frame and flushes it.
///
/// # Errors
///
/// Returns I/O failures or protocol violations.
pub fn write_request(
    writer: &mut impl Write,
    token: &str,
    request: &Request,
) -> Result<(), ReadError> {
    writer.write_all(&encode_request(token, request)?)?;
    writer.flush()?;
    Ok(())
}

/// Writes one response frame and flushes it.
///
/// # Errors
///
/// Returns I/O failures or protocol violations.
pub fn write_response(writer: &mut impl Write, response: &Response) -> Result<(), ReadError> {
    writer.write_all(&encode_response(response)?)?;
    writer.flush()?;
    Ok(())
}

fn read_frame(reader: &mut impl Read, max_payload: usize) -> Result<(u8, Vec<u8>), ReadError> {
    let mut header = [0; HEADER_BYTES];
    reader.read_exact(&mut header)?;
    if header[..4] != MAGIC {
        return Err(ParseError::InvalidMagic.into());
    }
    let version = u16::from_be_bytes([header[4], header[5]]);
    if version != OVERLAY_PROTOCOL_VERSION {
        return Err(ParseError::UnsupportedVersion(version).into());
    }
    if header[7] != 0 {
        return Err(ParseError::InvalidValue("header flags").into());
    }
    let length = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let length = usize::try_from(length).map_err(|_| ParseError::TooLong)?;
    if length > max_payload {
        return Err(ParseError::TooLong.into());
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload)?;
    Ok((header[6], payload))
}

fn frame(kind: u8, payload: &[u8], max_payload: usize) -> Result<Vec<u8>, ParseError> {
    if payload.len() > max_payload {
        return Err(ParseError::TooLong);
    }
    let length = u32::try_from(payload.len()).map_err(|_| ParseError::TooLong)?;
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&OVERLAY_PROTOCOL_VERSION.to_be_bytes());
    out.push(kind);
    out.push(0);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn string(out: &mut Vec<u8>, value: &str, max: usize) -> Result<(), ParseError> {
    if value.len() > max {
        return Err(ParseError::TextTooLong);
    }
    let length = u16::try_from(value.len()).map_err(|_| ParseError::TextTooLong)?;
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

fn layer_id(out: &mut Vec<u8>, id: &str) -> Result<(), ParseError> {
    validate_layer_id(id)?;
    string(out, id, MAX_LAYER_ID_BYTES)
}

fn encode_geometry(out: &mut Vec<u8>, geometry: &Geometry) {
    out.extend_from_slice(&geometry.x.to_be_bytes());
    out.extend_from_slice(&geometry.y.to_be_bytes());
    out.extend_from_slice(&geometry.width.to_be_bytes());
    out.extend_from_slice(&geometry.height.to_be_bytes());
    out.extend_from_slice(&geometry.z.to_be_bytes());
    out.push(geometry.opacity);
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ParseError> {
        let end = self
            .offset
            .checked_add(n)
            .ok_or(ParseError::InvalidLength)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(ParseError::InvalidLength)?;
        self.offset = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ParseError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16, ParseError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ParseError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn string(&mut self, max: usize) -> Result<String, ParseError> {
        let length = usize::from(self.u16()?);
        if length > max {
            return Err(ParseError::TextTooLong);
        }
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| ParseError::InvalidUtf8)
    }

    fn layer_id(&mut self) -> Result<String, ParseError> {
        let id = self.string(MAX_LAYER_ID_BYTES)?;
        validate_layer_id(&id)?;
        Ok(id)
    }

    fn geometry(&mut self) -> Result<Geometry, ParseError> {
        Ok(Geometry {
            x: i32::from_be_bytes(self.array()?),
            y: i32::from_be_bytes(self.array()?),
            width: self.u32()?,
            height: self.u32()?,
            z: i16::from_be_bytes(self.array()?),
            opacity: self.array::<1>()?[0],
        })
    }

    fn rest(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.offset..];
        self.offset = self.bytes.len();
        rest
    }

    fn finish(&self) -> Result<(), ParseError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(ParseError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEOMETRY: Geometry = Geometry {
        x: -20,
        y: 40,
        width: 600,
        height: 80,
        z: -3,
        opacity: 200,
    };

    fn round_trip(token: &str, request: &Request) -> (String, Request) {
        let bytes = encode_request(token, request).unwrap();
        read_request(&mut bytes.as_slice()).unwrap()
    }

    #[test]
    fn requests_round_trip() {
        let requests = [
            Request::Put {
                id: "flower-12".to_owned(),
                geometry: GEOMETRY,
                png: vec![1, 2, 3],
            },
            Request::Put {
                id: "mask".to_owned(),
                geometry: Geometry {
                    width: 0,
                    height: 0,
                    ..GEOMETRY
                },
                png: vec![9],
            },
            Request::Move {
                id: "label".to_owned(),
                geometry: GEOMETRY,
            },
            Request::Delete {
                id: "a.b_c".to_owned(),
            },
            Request::Clear,
            Request::List,
        ];
        for request in requests {
            assert_eq!(
                round_trip("secret", &request),
                ("secret".to_owned(), request)
            );
        }
    }

    #[test]
    fn responses_round_trip() {
        let responses = [
            Response::Ok,
            Response::Error("layer 'x' does not exist".to_owned()),
            Response::Layers(vec![LayerInfo {
                id: "label".to_owned(),
                geometry: GEOMETRY,
                image_width: 300,
                image_height: 40,
            }]),
            Response::Layers(Vec::new()),
        ];
        for response in responses {
            let bytes = encode_response(&response).unwrap();
            assert_eq!(read_response(&mut bytes.as_slice()).unwrap(), response);
        }
    }

    #[test]
    fn rejects_bad_layer_ids() {
        for id in ["", "has space", "slash/", "ñ", &"x".repeat(65)] {
            assert!(validate_layer_id(id).is_err(), "{id:?} should be rejected");
            assert!(encode_request("", &Request::Delete { id: id.to_owned() }).is_err());
        }
    }

    #[test]
    fn natural_size_is_only_valid_for_put() {
        let natural = Geometry {
            width: 0,
            height: 0,
            ..GEOMETRY
        };
        let request = Request::Move {
            id: "label".to_owned(),
            geometry: natural,
        };
        assert_eq!(
            encode_request("", &request),
            Err(ParseError::InvalidValue("layer size"))
        );
        let half = Geometry {
            width: 0,
            ..GEOMETRY
        };
        let request = Request::Put {
            id: "label".to_owned(),
            geometry: half,
            png: vec![1],
        };
        assert!(encode_request("", &request).is_err());
    }

    #[test]
    fn rejects_out_of_range_geometry() {
        let request = Request::Move {
            id: "label".to_owned(),
            geometry: Geometry {
                x: 20_000,
                ..GEOMETRY
            },
        };
        assert_eq!(
            encode_request("", &request),
            Err(ParseError::InvalidValue("layer origin"))
        );
        let request = Request::Move {
            id: "label".to_owned(),
            geometry: Geometry {
                width: MAX_RENDER_EXTENT + 1,
                ..GEOMETRY
            },
        };
        assert!(encode_request("", &request).is_err());
    }

    #[test]
    fn rejects_malformed_frames() {
        let good = encode_request("", &Request::List).unwrap();

        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(matches!(
            read_request(&mut bad_magic.as_slice()),
            Err(ReadError::Parse(ParseError::InvalidMagic))
        ));

        let mut bad_version = good.clone();
        bad_version[5] = 99;
        assert!(matches!(
            read_request(&mut bad_version.as_slice()),
            Err(ReadError::Parse(ParseError::UnsupportedVersion(99)))
        ));

        let mut bad_flags = good.clone();
        bad_flags[7] = 1;
        assert!(read_request(&mut bad_flags.as_slice()).is_err());

        let mut oversized = good.clone();
        oversized[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            read_request(&mut oversized.as_slice()),
            Err(ReadError::Parse(ParseError::TooLong))
        ));

        let mut trailing = good.clone();
        trailing.push(0);
        trailing[11] += 1;
        assert!(matches!(
            read_request(&mut trailing.as_slice()),
            Err(ReadError::Parse(ParseError::TrailingBytes))
        ));

        let truncated = &good[..good.len() - 1];
        assert!(matches!(
            read_request(&mut &truncated[..]),
            Err(ReadError::Io(_))
        ));

        assert_eq!(
            decode_request(99, &[0, 0]),
            Err(ParseError::UnknownMessage(99))
        );
    }

    #[test]
    fn long_error_messages_are_truncated_on_a_char_boundary() {
        let message = "é".repeat(MAX_ERROR_BYTES);
        let bytes = encode_response(&Response::Error(message)).unwrap();
        let Response::Error(decoded) = read_response(&mut bytes.as_slice()).unwrap() else {
            panic!("expected an error response");
        };
        assert!(decoded.len() <= MAX_ERROR_BYTES);
        assert!(decoded.chars().all(|c| c == 'é'));
    }
}
