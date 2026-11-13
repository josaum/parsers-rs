//! Reading an HTTP response body under a size limit.
//!
//! Every response the WMS and WFS clients read goes through
//! [`read_body_within`], so the rule for how much of a server's answer is
//! kept lives in one place.

use crate::error::GeoError;

/// Reads the body of `response`, refusing one longer than `max` bytes.
///
/// A `Content-Length` over `max` is refused before any of the body is
/// read. Otherwise the length is checked before each piece is kept, so no
/// more than `max` bytes and the piece in hand are ever held. `what` names
/// the request in the error, as in `WMS GetMap`.
///
/// # Errors
///
/// Returns [`GeoError::Http`] when the body is longer than `max` bytes or
/// reading it fails.
pub(crate) async fn read_body_within(
    mut response: reqwest::Response,
    max: usize,
    what: &str,
) -> Result<Vec<u8>, GeoError> {
    let too_large = || GeoError::Http(format!("{what} response is larger than {max} bytes"));
    let announced = response.content_length();
    if announced.is_some_and(|length| usize::try_from(length).map_or(true, |length| length > max)) {
        return Err(too_large());
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(piece) = response
        .chunk()
        .await
        .map_err(|e| GeoError::Http(e.to_string()))?
    {
        if piece.len() > max.saturating_sub(body.len()) {
            return Err(too_large());
        }
        body.extend_from_slice(&piece);
    }
    Ok(body)
}

/// A one-connection HTTP server and a client that reaches it directly.
#[cfg(test)]
pub(crate) mod test_server {
    use reqwest::Client;

    /// Serves `response` to the first connection on a local port and
    /// returns the address to ask.
    pub(crate) fn serve_once(response: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                // The client hangs up on a body it refuses.
                let _ = stream.write_all(&response);
            }
        });
        format!("http://{address}")
    }

    /// A client that ignores any proxy the environment configures.
    pub(crate) fn local_client() -> Client {
        Client::builder().no_proxy().build().expect("client")
    }

    /// `body` as a `200 OK` response, its length announced in
    /// `Content-Length` or, without it, given by the connection closing.
    pub(crate) fn ok_response(body: &[u8], announced: bool) -> Vec<u8> {
        let head = if announced {
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len())
        } else {
            "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_owned()
        };
        let mut response = head.into_bytes();
        response.extend_from_slice(body);
        response
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{local_client, serve_once};
    use super::*;

    async fn read_within(response: Vec<u8>, max: usize) -> Result<Vec<u8>, GeoError> {
        let url = serve_once(response);
        let response = local_client().get(&url).send().await.expect("response");
        read_body_within(response, max, "WMS GetCapabilities").await
    }

    /// The body used to be read whole, whatever its length.
    #[tokio::test]
    async fn response_body_over_the_cap_is_refused() {
        let refused = |result: Result<Vec<u8>, GeoError>| match result {
            Err(GeoError::Http(message)) => {
                message == "WMS GetCapabilities response is larger than 1000 bytes"
            }
            _ => false,
        };
        // Announced: refused on the header.
        let mut announced = b"HTTP/1.1 200 OK\r\nContent-Length: 1001\r\n\r\n".to_vec();
        announced.extend_from_slice(&[b'x'; 1001]);
        assert!(refused(read_within(announced, 1000).await));

        // Not announced, 4 MB sent until the connection closes: refused
        // while reading.
        let mut unannounced = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        unannounced.extend_from_slice(&vec![b'x'; 4 << 20]);
        assert!(refused(read_within(unannounced, 1000).await));

        // Exactly the cap is read.
        let mut exact = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        exact.extend_from_slice(&[b'x'; 1000]);
        let body = read_within(exact, 1000).await.expect("body");
        assert_eq!(body, vec![b'x'; 1000]);
    }
}
