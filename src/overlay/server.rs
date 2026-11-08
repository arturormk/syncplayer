//! Per-player overlay listener and a matching blocking client.

use super::protocol::{
    Request, Response, read_request, read_response, write_request, write_response,
};
use super::store::{LayerStore, decode_png};
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Most simultaneous overlay connections; more are refused with an error response.
pub const MAX_OVERLAY_CONNECTIONS: usize = 4;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A running overlay listener. Dropping it stops accepting connections.
pub struct OverlayServer {
    local_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
}

impl OverlayServer {
    /// Starts serving overlay requests from `listener` into `store`.
    ///
    /// # Errors
    ///
    /// Returns an error when the listener cannot be configured or its thread cannot start.
    pub fn spawn(
        listener: TcpListener,
        store: Arc<LayerStore>,
        token: Option<String>,
    ) -> io::Result<Self> {
        let local_addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let context = Arc::new(Context {
            store,
            token: token.unwrap_or_default(),
            shutdown: shutdown.clone(),
            active: AtomicUsize::new(0),
        });
        let handle = thread::Builder::new()
            .name("syncplayer-overlay-listener".to_owned())
            .spawn(move || accept_loop(&listener, &context))?;
        Ok(Self {
            local_addr,
            shutdown,
            listener: Some(handle),
        })
    }

    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for OverlayServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.listener.take() {
            let _ = handle.join();
        }
    }
}

struct Context {
    store: Arc<LayerStore>,
    token: String,
    shutdown: Arc<AtomicBool>,
    active: AtomicUsize,
}

struct ActiveConnection<'a>(&'a AtomicUsize);

impl Drop for ActiveConnection<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn accept_loop(listener: &TcpListener, context: &Arc<Context>) {
    while !context.shutdown.load(Ordering::Acquire) {
        let (mut stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            }
            Err(error) => {
                warn!(%error, "failed to accept overlay connection");
                thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            }
        };
        if stream.set_nonblocking(false).is_err() {
            continue;
        }
        if context.active.fetch_add(1, Ordering::AcqRel) >= MAX_OVERLAY_CONNECTIONS {
            context.active.fetch_sub(1, Ordering::AcqRel);
            warn!(%peer, "refused overlay connection: too many open connections");
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            let _ = write_response(
                &mut stream,
                &Response::Error("too many open overlay connections".to_owned()),
            );
            continue;
        }
        let context = context.clone();
        let spawned = thread::Builder::new()
            .name("syncplayer-overlay-session".to_owned())
            .spawn(move || {
                let _active = ActiveConnection(&context.active);
                if let Err(error) = run_session(stream, peer, &context) {
                    warn!(%peer, %error, "overlay connection ended with an error");
                }
            });
        if let Err(error) = spawned {
            warn!(%error, "failed to start overlay session");
        }
    }
}

fn run_session(mut stream: TcpStream, peer: SocketAddr, context: &Context) -> io::Result<()> {
    stream.set_read_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_write_timeout(Some(IDLE_TIMEOUT))?;
    stream.set_nodelay(true)?;
    loop {
        if context.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        let (token, request) = match read_request(&mut stream) {
            Ok(request) => request,
            Err(super::protocol::ReadError::Io(error))
                if error.kind() == ErrorKind::UnexpectedEof =>
            {
                return Ok(());
            }
            Err(super::protocol::ReadError::Io(error)) => return Err(error),
            Err(super::protocol::ReadError::Parse(error)) => {
                // Framing can no longer be trusted, so report and close.
                let _ = write_response(&mut stream, &Response::Error(error.to_string()));
                return Err(io::Error::new(ErrorKind::InvalidData, error));
            }
        };
        let response = if tokens_match(&token, &context.token) {
            execute(&context.store, request, peer)
        } else {
            warn!(%peer, "rejected overlay request with an invalid token");
            Response::Error("invalid overlay token".to_owned())
        };
        write_response(&mut stream, &response)
            .map_err(|error| io::Error::new(ErrorKind::BrokenPipe, error.to_string()))?;
    }
}

fn execute(store: &LayerStore, request: Request, peer: SocketAddr) -> Response {
    let result = match request {
        Request::Put { id, geometry, png } => {
            let started = Instant::now();
            let png_bytes = png.len();
            decode_png(&png).and_then(|image| {
                let (width, height) = (image.width, image.height);
                store.put(&id, geometry, image)?;
                info!(
                    %peer, layer = %id, x = geometry.x, y = geometry.y,
                    render_width = geometry.width, render_height = geometry.height,
                    z = geometry.z, opacity = geometry.opacity,
                    image_width = width, image_height = height, png_bytes,
                    decode_ms = started.elapsed().as_secs_f64() * 1000.0,
                    "overlay layer set"
                );
                Ok(())
            })
        }
        Request::Move { id, geometry } => store.move_layer(&id, geometry).inspect(|()| {
            info!(
                %peer, layer = %id, x = geometry.x, y = geometry.y,
                render_width = geometry.width, render_height = geometry.height,
                z = geometry.z, opacity = geometry.opacity,
                "overlay layer moved"
            );
        }),
        Request::Delete { id } => store
            .delete(&id)
            .inspect(|()| info!(%peer, layer = %id, "overlay layer deleted")),
        Request::Clear => {
            store.clear();
            info!(%peer, "overlay layers cleared");
            Ok(())
        }
        Request::List => return Response::Layers(store.list()),
    };
    match result {
        Ok(()) => Response::Ok,
        Err(error) => Response::Error(error.to_string()),
    }
}

/// Compares tokens without an early exit on the first differing byte.
fn tokens_match(received: &str, expected: &str) -> bool {
    let (received, expected) = (received.as_bytes(), expected.as_bytes());
    let mut difference = received.len() ^ expected.len();
    for (index, byte) in expected.iter().enumerate() {
        difference |= usize::from(byte ^ received.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

/// Sends one request to a player's overlay listener and returns its response.
///
/// # Errors
///
/// Returns connection failures and protocol violations.
pub fn send_request(
    address: impl ToSocketAddrs,
    token: &str,
    request: &Request,
    timeout: Duration,
) -> io::Result<Response> {
    let mut last_error = io::Error::new(ErrorKind::NotFound, "address did not resolve");
    for candidate in address.to_socket_addrs()? {
        match TcpStream::connect_timeout(&candidate, timeout) {
            Ok(mut stream) => {
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                stream.set_nodelay(true)?;
                write_request(&mut stream, token, request).map_err(to_io)?;
                return read_response(&mut stream).map_err(to_io);
            }
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn to_io(error: super::protocol::ReadError) -> io::Error {
    match error {
        super::protocol::ReadError::Io(error) => error,
        super::protocol::ReadError::Parse(error) => io::Error::new(ErrorKind::InvalidData, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::protocol::Geometry;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn tiny_png() -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, 3, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[255; 3 * 2 * 4]).unwrap();
        writer.finish().unwrap();
        out
    }

    fn start(token: Option<&str>) -> (OverlayServer, Arc<LayerStore>) {
        gstreamer::init().unwrap();
        let store = Arc::new(LayerStore::new());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server =
            OverlayServer::spawn(listener, store.clone(), token.map(str::to_owned)).unwrap();
        (server, store)
    }

    const NATURAL: Geometry = Geometry {
        x: 10,
        y: 20,
        width: 0,
        height: 0,
        z: 0,
        opacity: 255,
    };

    #[test]
    fn put_move_list_delete_over_tcp() {
        let (server, store) = start(None);
        let address = server.local_addr();
        let put = Request::Put {
            id: "label".to_owned(),
            geometry: NATURAL,
            png: tiny_png(),
        };
        assert_eq!(
            send_request(address, "", &put, TIMEOUT).unwrap(),
            Response::Ok
        );
        assert!(store.composition().is_some());

        let Response::Layers(layers) = send_request(address, "", &Request::List, TIMEOUT).unwrap()
        else {
            panic!("expected a layer listing");
        };
        assert_eq!(layers.len(), 1);
        assert_eq!(
            (layers[0].geometry.width, layers[0].geometry.height),
            (3, 2)
        );

        let moved = Geometry {
            width: 30,
            height: 20,
            ..NATURAL
        };
        let request = Request::Move {
            id: "label".to_owned(),
            geometry: moved,
        };
        assert_eq!(
            send_request(address, "", &request, TIMEOUT).unwrap(),
            Response::Ok
        );
        assert_eq!(store.list()[0].geometry, moved);

        let request = Request::Delete {
            id: "label".to_owned(),
        };
        assert_eq!(
            send_request(address, "", &request, TIMEOUT).unwrap(),
            Response::Ok
        );
        assert!(store.composition().is_none());

        let Response::Error(message) = send_request(address, "", &request, TIMEOUT).unwrap() else {
            panic!("deleting a missing layer must fail");
        };
        assert!(message.contains("does not exist"), "{message}");
    }

    #[test]
    fn rejects_bad_png_and_wrong_token() {
        let (server, store) = start(Some("s3cret"));
        let address = server.local_addr();
        let put = Request::Put {
            id: "label".to_owned(),
            geometry: NATURAL,
            png: b"not a png".to_vec(),
        };
        let Response::Error(message) = send_request(address, "s3cret", &put, TIMEOUT).unwrap()
        else {
            panic!("a malformed PNG must be rejected");
        };
        assert!(message.contains("PNG"), "{message}");

        let Response::Error(message) =
            send_request(address, "wrong", &Request::Clear, TIMEOUT).unwrap()
        else {
            panic!("a wrong token must be rejected");
        };
        assert!(message.contains("token"), "{message}");
        assert_eq!(
            send_request(address, "", &Request::List, TIMEOUT).unwrap(),
            Response::Error("invalid overlay token".to_owned())
        );
        assert!(store.list().is_empty());
        assert!(matches!(
            send_request(address, "s3cret", &Request::List, TIMEOUT).unwrap(),
            Response::Layers(_)
        ));
    }

    #[test]
    fn token_comparison() {
        assert!(tokens_match("", ""));
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abc", "abd"));
        assert!(!tokens_match("ab", "abc"));
        assert!(!tokens_match("abcd", "abc"));
        assert!(!tokens_match("x", ""));
    }
}
