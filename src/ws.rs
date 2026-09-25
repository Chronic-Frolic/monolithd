//! A small WebSocket client for loopback JSON protocols (Steam's CEF DevTools).
//!
//! Handles what a DevTools peer can send: 7-, 16- and 64-bit lengths, continuation frames,
//! ping (answered with pong) and close. `qlc.rs` keeps its own minimal client; this one is
//! separate so the production QLC+ link is not touched.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Largest message accepted; Steam's download overview with its history is about 20 KB.
const MAX_MESSAGE: usize = 4 * 1024 * 1024;
const MAX_HANDSHAKE: usize = 16 * 1024;

pub struct WebSocket {
    stream: TcpStream,
    buffer: Vec<u8>,
    /// A fragmented message being reassembled. Kept here, not in `read_text`, so a caller
    /// may drop a pending `read_text` (for example inside `tokio::select!`) without losing it.
    partial: Option<Vec<u8>>,
}

/// One frame parsed from the front of a buffer.
#[derive(Debug, PartialEq)]
struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
    used: usize,
}

impl WebSocket {
    /// Connect to `host:port` and upgrade `path` (for example `/devtools/page/ID`).
    pub async fn connect(host: &str, path: &str) -> Result<Self, String> {
        let mut stream = TcpStream::connect(host).await.map_err(|error| format!("connect {host}: {error}"))?;
        let key = "bW9ub2xpdGhkLXN0ZWFtLTE2"; // any 16-byte base64 value; the peer only echoes a hash of it
        let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n");
        stream.write_all(request.as_bytes()).await.map_err(|error| format!("send handshake: {error}"))?;
        let mut response = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = loop {
            if let Some(position) = response.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            let count = stream.read(&mut chunk).await.map_err(|error| format!("read handshake: {error}"))?;
            if count == 0 {
                return Err("peer closed during the WebSocket handshake".to_owned());
            }
            response.extend_from_slice(&chunk[..count]);
            if response.len() > MAX_HANDSHAKE {
                return Err("WebSocket handshake reply too large".to_owned());
            }
        };
        if !response.starts_with(b"HTTP/1.1 101") {
            return Err(format!("WebSocket upgrade refused: {}", String::from_utf8_lossy(&response[..end.min(200)]).lines().next().unwrap_or("")));
        }
        Ok(Self { stream, buffer: response.split_off(end), partial: None })
    }

    pub async fn send_text(&mut self, text: &str) -> Result<(), String> {
        self.stream.write_all(&encode_frame(0x1, text.as_bytes(), mask())).await.map_err(|error| format!("send: {error}"))
    }

    /// The next complete text message. Pings are answered; a close ends the connection.
    /// Cancel-safe between frames: unread bytes and a partial message stay in `self`.
    pub async fn read_text(&mut self) -> Result<String, String> {
        loop {
            while let Some(frame) = parse_frame(&self.buffer)? {
                self.buffer.drain(..frame.used);
                match (frame.opcode, self.partial.as_mut()) {
                    (0x1 | 0x2, None) => self.partial = Some(frame.payload),
                    (0x0, Some(message)) => message.extend_from_slice(&frame.payload),
                    (0x9, _) => {
                        self.stream.write_all(&encode_frame(0xA, &frame.payload, mask())).await.map_err(|error| format!("send pong: {error}"))?;
                        continue;
                    }
                    (0xA, _) => continue,
                    (0x8, _) => return Err("peer closed the WebSocket".to_owned()),
                    (opcode, _) => return Err(format!("unexpected WebSocket frame opcode {opcode:#x}")),
                }
                if self.partial.as_ref().is_some_and(|message| message.len() > MAX_MESSAGE) {
                    return Err("WebSocket message too large".to_owned());
                }
                if frame.fin {
                    let message = self.partial.take().unwrap_or_default();
                    return String::from_utf8(message).map_err(|_| "WebSocket text was not UTF-8".to_owned());
                }
            }
            let mut chunk = [0u8; 16384];
            let count = self.stream.read(&mut chunk).await.map_err(|error| format!("read: {error}"))?;
            if count == 0 {
                return Err("peer closed the connection".to_owned());
            }
            self.buffer.extend_from_slice(&chunk[..count]);
        }
    }
}

fn mask() -> [u8; 4] {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.subsec_nanos());
    nanos.to_le_bytes()
}

/// A masked client frame (clients must mask every frame).
fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    match payload.len() {
        length @ 0..=125 => frame.push(0x80 | length as u8),
        length @ 126..=65535 => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        }
        length => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(length as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ mask[index % 4]));
    frame
}

/// Parse one frame from the front of `buffer`; `None` if it is not complete yet.
fn parse_frame(buffer: &[u8]) -> Result<Option<Frame>, String> {
    if buffer.len() < 2 {
        return Ok(None);
    }
    let fin = buffer[0] & 0x80 != 0;
    let opcode = buffer[0] & 0x0f;
    let masked = buffer[1] & 0x80 != 0;
    let (length, mut offset) = match buffer[1] & 0x7f {
        126 if buffer.len() >= 4 => (u16::from_be_bytes([buffer[2], buffer[3]]) as usize, 4),
        127 if buffer.len() >= 10 => (usize::try_from(u64::from_be_bytes(buffer[2..10].try_into().unwrap())).unwrap_or(usize::MAX), 10),
        126 | 127 => return Ok(None),
        short => (short as usize, 2),
    };
    if length > MAX_MESSAGE {
        return Err(format!("WebSocket frame of {length} bytes is too large"));
    }
    let key = if masked {
        if buffer.len() < offset + 4 {
            return Ok(None);
        }
        let key = [buffer[offset], buffer[offset + 1], buffer[offset + 2], buffer[offset + 3]];
        offset += 4;
        Some(key)
    } else {
        None
    };
    if buffer.len() < offset + length {
        return Ok(None);
    }
    let mut payload = buffer[offset..offset + length].to_vec();
    if let Some(key) = key {
        payload.iter_mut().enumerate().for_each(|(index, byte)| *byte ^= key[index % 4]);
    }
    Ok(Some(Frame { fin, opcode, payload, used: offset + length }))
}

/// `GET path` over plain HTTP on loopback, returning the body (DevTools' `/json` target list).
/// Reads exactly `Content-Length` bytes: Chromium's DevTools server keeps the connection open
/// even when asked to close it, so reading to the end would never finish.
pub async fn http_get(host: &str, path: &str) -> Result<String, String> {
    let mut stream = TcpStream::connect(host).await.map_err(|error| format!("connect {host}: {error}"))?;
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .map_err(|error| format!("send GET {path}: {error}"))?;
    let mut response = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        if let Some(end) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&response[..end]).into_owned();
            if !head.starts_with("HTTP/1.1 200") {
                return Err(format!("GET {path}: {}", head.lines().next().unwrap_or("")));
            }
            let length = head
                .lines()
                .find_map(|line| line.split_once(':').filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length")).and_then(|(_, value)| value.trim().parse::<usize>().ok()))
                .ok_or_else(|| format!("GET {path}: no Content-Length"))?;
            if length > MAX_MESSAGE {
                return Err(format!("GET {path}: reply of {length} bytes is too large"));
            }
            while response.len() < end + 4 + length {
                let count = stream.read(&mut chunk).await.map_err(|error| format!("read GET {path}: {error}"))?;
                if count == 0 {
                    return Err(format!("GET {path}: reply cut short"));
                }
                response.extend_from_slice(&chunk[..count]);
            }
            return String::from_utf8(response[end + 4..end + 4 + length].to_vec()).map_err(|_| format!("GET {path}: body is not UTF-8"));
        }
        let count = stream.read(&mut chunk).await.map_err(|error| format!("read GET {path}: {error}"))?;
        if count == 0 {
            return Err(format!("GET {path}: connection closed before the headers"));
        }
        response.extend_from_slice(&chunk[..count]);
        if response.len() > MAX_HANDSHAKE {
            return Err(format!("GET {path}: headers too large"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unmask(frame: &[u8]) -> Frame {
        parse_frame(frame).unwrap().unwrap()
    }

    #[test]
    fn frames_round_trip_at_every_length_class() {
        for length in [0, 125, 126, 65535, 65536, 200_000] {
            let payload: Vec<u8> = (0..length).map(|index| (index % 251) as u8).collect();
            let frame = encode_frame(0x1, &payload, [1, 2, 3, 4]);
            let parsed = unmask(&frame);
            assert_eq!((parsed.fin, parsed.opcode, parsed.used), (true, 0x1, frame.len()), "length {length}");
            assert_eq!(parsed.payload, payload, "length {length}");
        }
    }

    #[test]
    fn incomplete_frames_wait_for_more_bytes() {
        let frame = encode_frame(0x1, &[7; 300], [9, 9, 9, 9]);
        for cut in [1, 3, 7, frame.len() - 1] {
            assert_eq!(parse_frame(&frame[..cut]).unwrap(), None, "cut at {cut}");
        }
    }

    #[test]
    fn unmasked_server_frames_parse() {
        let mut frame = vec![0x01, 3];
        frame.extend_from_slice(b"abc");
        frame.extend_from_slice(&[0x80, 2]);
        frame.extend_from_slice(b"de");
        let first = parse_frame(&frame).unwrap().unwrap();
        assert_eq!((first.fin, first.opcode, first.payload.as_slice()), (false, 0x1, &b"abc"[..]));
        let second = parse_frame(&frame[first.used..]).unwrap().unwrap();
        assert_eq!((second.fin, second.opcode, second.payload.as_slice()), (true, 0x0, &b"de"[..]));
    }

    #[test]
    fn oversized_frames_are_refused() {
        let mut frame = vec![0x81, 127];
        frame.extend_from_slice(&(u64::MAX).to_be_bytes());
        assert!(parse_frame(&frame).is_err());
    }

    #[tokio::test]
    async fn http_get_reads_the_body_without_waiting_for_close() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut chunk = [0u8; 1024];
            let _ = stream.read(&mut chunk).await.unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n[1, 2]\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await; // like DevTools: never closes
        });
        let body = tokio::time::timeout(std::time::Duration::from_secs(2), http_get(&address, "/json")).await.expect("must not wait for close").unwrap();
        assert_eq!(body, "[1, 2]\n");
        server.abort();
    }

    #[tokio::test]
    async fn reads_fragmented_messages_and_answers_pings() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..count]);
            }
            stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n").await.unwrap();
            // A ping, then "hello world" split over a text frame and a continuation.
            stream.write_all(&[0x89, 2, b'p', b'!', 0x01, 6]).await.unwrap();
            stream.write_all(b"hello ").await.unwrap();
            stream.write_all(&[0x80, 5]).await.unwrap();
            stream.write_all(b"world").await.unwrap();
            let mut reply = vec![0u8; 8];
            stream.read_exact(&mut reply).await.unwrap();
            let pong = parse_frame(&reply).unwrap().unwrap();
            assert_eq!((pong.opcode, pong.payload.as_slice()), (0xA, &b"p!"[..]));
            stream.write_all(&[0x88, 0]).await.unwrap();
        });
        let mut socket = WebSocket::connect(&address, "/devtools/page/X").await.unwrap();
        assert_eq!(socket.read_text().await.unwrap(), "hello world");
        assert!(socket.read_text().await.unwrap_err().contains("closed"));
        server.await.unwrap();
    }
}
