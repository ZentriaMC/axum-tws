#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

pub mod upgrade;
pub mod websocket;

use std::fmt::Display;

use axum_core::body::Body;
use axum_core::response::IntoResponse;
use axum_core::response::Response;
use http::StatusCode;

pub use tokio_websockets::*;

pub use crate::{upgrade::WebSocketUpgrade, websocket::WebSocket};

#[derive(Debug)]
pub enum WebSocketError {
    ConnectionNotUpgradeable,
    Internal(tokio_websockets::Error),
    InvalidConnectionHeader,
    /// For WebSocket over HTTP/2+
    InvalidProtocolPseudoheader,
    InvalidUpgradeHeader,
    InvalidWebSocketVersionHeader,
    /// Invalid method for WebSocket over HTTP/1.x
    MethodNotGet,
    /// Invalid method for WebSocket over HTTP/2+
    MethodNotConnect,
    UpgradeFailed(hyper::Error),
}

impl Display for WebSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebSocketError::ConnectionNotUpgradeable => {
                write!(f, "connection is not upgradeable")
            }
            WebSocketError::Internal(e) => {
                write!(f, "internal server error: {}", e)
            }
            WebSocketError::InvalidConnectionHeader => {
                write!(f, "invalid `Connection` header")
            }
            WebSocketError::InvalidProtocolPseudoheader => {
                write!(f, "invalid `:protocol` pseudoheader")
            }
            WebSocketError::InvalidUpgradeHeader => {
                write!(f, "invalid `Upgrade` header")
            }
            WebSocketError::InvalidWebSocketVersionHeader => {
                write!(f, "invalid `Sec-WebSocket-Version` header")
            }
            WebSocketError::MethodNotGet => {
                write!(f, "http request method must be `GET`")
            }
            WebSocketError::MethodNotConnect => {
                write!(f, "http2 request method must be `CONNECT`")
            }
            WebSocketError::UpgradeFailed(e) => {
                write!(f, "upgrade failed: {}", e)
            }
        }
    }
}

impl std::error::Error for WebSocketError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WebSocketError::Internal(e) => Some(e),
            WebSocketError::UpgradeFailed(e) => Some(e),
            _ => None,
        }
    }
}

impl IntoResponse for WebSocketError {
    fn into_response(self) -> Response<Body> {
        let status = match self {
            WebSocketError::ConnectionNotUpgradeable => StatusCode::UPGRADE_REQUIRED,

            // Request headers are invalid or missing.
            WebSocketError::InvalidConnectionHeader
            | WebSocketError::InvalidUpgradeHeader
            | WebSocketError::InvalidWebSocketVersionHeader => StatusCode::BAD_REQUEST,

            // Invalid request method.
            WebSocketError::MethodNotGet => StatusCode::METHOD_NOT_ALLOWED,

            // All other errors will be treated as internal server errors.
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        Response::builder()
            .status(status)
            .body(Body::empty())
            .unwrap()
    }
}

impl From<tokio_websockets::Error> for WebSocketError {
    fn from(e: tokio_websockets::Error) -> Self {
        WebSocketError::Internal(e)
    }
}

impl From<hyper::Error> for WebSocketError {
    fn from(e: hyper::Error) -> Self {
        WebSocketError::UpgradeFailed(e)
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;
    use std::net::SocketAddr;

    use super::*;
    use axum::serve::ListenerExt;
    use axum::{routing::any, Router};
    use futures_util::{SinkExt, StreamExt};
    use http::{Method, Request, Version};
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::net::TcpListener;
    use tokio_websockets::Message;
    use tower::ServiceExt;

    async fn spawn_service() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let listener = listener.tap_io(|stream| {
            if let Err(err) = stream.set_nodelay(true) {
                eprintln!("Failed to set nodelay: {}", err);
            }
        });
        tokio::spawn(async move {
            axum::serve(listener, echo_app()).await.unwrap();
        });

        addr
    }

    #[tokio::test]
    async fn rejects_http_1_0_requests() {
        let svc = any(|ws: Result<WebSocketUpgrade, WebSocketError>| {
            let rejection = ws.unwrap_err();
            assert!(matches!(
                rejection,
                WebSocketError::ConnectionNotUpgradeable,
            ));
            std::future::ready(())
        });

        let req = Request::builder()
            .version(Version::HTTP_10)
            .method(Method::GET)
            .header("upgrade", "websocket")
            .header("connection", "Upgrade")
            .header("sec-websocket-key", "6D69KGBOr4Re+Nj6zx9aQA==")
            .header("sec-websocket-version", "13")
            .body(Body::empty())
            .unwrap();

        let res = svc.oneshot(req).await.unwrap();

        assert_eq!(res.status(), StatusCode::OK);
    }

    #[allow(dead_code)]
    fn default_on_failed_upgrade() {
        async fn handler(ws: WebSocketUpgrade) -> Response {
            ws.on_upgrade(|_| async {})
        }
        let _: Router = Router::new().route("/", any(handler));
    }

    #[allow(dead_code)]
    fn on_failed_upgrade() {
        async fn handler(ws: WebSocketUpgrade) -> Response {
            ws.on_failed_upgrade(|_error: WebSocketError| println!("oops!"))
                .on_upgrade(|_| async {})
        }
        let _: Router = Router::new().route("/", any(handler));
    }

    #[tokio::test]
    async fn integration_test() {
        let addr = spawn_service().await;

        let (socket, _response) =
            tokio_websockets::ClientBuilder::from_uri(format!("ws://{addr}/echo").parse().unwrap())
                .connect()
                .await
                .unwrap();
        test_echo_app(socket).await;
    }

    #[tokio::test]
    #[cfg(feature = "http2")]
    async fn http2() {
        use http_body_util::BodyExt as _;
        use hyper_util::rt::{TokioExecutor, TokioIo};
        use tokio::net::TcpStream;

        let addr = spawn_service().await;
        let io = TokioIo::new(TcpStream::connect(addr).await.unwrap());
        let (mut send_request, conn) =
            hyper::client::conn::http2::Builder::new(TokioExecutor::new())
                .handshake(io)
                .await
                .unwrap();

        // Wait a little for the SETTINGS frame to go through…
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(conn.is_extended_connect_protocol_enabled());
        tokio::spawn(async {
            conn.await.unwrap();
        });

        let req = Request::builder()
            .method(Method::CONNECT)
            .extension(hyper::ext::Protocol::from_static("websocket"))
            .uri("/echo")
            .header("sec-websocket-version", "13")
            .header("Host", "server.example.com")
            .body(Body::empty())
            .unwrap();

        let response = send_request.send_request(req).await.unwrap();
        let status = response.status();
        if status != 200 {
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body = std::str::from_utf8(&body).unwrap();
            panic!("response status was {status}: {body}");
        }
        let upgraded = hyper::upgrade::on(response).await.unwrap();
        let upgraded = TokioIo::new(upgraded);

        let socket = ClientBuilder::new().take_over(upgraded);
        test_echo_app(socket).await;
    }

    fn echo_app() -> Router {
        async fn handle_socket(mut socket: WebSocket) {
            while let Some(Ok(msg)) = socket.next().await {
                if msg.is_text() || msg.is_binary() || msg.is_close() {
                    // XXX: Clippy do not merge this above please!
                    if socket.send(msg).await.is_err() {
                        break;
                    }
                }
                // tokio-websockets will respond to pings automatically
            }
        }

        Router::new().route(
            "/echo",
            any(|ws: WebSocketUpgrade| ready(ws.on_upgrade(handle_socket))),
        )
    }

    async fn test_echo_app<S: AsyncRead + AsyncWrite + Unpin>(mut socket: WebSocketStream<S>) {
        let input = Message::text("foobar");
        socket.send(input.clone()).await.unwrap();

        let output = socket.next().await.unwrap().unwrap();
        assert_eq!(input.as_text(), output.as_text());

        socket.send(Message::ping("ping")).await.unwrap();

        let output = socket.next().await.unwrap().unwrap();

        assert!(output.is_pong());
        assert!(output.as_payload().to_vec() == b"ping");
    }
}
