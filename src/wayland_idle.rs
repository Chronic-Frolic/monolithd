//! A minimal Wayland client for one question: has anyone touched this machine lately?
//!
//! It speaks the Wayland wire protocol directly (no libraries): binds `wl_seat` and
//! `ext_idle_notifier_v1`, and asks for an input-idle notification. Version 2's
//! `get_input_idle_notification` counts only real input and ignores idle inhibitors held by
//! video players and similar apps; version 1's `get_idle_notification` is the fallback.
//! KWin 6 advertises version 2 (measured 2026-09-25 on White Monolith).

use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const DISPLAY: u32 = 1;
const REGISTRY: u32 = 2;
const SYNC: u32 = 3;
const SEAT: u32 = 4;
const NOTIFIER: u32 = 5;
const NOTIFICATION: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEvent {
    /// No input for the requested time.
    Idled,
    /// Input again after `Idled`.
    Resumed,
}

/// One message on the wire: object, opcode, argument bytes.
#[derive(Debug, PartialEq)]
struct Message {
    object: u32,
    opcode: u16,
    args: Vec<u8>,
}

fn encode(object: u32, opcode: u16, args: &[u8]) -> Vec<u8> {
    let size = (8 + args.len()) as u32;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&object.to_le_bytes());
    out.extend_from_slice(&((size << 16) | u32::from(opcode)).to_le_bytes());
    out.extend_from_slice(args);
    out
}

fn uint(value: u32) -> [u8; 4] {
    value.to_le_bytes()
}

fn string(text: &str) -> Vec<u8> {
    let mut out = uint(text.len() as u32 + 1).to_vec();
    out.extend_from_slice(text.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

/// Parse one message from the front of `buffer`, returning it and the bytes it used.
fn decode(buffer: &[u8]) -> Result<Option<(Message, usize)>, String> {
    if buffer.len() < 8 {
        return Ok(None);
    }
    let object = u32::from_le_bytes(buffer[0..4].try_into().unwrap());
    let word = u32::from_le_bytes(buffer[4..8].try_into().unwrap());
    let size = (word >> 16) as usize;
    if size < 8 {
        return Err(format!("malformed Wayland message of {size} bytes"));
    }
    if buffer.len() < size {
        return Ok(None);
    }
    Ok(Some((Message { object, opcode: (word & 0xffff) as u16, args: buffer[8..size].to_vec() }, size)))
}

fn read_uint(args: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(args.get(offset..offset + 4)?.try_into().ok()?))
}

/// A string argument at `offset`, and the offset after it.
fn read_string(args: &[u8], offset: usize) -> Option<(String, usize)> {
    let length = read_uint(args, offset)? as usize;
    let text = args.get(offset + 4..offset + 4 + length.checked_sub(1)?)?;
    Some((String::from_utf8_lossy(text).into_owned(), offset + 4 + length.div_ceil(4) * 4))
}

/// Compositor sockets in the runtime directory: KDE's `wayland-N`, gamescope's `gamescope-N`.
pub fn sockets(runtime: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(runtime)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
            (name.starts_with("wayland-") || name.starts_with("gamescope-")) && !name.ends_with(".lock")
        })
        .collect();
    found.sort();
    found
}

pub struct IdleWatch {
    stream: UnixStream,
    buffer: Vec<u8>,
    pub version: u32,
}

impl IdleWatch {
    /// Connect to the compositor at `socket` and ask to hear about `timeout_ms` of no input.
    pub async fn connect(socket: &Path, timeout_ms: u32) -> Result<Self, String> {
        let mut stream = UnixStream::connect(socket).await.map_err(|error| format!("connect {}: {error}", socket.display()))?;
        let mut hello = encode(DISPLAY, 1, &uint(REGISTRY));
        hello.extend(encode(DISPLAY, 0, &uint(SYNC)));
        stream.write_all(&hello).await.map_err(|error| format!("send: {error}"))?;
        let mut watch = Self { stream, buffer: Vec::new(), version: 0 };
        let (mut seat, mut notifier) = (None, None);
        loop {
            let message = watch.next().await?;
            match (message.object, message.opcode) {
                (REGISTRY, 0) => {
                    let name = read_uint(&message.args, 0).ok_or("short wl_registry.global")?;
                    let (interface, after) = read_string(&message.args, 4).ok_or("short wl_registry.global")?;
                    let version = read_uint(&message.args, after).ok_or("short wl_registry.global")?;
                    match interface.as_str() {
                        "wl_seat" if seat.is_none() => seat = Some(name),
                        "ext_idle_notifier_v1" => notifier = Some((name, version)),
                        _ => {}
                    }
                }
                (SYNC, 0) => break,
                _ => {}
            }
        }
        let seat = seat.ok_or("the compositor has no wl_seat")?;
        let (notifier, version) = notifier.ok_or("the compositor has no ext_idle_notifier_v1")?;
        let version = version.min(2);
        let bind = |name: u32, interface: &str, version: u32, id: u32| {
            let mut args = uint(name).to_vec();
            args.extend(string(interface));
            args.extend(uint(version));
            args.extend(uint(id));
            encode(REGISTRY, 0, &args)
        };
        let mut request = bind(seat, "wl_seat", 1, SEAT);
        request.extend(bind(notifier, "ext_idle_notifier_v1", version, NOTIFIER));
        let mut args = uint(NOTIFICATION).to_vec();
        args.extend(uint(timeout_ms));
        args.extend(uint(SEAT));
        // Opcode 2 = get_input_idle_notification (v2, ignores inhibitors); 1 = get_idle_notification.
        request.extend(encode(NOTIFIER, if version >= 2 { 2 } else { 1 }, &args));
        watch.stream.write_all(&request).await.map_err(|error| format!("send: {error}"))?;
        watch.version = version;
        Ok(watch)
    }

    async fn next(&mut self) -> Result<Message, String> {
        loop {
            if let Some((message, used)) = decode(&self.buffer)? {
                self.buffer.drain(..used);
                if message.object == DISPLAY && message.opcode == 0 {
                    let text = read_string(&message.args, 8).map(|(text, _)| text).unwrap_or_default();
                    return Err(format!("Wayland protocol error: {text}"));
                }
                return Ok(message);
            }
            let mut chunk = [0u8; 4096];
            let count = self.stream.read(&mut chunk).await.map_err(|error| format!("read: {error}"))?;
            if count == 0 {
                return Err("the compositor closed the connection".to_owned());
            }
            self.buffer.extend_from_slice(&chunk[..count]);
        }
    }

    /// The next idle or resume event. Cancel-safe: unread bytes stay in `self`.
    pub async fn event(&mut self) -> Result<IdleEvent, String> {
        loop {
            let message = self.next().await?;
            if message.object == NOTIFICATION {
                match message.opcode {
                    0 => return Ok(IdleEvent::Idled),
                    1 => return Ok(IdleEvent::Resumed),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_with_padded_strings() {
        let mut args = uint(21).to_vec();
        args.extend(string("ext_idle_notifier_v1"));
        args.extend(uint(2));
        let wire = encode(REGISTRY, 0, &args);
        assert_eq!(wire.len() % 4, 0);
        let (message, used) = decode(&wire).unwrap().unwrap();
        assert_eq!((message.object, message.opcode, used), (REGISTRY, 0, wire.len()));
        let (interface, after) = read_string(&message.args, 4).unwrap();
        assert_eq!(interface, "ext_idle_notifier_v1");
        assert_eq!(read_uint(&message.args, after), Some(2));
        assert_eq!(decode(&wire[..wire.len() - 1]).unwrap(), None, "incomplete messages wait");
    }

    #[test]
    fn string_padding_matches_the_protocol() {
        assert_eq!(string("abc").len(), 8, "4-byte length + 'abc\\0'");
        assert_eq!(string("abcd").len(), 12, "4 + 'abcd\\0' padded to 8");
    }

    #[test]
    fn finds_kde_and_gamescope_sockets_but_not_locks() {
        let directory = std::env::temp_dir().join(format!("wayland-sockets-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        for name in ["wayland-0", "wayland-0.lock", "gamescope-0", "pipewire-0", "bus"] {
            std::fs::write(directory.join(name), b"").unwrap();
        }
        let names: Vec<String> = sockets(&directory).iter().map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["gamescope-0", "wayland-0"]);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// A fake compositor: announces a seat and the idle notifier, then reports idle and resume.
    #[tokio::test]
    async fn subscribes_and_reports_idle_then_resume() {
        use tokio::net::UnixListener;
        let path = std::env::temp_dir().join(format!("wayland-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut chunk = [0u8; 256];
            let _ = stream.read(&mut chunk).await.unwrap();
            let global = |name: u32, interface: &str, version: u32| {
                let mut args = uint(name).to_vec();
                args.extend(string(interface));
                args.extend(uint(version));
                encode(REGISTRY, 0, &args)
            };
            let mut reply = global(4, "wl_seat", 9);
            reply.extend(global(21, "ext_idle_notifier_v1", 2));
            reply.extend(encode(SYNC, 0, &uint(0)));
            stream.write_all(&reply).await.unwrap();
            let mut requests = Vec::new();
            while requests.len() < 3 {
                let count = stream.read(&mut chunk).await.unwrap();
                let mut bytes = &chunk[..count];
                while let Some((message, used)) = decode(bytes).unwrap() {
                    requests.push((message.object, message.opcode));
                    bytes = &bytes[used..];
                }
            }
            assert_eq!(requests, vec![(REGISTRY, 0), (REGISTRY, 0), (NOTIFIER, 2)], "binds, then the v2 input-idle request");
            let mut events = encode(NOTIFICATION, 0, &[]);
            events.extend(encode(NOTIFICATION, 1, &[]));
            stream.write_all(&events).await.unwrap();
        });
        let mut watch = IdleWatch::connect(&path, 60_000).await.unwrap();
        assert_eq!(watch.version, 2);
        assert_eq!(watch.event().await.unwrap(), IdleEvent::Idled);
        assert_eq!(watch.event().await.unwrap(), IdleEvent::Resumed);
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
