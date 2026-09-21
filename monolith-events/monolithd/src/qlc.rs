use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

const HANDSHAKE_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;
const MAX_REPLY_BYTES: usize = 64 * 1024;
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// What QLC+ reports for a Function via `getFunctionStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionStatus {
    Running,
    Stopped,
    Undefined,
}

impl FunctionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Stopped => "Stopped",
            Self::Undefined => "Undefined",
        }
    }
}

/// Single-attempt client for QLC+'s local WebSocket API.
///
/// The production lighting stack places QLC+ and this client in the same
/// private network namespace.  The listener must therefore remain loopback.
#[derive(Clone, Debug)]
pub struct Client {
    listener: String,
}

impl Client {
    pub fn new(listener: &str) -> Self {
        Self { listener: listener.to_owned() }
    }

    /// Send several start/stop commands in one write over one connection, so
    /// QLC+ sees them together and applies them in order within one pass.
    pub async fn send_batch(&self, commands: &[(u32, bool)]) -> Result<(), String> {
        set_running_batch_once(parse_listener(&self.listener)?, commands).await
    }

    pub async fn status(&self, function_id: u32) -> Result<FunctionStatus, String> {
        status_once(parse_listener(&self.listener)?, function_id).await
    }
}

fn parse_listener(listener: &str) -> Result<SocketAddr, String> {
    let address: SocketAddr = listener
        .parse()
        .map_err(|error| format!("parse qlc_e131.web_listener: {error}"))?;
    if !address.ip().is_loopback() {
        return Err("qlc_e131.web_listener must be a loopback address".to_owned());
    }
    Ok(address)
}

/// Connect and complete the WebSocket handshake. Returns the stream plus any
/// bytes that arrived after the handshake headers (the start of the first frame).
async fn open(address: SocketAddr) -> Result<(TcpStream, Vec<u8>), String> {
    let mut stream = timeout(Duration::from_secs(2), TcpStream::connect(address))
        .await
        .map_err(|_| format!("connect {address}: timed out"))?
        .map_err(|error| format!("connect {address}: {error}"))?;

    let request = format!(
        "GET /qlcplusWS HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {HANDSHAKE_KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| format!("write QLC+ WebSocket handshake: {error}"))?;

    let mut response = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(position) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let count = timeout(Duration::from_secs(2), stream.read(&mut chunk))
            .await
            .map_err(|_| "read QLC+ WebSocket handshake: timed out".to_owned())?
            .map_err(|error| format!("read QLC+ WebSocket handshake: {error}"))?;
        if count == 0 {
            return Err("QLC+ closed the WebSocket handshake".to_owned());
        }
        response.extend_from_slice(&chunk[..count]);
        if response.len() > MAX_HANDSHAKE_BYTES {
            return Err("QLC+ WebSocket handshake exceeded 16 KiB".to_owned());
        }
    };
    if !response.starts_with(b"HTTP/1.1 101") {
        return Err(format!(
            "QLC+ declined the WebSocket handshake: {}",
            String::from_utf8_lossy(&response)
        ));
    }
    Ok((stream, response.split_off(header_end)))
}

async fn set_running_batch_once(address: SocketAddr, commands: &[(u32, bool)]) -> Result<(), String> {
    let (mut stream, _) = open(address).await?;
    let mut frames = Vec::new();
    for (function_id, running) in commands {
        let command = format!("QLC+API|setFunctionStatus|{function_id}|{}", u8::from(*running));
        frames.extend_from_slice(&masked_text_frame(command.as_bytes()));
    }
    stream
        .write_all(&frames)
        .await
        .map_err(|error| format!("send QLC+ commands {commands:?}: {error}"))?;
    stream
        .shutdown()
        .await
        .map_err(|error| format!("close QLC+ WebSocket: {error}"))?;
    Ok(())
}

async fn status_once(address: SocketAddr, function_id: u32) -> Result<FunctionStatus, String> {
    let (mut stream, mut buffer) = open(address).await?;
    let command = format!("QLC+API|getFunctionStatus|{function_id}");
    stream
        .write_all(&masked_text_frame(command.as_bytes()))
        .await
        .map_err(|error| format!("send QLC+ status query for Function {function_id}: {error}"))?;

    let read_reply = async {
        let mut chunk = [0u8; 1024];
        loop {
            while let Some((opcode, payload, used)) = parse_server_frame(&buffer)? {
                buffer.drain(..used);
                if opcode == 0x1 {
                    if let Some(status) = parse_status_text(&String::from_utf8_lossy(&payload)) {
                        return Ok(status);
                    }
                }
            }
            let count = stream
                .read(&mut chunk)
                .await
                .map_err(|error| format!("read QLC+ status reply: {error}"))?;
            if count == 0 {
                return Err("QLC+ closed the connection before replying".to_owned());
            }
            buffer.extend_from_slice(&chunk[..count]);
            if buffer.len() > MAX_REPLY_BYTES {
                return Err("QLC+ status reply exceeded 64 KiB".to_owned());
            }
        }
    };
    timeout(REPLY_TIMEOUT, read_reply)
        .await
        .map_err(|_| format!("QLC+ did not answer the status query for Function {function_id}"))?
}

/// `QLC+API|getFunctionStatus|Running` and its siblings; anything else is not our reply.
fn parse_status_text(text: &str) -> Option<FunctionStatus> {
    let mut parts = text.trim().split('|');
    if parts.next()? != "QLC+API" || parts.next()? != "getFunctionStatus" {
        return None;
    }
    match parts.last()? {
        "Running" => Some(FunctionStatus::Running),
        "Stopped" => Some(FunctionStatus::Stopped),
        "Undefined" => Some(FunctionStatus::Undefined),
        _ => None,
    }
}

/// Parse one WebSocket frame from the front of `buffer`.
/// Returns `(opcode, payload, bytes_consumed)`, or `None` if the frame is incomplete.
fn parse_server_frame(buffer: &[u8]) -> Result<Option<(u8, Vec<u8>, usize)>, String> {
    if buffer.len() < 2 {
        return Ok(None);
    }
    let opcode = buffer[0] & 0x0f;
    let masked = buffer[1] & 0x80 != 0;
    let (length, mut offset) = match buffer[1] & 0x7f {
        126 if buffer.len() >= 4 => (u16::from_be_bytes([buffer[2], buffer[3]]) as usize, 4),
        126 => return Ok(None),
        127 if buffer.len() >= 10 => {
            let length = u64::from_be_bytes(buffer[2..10].try_into().unwrap());
            (usize::try_from(length).unwrap_or(usize::MAX), 10)
        }
        127 => return Ok(None),
        short => (short as usize, 2),
    };
    if length > MAX_REPLY_BYTES {
        return Err(format!("QLC+ sent a {length}-byte frame"));
    }
    let mask = if masked {
        if buffer.len() < offset + 4 {
            return Ok(None);
        }
        let mask = [buffer[offset], buffer[offset + 1], buffer[offset + 2], buffer[offset + 3]];
        offset += 4;
        Some(mask)
    } else {
        None
    };
    if buffer.len() < offset + length {
        return Ok(None);
    }
    let mut payload = buffer[offset..offset + length].to_vec();
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok(Some((opcode, payload, offset + length)))
}

fn masked_text_frame(payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() < 126, "Monolith QLC commands fit in a short WebSocket frame");
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    let mut frame = Vec::with_capacity(payload.len() + 6);
    frame.push(0x81); // FIN + text
    frame.push(0x80 | payload.len() as u8);
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(index, byte)| byte ^ mask[index % mask.len()]));
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn client_commands_are_masked_text_frames() {
        let payload = b"QLC+API|setFunctionStatus|106|1";
        let frame = masked_text_frame(payload);
        assert_eq!(frame[0], 0x81);
        assert_eq!(frame[1], 0x80 | payload.len() as u8);
        let mask = &frame[2..6];
        let restored: Vec<u8> = frame[6..]
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()])
            .collect();
        assert_eq!(restored, payload);
    }

    #[test]
    fn parses_short_extended_and_partial_server_frames() {
        let mut short = vec![0x81, 5];
        short.extend_from_slice(b"hello");
        assert_eq!(parse_server_frame(&short).unwrap(), Some((1, b"hello".to_vec(), 7)));

        let mut extended = vec![0x81, 126, 0, 200];
        extended.extend(std::iter::repeat(b'x').take(200));
        let (opcode, payload, used) = parse_server_frame(&extended).unwrap().unwrap();
        assert_eq!((opcode, payload.len(), used), (1, 200, 204));

        assert_eq!(parse_server_frame(&short[..4]).unwrap(), None);
        assert_eq!(parse_server_frame(&[0x81]).unwrap(), None);
        assert!(parse_server_frame(&[0x81, 127, 0, 0, 0, 0, 0, 2, 0, 0]).is_err());
    }

    #[test]
    fn parses_only_function_status_replies() {
        assert_eq!(parse_status_text("QLC+API|getFunctionStatus|Running"), Some(FunctionStatus::Running));
        assert_eq!(parse_status_text("QLC+API|getFunctionStatus|Stopped"), Some(FunctionStatus::Stopped));
        assert_eq!(parse_status_text("QLC+API|getFunctionStatus|Undefined"), Some(FunctionStatus::Undefined));
        assert_eq!(parse_status_text("QLC+API|getFunctionStatus|Nonsense"), None);
        assert_eq!(parse_status_text("QLC+API|getFunctionsList|1|x"), None);
        assert_eq!(parse_status_text("hello"), None);
    }

    /// A one-shot fake QLC+ WebSocket server. Returns the address and a handle
    /// yielding the unmasked command it received.
    async fn fake_qlc(reply: Option<&'static str>) -> (SocketAddr, tokio::task::JoinHandle<String>) {
        let (address, handle) = fake_qlc_batch(reply, 1).await;
        (address, tokio::spawn(async move { handle.await.unwrap().remove(0) }))
    }

    /// Like `fake_qlc`, but reads `count` command frames.
    async fn fake_qlc_batch(reply: Option<&'static str>, count: usize) -> (SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..count]);
            }
            stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n").await.unwrap();
            let mut frame = Vec::new();
            let mut commands = Vec::new();
            while commands.len() < count {
                while let Some((_, payload, used)) = parse_server_frame(&frame).unwrap() {
                    frame.drain(..used);
                    commands.push(String::from_utf8(payload).unwrap());
                }
                if commands.len() < count {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "client closed before sending every command");
                    frame.extend_from_slice(&chunk[..read]);
                }
            }
            if let Some(reply) = reply {
                let mut out = vec![0x81, reply.len() as u8];
                out.extend_from_slice(reply.as_bytes());
                stream.write_all(&out).await.unwrap();
            }
            commands
        });
        (address, handle)
    }

    #[tokio::test]
    async fn sends_start_and_stop_commands() {
        for (running, expected) in [(true, "QLC+API|setFunctionStatus|109|1"), (false, "QLC+API|setFunctionStatus|109|0")] {
            let (address, server) = fake_qlc(None).await;
            set_running_batch_once(address, &[(109, running)]).await.unwrap();
            assert_eq!(server.await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn sends_a_batch_of_commands_in_order_over_one_connection() {
        let (address, server) = fake_qlc_batch(None, 3).await;
        set_running_batch_once(address, &[(109, false), (16, true), (112, true)]).await.unwrap();
        assert_eq!(
            server.await.unwrap(),
            vec!["QLC+API|setFunctionStatus|109|0", "QLC+API|setFunctionStatus|16|1", "QLC+API|setFunctionStatus|112|1"]
        );
    }

    #[tokio::test]
    async fn reads_a_status_reply() {
        let (address, server) = fake_qlc(Some("QLC+API|getFunctionStatus|Running")).await;
        assert_eq!(status_once(address, 109).await.unwrap(), FunctionStatus::Running);
        assert_eq!(server.await.unwrap(), "QLC+API|getFunctionStatus|109");
    }

    #[tokio::test]
    async fn refuses_non_loopback_listeners() {
        assert!(Client::new("192.168.1.10:9999").status(1).await.is_err());
    }
}
