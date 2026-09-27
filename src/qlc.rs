use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
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

/// One message of a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Start or stop a Function.
    Set { function: u32, running: bool },
    /// Start a Chaser at a given step through its Cue List widget: select the step on the
    /// stopped Cue List, then play. QLC+ 5.2.2 starts a stopped Cue List's Chaser at the
    /// selected step; a running Chaser cannot be moved this way (probed 2026-09-26).
    CueStart { cue_list: u32, step: u32 },
}

/// Client for QLC+'s local WebSocket API.
///
/// The production lighting stack places QLC+ and this client in the same
/// private network namespace.  The listener must therefore remain loopback.
///
/// Status queries share one kept-open WebSocket (2026-09-24). Opening one per
/// query made headless QLC+ log `QObject::disconnect: wildcard call disconnects
/// from destroyed signal of QTcpSocket` on every close: about 3 lines/s from the
/// controller's 1 s reconcile, which flushed the 50 MB journal within hours. The
/// link is dropped on any error, so a late reply can never answer a later query.
/// Start/stop batches still use their own short connection; they are rare.
#[derive(Clone, Debug)]
pub struct Client {
    listener: String,
    link: Arc<Mutex<Option<Link>>>,
}

#[derive(Debug)]
struct Link {
    stream: TcpStream,
    /// Bytes received but not yet parsed into frames.
    buffer: Vec<u8>,
}

/// Why a status query failed. Only a connection that broke is worth retrying
/// on a fresh one; a QLC+ that stopped answering would just time out again.
enum Failure {
    Broken(String),
    TimedOut(String),
}

impl Failure {
    fn message(self) -> String {
        match self {
            Self::Broken(message) | Self::TimedOut(message) => message,
        }
    }
}

impl Client {
    pub fn new(listener: &str) -> Self {
        Self { listener: listener.to_owned(), link: Arc::new(Mutex::new(None)) }
    }

    /// Send several start/stop commands in one write over one connection, so
    /// QLC+ sees them together and applies them in order within one pass. (The gateway
    /// sends `send_ops`; this form is kept for the live QLC+ tests.)
    #[cfg(test)]
    pub async fn send_batch(&self, commands: &[(u32, bool)]) -> Result<(), String> {
        let ops: Vec<Op> = commands.iter().map(|&(function, running)| Op::Set { function, running }).collect();
        self.send_ops(&ops).await
    }

    /// Send a mixed batch (starts, stops, Cue List starts at a step) in one write.
    pub async fn send_ops(&self, ops: &[Op]) -> Result<(), String> {
        send_ops_once(parse_listener(&self.listener)?, ops).await
    }

    pub async fn status(&self, function_id: u32) -> Result<FunctionStatus, String> {
        Ok(self.statuses(&[function_id]).await?.remove(0))
    }

    /// Ask for several Functions in one write and read the replies in order;
    /// QLC+ answers each `getFunctionStatus` on a connection in the order sent.
    pub async fn statuses(&self, function_ids: &[u32]) -> Result<Vec<FunctionStatus>, String> {
        if function_ids.is_empty() {
            return Ok(Vec::new());
        }
        let address = parse_listener(&self.listener)?;
        let mut kept = self.link.lock().await;
        if let Some(mut link) = kept.take() {
            match query(&mut link, function_ids).await {
                Ok(statuses) => {
                    *kept = Some(link);
                    return Ok(statuses);
                }
                Err(Failure::TimedOut(message)) => return Err(message),
                // Most often QLC+ restarted and closed the old link: reconnect once.
                Err(Failure::Broken(_)) => {}
            }
        }
        let (stream, buffer) = open(address).await?;
        let mut link = Link { stream, buffer };
        let statuses = query(&mut link, function_ids).await.map_err(Failure::message)?;
        *kept = Some(link);
        Ok(statuses)
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

/// The WebSocket text messages for one batch, in order.
fn op_messages(ops: &[Op]) -> Vec<String> {
    let mut messages = Vec::new();
    for op in ops {
        match *op {
            Op::Set { function, running } => messages.push(format!("QLC+API|setFunctionStatus|{function}|{}", u8::from(running))),
            Op::CueStart { cue_list, step } => {
                messages.push(format!("{cue_list}|STEP|{step}"));
                messages.push(format!("{cue_list}|PLAY"));
            }
        }
    }
    messages
}

async fn send_ops_once(address: SocketAddr, ops: &[Op]) -> Result<(), String> {
    let (mut stream, _) = open(address).await?;
    let mut frames = Vec::new();
    for message in op_messages(ops) {
        frames.extend_from_slice(&masked_text_frame(message.as_bytes()));
    }
    stream
        .write_all(&frames)
        .await
        .map_err(|error| format!("send QLC+ commands {ops:?}: {error}"))?;
    stream
        .shutdown()
        .await
        .map_err(|error| format!("close QLC+ WebSocket: {error}"))?;
    Ok(())
}

async fn query(link: &mut Link, function_ids: &[u32]) -> Result<Vec<FunctionStatus>, Failure> {
    let mut frames = Vec::new();
    for function_id in function_ids {
        frames.extend_from_slice(&masked_text_frame(format!("QLC+API|getFunctionStatus|{function_id}").as_bytes()));
    }
    link.stream
        .write_all(&frames)
        .await
        .map_err(|error| Failure::Broken(format!("send QLC+ status query for Functions {function_ids:?}: {error}")))?;

    let read_replies = async {
        let mut statuses = Vec::with_capacity(function_ids.len());
        let mut chunk = [0u8; 1024];
        loop {
            while let Some((opcode, payload, used)) = parse_server_frame(&link.buffer).map_err(Failure::Broken)? {
                link.buffer.drain(..used);
                match opcode {
                    0x1 => {
                        if let Some(status) = parse_status_text(&String::from_utf8_lossy(&payload)) {
                            statuses.push(status);
                            if statuses.len() == function_ids.len() {
                                return Ok(statuses);
                            }
                        }
                    }
                    0x8 => return Err(Failure::Broken("QLC+ closed the WebSocket".to_owned())),
                    _ => {}
                }
            }
            let count = link.stream
                .read(&mut chunk)
                .await
                .map_err(|error| Failure::Broken(format!("read QLC+ status reply: {error}")))?;
            if count == 0 {
                return Err(Failure::Broken("QLC+ closed the connection before replying".to_owned()));
            }
            link.buffer.extend_from_slice(&chunk[..count]);
            if link.buffer.len() > MAX_REPLY_BYTES {
                return Err(Failure::Broken("QLC+ status reply exceeded 64 KiB".to_owned()));
            }
        }
    };
    timeout(REPLY_TIMEOUT, read_replies)
        .await
        .map_err(|_| Failure::TimedOut(format!("QLC+ did not answer the status query for Functions {function_ids:?}")))?
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
            send_ops_once(address, &[Op::Set { function: 109, running }]).await.unwrap();
            assert_eq!(server.await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn sends_a_batch_of_commands_in_order_over_one_connection() {
        let (address, server) = fake_qlc_batch(None, 3).await;
        send_ops_once(address, &[Op::Set { function: 109, running: false }, Op::Set { function: 16, running: true }, Op::Set { function: 112, running: true }]).await.unwrap();
        assert_eq!(
            server.await.unwrap(),
            vec!["QLC+API|setFunctionStatus|109|0", "QLC+API|setFunctionStatus|16|1", "QLC+API|setFunctionStatus|112|1"]
        );
    }

    #[tokio::test]
    async fn reads_a_status_reply() {
        let (address, server) = fake_qlc(Some("QLC+API|getFunctionStatus|Running")).await;
        assert_eq!(Client::new(&address.to_string()).status(109).await.unwrap(), FunctionStatus::Running);
        assert_eq!(server.await.unwrap(), "QLC+API|getFunctionStatus|109");
    }

    /// A fake QLC+ that answers each status query with the Function's parity
    /// (even `Running`, odd `Stopped`) after an unrelated broadcast frame, and
    /// closes each connection after `per_connection` answers. Counts connections.
    async fn fake_qlc_server(per_connection: usize) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        let count = stream.read(&mut chunk).await.unwrap();
                        if count == 0 { return; }
                        buffer.extend_from_slice(&chunk[..count]);
                    }
                    buffer.clear();
                    stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n").await.unwrap();
                    let mut answered = 0;
                    while answered < per_connection {
                        while let Some((_, payload, used)) = parse_server_frame(&buffer).unwrap() {
                            buffer.drain(..used);
                            let id: u32 = String::from_utf8(payload).unwrap().rsplit('|').next().unwrap().parse().unwrap();
                            let state = if id % 2 == 0 { "Running" } else { "Stopped" };
                            for text in ["FUNCTION|broadcast".to_owned(), format!("QLC+API|getFunctionStatus|{state}")] {
                                let mut out = vec![0x81, text.len() as u8];
                                out.extend_from_slice(text.as_bytes());
                                stream.write_all(&out).await.unwrap();
                            }
                            answered += 1;
                        }
                        let count = stream.read(&mut chunk).await.unwrap_or(0);
                        if count == 0 { return; }
                        buffer.extend_from_slice(&chunk[..count]);
                    }
                });
            }
        });
        (address, connections)
    }

    #[tokio::test]
    async fn status_queries_share_one_kept_connection() {
        let (address, connections) = fake_qlc_server(usize::MAX).await;
        let client = Client::new(&address);
        for _ in 0..5 {
            assert_eq!(client.status(110).await.unwrap(), FunctionStatus::Running);
        }
        // A clone (as the gateway holds) shares the same link.
        assert_eq!(client.clone().status(111).await.unwrap(), FunctionStatus::Stopped);
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_batch_of_status_queries_is_answered_in_order() {
        let (address, connections) = fake_qlc_server(usize::MAX).await;
        let statuses = Client::new(&address).statuses(&[109, 112, 115, 116]).await.unwrap();
        assert_eq!(statuses, vec![FunctionStatus::Stopped, FunctionStatus::Running, FunctionStatus::Stopped, FunctionStatus::Running]);
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_closed_link_is_replaced_without_failing_the_query() {
        // QLC+ restarting looks like this: the kept link closes between queries.
        let (address, connections) = fake_qlc_server(1).await;
        let client = Client::new(&address);
        assert_eq!(client.status(110).await.unwrap(), FunctionStatus::Running);
        assert_eq!(client.status(110).await.unwrap(), FunctionStatus::Running);
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_silent_qlc_times_out_and_the_link_is_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut chunk = [0u8; 1024];
            let _ = stream.read(&mut chunk).await;
            stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n").await.unwrap();
            // Never answer; hold the connection open.
            loop { if stream.read(&mut chunk).await.unwrap_or(0) == 0 { return; } }
        });
        let client = Client::new(&address);
        assert!(client.status(110).await.unwrap_err().contains("did not answer"));
        assert!(client.link.lock().await.is_none(), "a timed-out link must not be reused");
    }

    /// Against a real QLC+ only: `MONOLITH_QLC_LISTENER=127.0.0.1:9999 <test binary> --ignored live_qlc --test-threads=1`.
    /// It starts and stops Function 109, so point it only at a scratch instance, never production. Both live
    /// tests toggle 109, so run them one at a time.
    #[tokio::test]
    #[ignore]
    async fn live_qlc_answers_batches_in_order_over_a_kept_link() {
        let Ok(listener) = std::env::var("MONOLITH_QLC_LISTENER") else { return };
        let client = Client::new(&listener);
        client.send_batch(&[(109, true)]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let expected = vec![FunctionStatus::Running, FunctionStatus::Stopped, FunctionStatus::Running, FunctionStatus::Stopped];
        for (round, pause) in [1, 75, 1, 0].into_iter().enumerate() {
            assert_eq!(client.statuses(&[109, 112, 109, 115]).await.unwrap(), expected, "round {round}");
            eprintln!("live QLC round {round} ok; pausing {pause} s");
            tokio::time::sleep(Duration::from_secs(pause)).await;
        }
        client.send_batch(&[(109, false)]).await.unwrap();
    }

    /// Against a scratch QLC+ only (see above): how long a commanded state takes to show in
    /// status on the kept link, polled the way the gateway confirms (every 5 ms, 400 polls).
    #[tokio::test]
    #[ignore]
    async fn live_qlc_confirmation_latency_on_a_kept_link() {
        let Ok(listener) = std::env::var("MONOLITH_QLC_LISTENER") else { return };
        let client = Client::new(&listener);
        let mut worst = (0, Duration::ZERO);
        for cycle in 0..100 {
            let running = cycle % 2 == 0;
            let want = if running { FunctionStatus::Running } else { FunctionStatus::Stopped };
            let started = tokio::time::Instant::now();
            client.send_batch(&[(109, running)]).await.unwrap();
            let mut polls = 0;
            while client.status(109).await.unwrap() != want {
                polls += 1;
                assert!(polls < 400, "cycle {cycle}: not confirmed within the gateway's 400 polls");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            worst = worst.max((polls, started.elapsed()));
        }
        eprintln!("live QLC confirmation: worst {} extra polls, {:?} from send to confirmed", worst.0, worst.1);
        client.send_batch(&[(109, false)]).await.unwrap();
    }

    #[tokio::test]
    async fn refuses_non_loopback_listeners() {
        assert!(Client::new("192.168.1.10:9999").status(1).await.is_err());
    }

    #[test]
    fn a_cue_start_selects_the_step_then_plays() {
        let messages = op_messages(&[Op::Set { function: 109, running: false }, Op::CueStart { cue_list: 10000, step: 7 }]);
        assert_eq!(messages, vec!["QLC+API|setFunctionStatus|109|0", "10000|STEP|7", "10000|PLAY"]);
    }
}
