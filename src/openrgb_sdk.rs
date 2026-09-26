//! A small OpenRGB SDK client: connect, list the controllers, put them under direct
//! control, and send colors. Written from OpenRGB's protocol documentation
//! (`Documentation/OpenRGBSDK.md` at tag `release_1.0`, 2026-09-26); it reads protocols 1
//! through 6 and asks for 6, which OpenRGB 1.0 serves.
//!
//! Every packet is a 16-byte header (`ORGB`, device ID, packet ID, body size; integers
//! little-endian) and a body. A protocol 6 server also talks unprompted: it acknowledges
//! every request after answering it, announces its own name, and sends every client an
//! update packet each time any client changes LEDs (measured on OpenRGB 1.0, 2026-09-26:
//! about 20 a second per controller while the lighting stack runs). A reader task drains
//! all of it so the server never stalls on this client. It hands the answer to a request
//! to the one caller waiting for it, and an error acknowledgement surfaces on the next
//! write, since writes do not wait for their acknowledgement.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::time::timeout;

const MAGIC: &[u8; 4] = b"ORGB";
/// The newest protocol this client reads.
const PROTOCOL: u32 = 6;
const CLIENT_NAME: &str = "monolithd";
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Larger than any real packet (a full controller description is a few kilobytes).
const MAX_BODY: usize = 16 << 20;

const REQUEST_CONTROLLER_COUNT: u32 = 0;
const REQUEST_CONTROLLER_DATA: u32 = 1;
const ACK: u32 = 10;
const REQUEST_PROTOCOL_VERSION: u32 = 40;
const SET_CLIENT_NAME: u32 = 50;
const UPDATE_LEDS: u32 = 1050;
const UPDATE_ZONE_LEDS: u32 = 1051;
const SET_CUSTOM_MODE: u32 = 1100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

fn packet(device: u32, id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + body.len());
    out.extend_from_slice(MAGIC);
    for word in [device, id, body.len() as u32] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(body);
    out
}

/// Device ID, packet ID and body size from a packet header.
fn header(bytes: &[u8; 16]) -> Result<(u32, u32, usize), String> {
    if &bytes[..4] != MAGIC {
        return Err(format!("not an OpenRGB packet (header {:02x?})", &bytes[..4]));
    }
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    Ok((word(4), word(8), word(12) as usize))
}

/// The body of an UpdateLEDs (no zone) or UpdateZoneLEDs packet. Its size field counts
/// itself, as the server's own size fields do.
fn colors_body(zone: Option<u32>, colors: &[Color]) -> Vec<u8> {
    let size = 4 + zone.map_or(0, |_| 4) + 2 + 4 * colors.len();
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(size as u32).to_le_bytes());
    if let Some(zone) = zone {
        out.extend_from_slice(&zone.to_le_bytes());
    }
    out.extend_from_slice(&(colors.len() as u16).to_le_bytes());
    for color in colors {
        out.extend_from_slice(&[color.r, color.g, color.b, 0]);
    }
    out
}

// ---------------------------------------------------------------- parsing

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or_else(|| format!("truncated at byte {} (wanted {count} more)", self.at))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    /// A string with a 16-bit length that counts its NUL terminator.
    fn string(&mut self) -> Result<String, String> {
        let length = self.u16()? as usize;
        let bytes = self.take(length)?;
        let text = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
        Ok(String::from_utf8_lossy(text).into_owned())
    }

    fn color(&mut self) -> Result<Color, String> {
        let bytes = self.take(4)?;
        Ok(Color::new(bytes[0], bytes[1], bytes[2]))
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Zone {
    #[allow(dead_code)] // for diagnosis
    pub name: String,
    num_leds: usize,
}

impl Zone {
    pub fn num_leds(&self) -> usize {
        self.num_leds
    }
}

/// What a controller says about itself; only the fields monolithd uses are kept.
#[derive(Debug, Clone, PartialEq)]
struct Description {
    name: String,
    vendor: String,
    serial: String,
    location: String,
    active_mode: i32,
    modes: Vec<String>,
    zones: Vec<Zone>,
    num_leds: usize,
    colors: Vec<Color>,
}

/// Controller IDs from a controller-count reply: unique IDs from protocol 6, list
/// positions before it.
fn parse_ids(body: &[u8], protocol: u32) -> Result<Vec<u32>, String> {
    let mut cursor = Cursor::new(body);
    let count = cursor.u32()?;
    if protocol < 6 {
        return Ok((0..count).collect());
    }
    (0..count).map(|_| cursor.u32()).collect()
}

/// One mode, skipped except for its name.
fn parse_mode(cursor: &mut Cursor, protocol: u32) -> Result<String, String> {
    let name = cursor.string()?;
    if protocol < 6 {
        cursor.take(4)?; // value
    }
    cursor.take(12)?; // flags, speed_min, speed_max
    if protocol >= 3 {
        cursor.take(8)?; // brightness_min, brightness_max
    }
    cursor.take(12)?; // colors_min, colors_max, speed
    if protocol >= 3 {
        cursor.take(4)?; // brightness
    }
    cursor.take(8)?; // direction, color_mode
    let colors = cursor.u16()? as usize;
    cursor.take(4 * colors)?;
    Ok(name)
}

/// A matrix map: a 16-bit length, then that many bytes (or none).
fn skip_matrix(cursor: &mut Cursor) -> Result<(), String> {
    let length = cursor.u16()? as usize;
    cursor.take(length).map(|_| ())
}

fn parse_segment(cursor: &mut Cursor, protocol: u32) -> Result<(), String> {
    cursor.string()?;
    cursor.take(12)?; // type, start_idx, leds_count
    if protocol >= 6 {
        skip_matrix(cursor)?;
        cursor.take(4)?; // flags
    }
    Ok(())
}

fn parse_zone(cursor: &mut Cursor, protocol: u32) -> Result<Zone, String> {
    let name = cursor.string()?;
    cursor.take(12)?; // type, leds_min, leds_max
    let num_leds = cursor.u32()? as usize;
    skip_matrix(cursor)?;
    if protocol >= 4 {
        for _ in 0..cursor.u16()? {
            parse_segment(cursor, protocol)?;
        }
    }
    if protocol >= 5 {
        cursor.take(4)?; // flags
    }
    if protocol >= 6 {
        cursor.take(4)?; // active_mode
        for _ in 0..cursor.u16()? {
            parse_mode(cursor, protocol)?;
        }
        cursor.string()?; // display name
    }
    Ok(Zone { name, num_leds })
}

/// A controller-data reply, read in the protocol version the request named. Every byte
/// must be accounted for: a miscounted field shifts everything after it.
fn parse_description(body: &[u8], protocol: u32) -> Result<Description, String> {
    let mut cursor = Cursor::new(body);
    let size = cursor.u32()? as usize;
    if size != body.len() {
        return Err(format!("the description says {size} bytes but the packet has {}", body.len()));
    }
    cursor.take(4)?; // type
    let name = cursor.string()?;
    let vendor = if protocol >= 1 { cursor.string()? } else { String::new() };
    cursor.string()?; // description
    cursor.string()?; // version
    let serial = cursor.string()?;
    let location = cursor.string()?;
    let mode_count = cursor.u16()?;
    let active_mode = cursor.i32()?;
    let modes = (0..mode_count).map(|_| parse_mode(&mut cursor, protocol)).collect::<Result<Vec<_>, _>>()?;
    let zone_count = cursor.u16()?;
    let zones = (0..zone_count).map(|_| parse_zone(&mut cursor, protocol)).collect::<Result<Vec<_>, _>>()?;
    let num_leds = cursor.u16()? as usize;
    for _ in 0..num_leds {
        cursor.string()?;
        if protocol < 6 {
            cursor.take(4)?; // value
        }
    }
    let color_count = cursor.u16()?;
    let colors = (0..color_count).map(|_| cursor.color()).collect::<Result<Vec<_>, _>>()?;
    if protocol >= 5 {
        for _ in 0..cursor.u16()? {
            cursor.string()?; // LED display names
        }
        cursor.take(4)?; // flags
    }
    if protocol >= 6 {
        cursor.string()?; // display name
        let length = cursor.u32()? as usize;
        cursor.take(length)?; // configuration
    }
    if cursor.remaining() != 0 {
        return Err(format!("{} bytes left over after the description", cursor.remaining()));
    }
    Ok(Description { name, vendor, serial, location, active_mode, modes, zones, num_leds, colors })
}

// ---------------------------------------------------------------- the connection

type Waiting = Option<(u32, u32, oneshot::Sender<Vec<u8>>)>;

#[derive(Default)]
struct Shared {
    /// The one request awaiting its answer: device ID, packet ID, where to deliver it.
    waiting: Mutex<Waiting>,
    /// An error acknowledgement not yet reported.
    refused: Mutex<Option<String>>,
    /// Why the connection ended, once it has.
    closed: Mutex<Option<String>>,
}

#[derive(Clone)]
struct Link {
    writer: Arc<AsyncMutex<OwnedWriteHalf>>,
    shared: Arc<Shared>,
}

impl Link {
    async fn send(&self, device: u32, id: u32, body: &[u8]) -> Result<(), String> {
        if let Some(reason) = self.shared.closed.lock().unwrap().clone() {
            return Err(reason);
        }
        if let Some(refusal) = self.shared.refused.lock().unwrap().take() {
            return Err(refusal);
        }
        self.writer.lock().await.write_all(&packet(device, id, body)).await.map_err(|error| format!("send to OpenRGB: {error}"))
    }

    async fn request(&self, device: u32, id: u32, body: &[u8]) -> Result<Vec<u8>, String> {
        let (deliver, answer) = oneshot::channel();
        *self.shared.waiting.lock().unwrap() = Some((device, id, deliver));
        self.send(device, id, body).await?;
        match timeout(REPLY_TIMEOUT, answer).await {
            Ok(Ok(body)) => Ok(body),
            Ok(Err(_)) => Err(self.shared.closed.lock().unwrap().clone().unwrap_or_else(|| "OpenRGB closed the connection".to_owned())),
            Err(_) => Err(format!("OpenRGB did not answer packet {id} for controller {device} within {} s", REPLY_TIMEOUT.as_secs())),
        }
    }
}

fn status_name(status: u32) -> &'static str {
    match status {
        1 => "generic error",
        2 => "unsupported",
        3 => "not allowed",
        4 => "invalid device ID",
        5 => "invalid data",
        _ => "unknown status",
    }
}

async fn read_body(reader: &mut OwnedReadHalf) -> Result<(u32, u32, Vec<u8>), String> {
    let mut head = [0u8; 16];
    reader.read_exact(&mut head).await.map_err(|error| format!("OpenRGB connection lost: {error}"))?;
    let (device, id, size) = header(&head)?;
    if size > MAX_BODY {
        return Err(format!("OpenRGB sent a {size}-byte packet"));
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body).await.map_err(|error| format!("OpenRGB connection lost: {error}"))?;
    Ok((device, id, body))
}

/// Read every packet the server sends until the connection ends.
async fn drain(mut reader: OwnedReadHalf, shared: Arc<Shared>) {
    let reason = loop {
        let (device, id, body) = match read_body(&mut reader).await {
            Ok(packet) => packet,
            Err(reason) => break reason,
        };
        if id == ACK {
            let mut cursor = Cursor::new(&body);
            if let (Ok(of), Ok(status)) = (cursor.u32(), cursor.u32()) {
                if status != 0 {
                    *shared.refused.lock().unwrap() = Some(format!("OpenRGB refused packet {of} for controller {device}: {}", status_name(status)));
                }
            }
            continue;
        }
        let mut waiting = shared.waiting.lock().unwrap();
        if matches!(&*waiting, Some((want_device, want_id, _)) if (*want_device, *want_id) == (device, id)) {
            if let Some((_, _, deliver)) = waiting.take() {
                let _ = deliver.send(body);
            }
        }
    };
    *shared.closed.lock().unwrap() = Some(reason);
    shared.waiting.lock().unwrap().take();
}

pub struct Client {
    link: Link,
    protocol: u32,
}

impl Client {
    pub async fn connect(address: &str) -> Result<Self, String> {
        let stream = timeout(REPLY_TIMEOUT, TcpStream::connect(address))
            .await
            .map_err(|_| format!("connect {address}: timed out"))?
            .map_err(|error| format!("connect {address}: {error}"))?;
        // A frame is several small packets; send each at once.
        stream.set_nodelay(true).map_err(|error| format!("connect {address}: {error}"))?;
        let (reader, writer) = stream.into_split();
        let shared = Arc::new(Shared::default());
        tokio::spawn(drain(reader, shared.clone()));
        let link = Link { writer: Arc::new(AsyncMutex::new(writer)), shared };
        let mut name = CLIENT_NAME.as_bytes().to_vec();
        name.push(0);
        link.send(0, SET_CLIENT_NAME, &name).await?;
        let reply = link.request(0, REQUEST_PROTOCOL_VERSION, &PROTOCOL.to_le_bytes()).await.map_err(|error| format!("{error} (OpenRGB before 0.5 is not supported)"))?;
        let server = Cursor::new(&reply).u32()?;
        Ok(Self { link, protocol: server.min(PROTOCOL) })
    }

    /// The protocol version in use: the lower of the server's and this client's.
    pub fn protocol(&self) -> u32 {
        self.protocol
    }

    pub async fn controllers(&self) -> Result<Vec<Controller>, String> {
        let reply = self.link.request(0, REQUEST_CONTROLLER_COUNT, &[]).await?;
        let mut controllers = Vec::new();
        for id in parse_ids(&reply, self.protocol)? {
            let body = self.link.request(id, REQUEST_CONTROLLER_DATA, &self.protocol.to_le_bytes()).await?;
            let description = parse_description(&body, self.protocol).map_err(|error| format!("OpenRGB controller {id}: {error}"))?;
            controllers.push(Controller { id, description, link: self.link.clone() });
        }
        Ok(controllers)
    }
}

/// One controller as the server described it when listed, with a handle for sending it
/// colors. The description is not refreshed.
pub struct Controller {
    id: u32,
    description: Description,
    link: Link,
}

impl Controller {
    pub fn id(&self) -> usize {
        self.id as usize
    }

    #[allow(dead_code)] // for diagnosis
    pub fn name(&self) -> &str {
        &self.description.name
    }

    pub fn vendor(&self) -> &str {
        &self.description.vendor
    }

    pub fn serial(&self) -> &str {
        &self.description.serial
    }

    pub fn location(&self) -> &str {
        &self.description.location
    }

    pub fn num_leds(&self) -> usize {
        self.description.num_leds
    }

    pub fn get_zone(&self, index: usize) -> Result<&Zone, String> {
        self.description.zones.get(index).ok_or_else(|| format!("controller {} ({}) has no zone {index}", self.id, self.description.name))
    }

    /// The mode that was active when the controller was listed.
    #[allow(dead_code)] // for diagnosis
    pub fn active_mode(&self) -> Option<&str> {
        usize::try_from(self.description.active_mode).ok().and_then(|index| self.description.modes.get(index)).map(String::as_str)
    }

    /// The LED colors when the controller was listed.
    #[allow(dead_code)] // for diagnosis
    pub fn colors(&self) -> &[Color] {
        &self.description.colors
    }

    /// Put the controller under software control: OpenRGB's SetCustomMode, which picks
    /// the controller's direct mode where it has one.
    pub async fn set_controllable_mode(&self) -> Result<(), String> {
        self.link.send(self.id, SET_CUSTOM_MODE, &[]).await
    }

    pub async fn set_leds(&self, colors: &[Color]) -> Result<(), String> {
        if colors.len() != self.num_leds() {
            return Err(format!("controller {} has {} LEDs, not {}", self.id, self.num_leds(), colors.len()));
        }
        self.link.send(self.id, UPDATE_LEDS, &colors_body(None, colors)).await
    }

    pub async fn set_all_leds(&self, color: Color) -> Result<(), String> {
        self.set_leds(&vec![color; self.num_leds()]).await
    }

    pub async fn set_zone_leds(&self, zone: usize, colors: &[Color]) -> Result<(), String> {
        let count = self.get_zone(zone)?.num_leds();
        if colors.len() != count {
            return Err(format!("zone {zone} of controller {} has {count} LEDs, not {}", self.id, colors.len()));
        }
        self.link.send(self.id, UPDATE_ZONE_LEDS, &colors_body(Some(zone as u32), colors)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// The captured OpenRGB 1.0 session: (device ID, packet ID, body).
    fn fixture() -> Vec<(u32, u32, Vec<u8>)> {
        let text = include_str!("../tests/fixtures/openrgb-1.0-controllers.hex");
        text.lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .map(|line| {
                let mut words = line.split_whitespace();
                let device = words.next().unwrap().parse().unwrap();
                let id = words.next().unwrap().parse().unwrap();
                let hex = words.next().unwrap();
                let body = (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap()).collect();
                (device, id, body)
            })
            .collect()
    }

    fn captured(device: u32, id: u32) -> Vec<u8> {
        fixture().into_iter().find(|(d, i, _)| (*d, *i) == (device, id)).map(|(_, _, body)| body).unwrap()
    }

    #[test]
    fn reads_every_captured_controller_to_the_last_byte() {
        let ids = parse_ids(&captured(0, REQUEST_CONTROLLER_COUNT), 6).unwrap();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
        for id in 0..4 {
            let ram = parse_description(&captured(id, REQUEST_CONTROLLER_DATA), 6).unwrap();
            assert_eq!((ram.vendor.as_str(), ram.num_leds, ram.colors.len()), ("ENE", 8, 8));
            assert!(ram.location.starts_with("I2C: SMBus PIIX4 adapter port 0"), "{}", ram.location);
        }
        let board = parse_description(&captured(4, REQUEST_CONTROLLER_DATA), 6).unwrap();
        assert_eq!((board.vendor.as_str(), board.serial.as_str()), ("ASUS", "9876543210"));
        assert_eq!(board.zones.iter().map(|zone| zone.num_leds).take(2).collect::<Vec<_>>(), vec![5, 70], "the ROG eye, then the ARGB header");
        assert_eq!(board.colors.len(), board.num_leds);
        assert_eq!(board.colors[0], Color::new(0x43, 0x00, 0xff), "the deep violet on screen at capture time");
    }

    #[test]
    fn a_truncated_description_is_an_error_not_a_panic() {
        let body = captured(4, REQUEST_CONTROLLER_DATA);
        for length in 0..body.len() {
            assert!(parse_description(&body[..length], 6).is_err(), "prefix of {length} bytes");
        }
    }

    #[test]
    fn controller_ids_are_positions_before_protocol_6() {
        let body = [3u32, 7, 9, 11].iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<_>>();
        assert_eq!(parse_ids(&body[..4], 5).unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_ids(&body, 6).unwrap(), vec![7, 9, 11]);
    }

    #[test]
    fn packets_follow_the_documented_layout() {
        let bytes = packet(4, UPDATE_ZONE_LEDS, &colors_body(Some(1), &[Color::new(1, 2, 3), Color::new(4, 5, 6)]));
        let expected: Vec<u8> = [&b"ORGB"[..], &4u32.to_le_bytes(), &1051u32.to_le_bytes(), &18u32.to_le_bytes(), &18u32.to_le_bytes(), &1u32.to_le_bytes(), &2u16.to_le_bytes(), &[1, 2, 3, 0, 4, 5, 6, 0]].concat();
        assert_eq!(bytes, expected);
        assert_eq!(header(&bytes[..16].try_into().unwrap()).unwrap(), (4, 1051, 18));
        assert!(header(b"NOPE\0\0\0\0\0\0\0\0\0\0\0\0").is_err());
        assert_eq!(colors_body(None, &[Color::new(9, 8, 7)]), [&10u32.to_le_bytes()[..], &1u16.to_le_bytes(), &[9, 8, 7, 0]].concat());
    }

    /// A fake OpenRGB 1.0 replaying the capture, chattering the way the real one does
    /// (a name, an ACK after every request, update packets), then checking what arrives.
    #[tokio::test]
    async fn lists_the_captured_controllers_and_sends_zone_colors_through_the_chatter() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();
            let fixture = fixture();
            let noise = fixture.iter().find(|(_, id, _)| *id == 1150).cloned().unwrap();
            let mut received = Vec::new();
            loop {
                let Ok((device, id, body)) = read_body(&mut reader).await else { break };
                let mut reply = Vec::new();
                if let Some((_, _, answer)) = fixture.iter().find(|(d, i, _)| *i == id && (*d == device || id != REQUEST_CONTROLLER_DATA) && matches!(id, 0 | 1 | 40)) {
                    reply.extend(packet(device, id, answer));
                }
                if id == REQUEST_PROTOCOL_VERSION {
                    reply.extend(packet(0, 51, b"OpenRGB 1.0\0"));
                }
                let status: u32 = if id == UPDATE_LEDS { 5 } else { 0 };
                reply.extend(packet(device, ACK, &[id.to_le_bytes(), status.to_le_bytes()].concat()));
                reply.extend(packet(noise.0, noise.1, &noise.2));
                writer.write_all(&reply).await.unwrap();
                received.push((device, id, body));
            }
            received
        });

        let client = Client::connect(&address).await.unwrap();
        assert_eq!(client.protocol(), 6);
        let controllers = client.controllers().await.unwrap();
        assert_eq!(controllers.iter().map(Controller::id).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
        let board = &controllers[4];
        assert_eq!(board.get_zone(1).unwrap().num_leds(), 70);
        assert!(board.get_zone(9).is_err());
        assert!(board.set_zone_leds(0, &[Color::new(1, 2, 3)]).await.is_err(), "the eye has five LEDs");
        board.set_controllable_mode().await.unwrap();
        board.set_zone_leds(0, &[Color::new(10, 20, 30); 5]).await.unwrap();
        // The fake refuses UpdateLEDs; the refusal is reported by the next write.
        controllers[0].set_all_leds(Color::new(0, 0, 0)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let refused = controllers[0].set_zone_leds(0, &vec![Color::default(); controllers[0].get_zone(0).unwrap().num_leds()]).await.unwrap_err();
        assert!(refused.contains("invalid data"), "{refused}");
        drop(controllers);
        drop(client);

        let received = server.await.unwrap();
        let ids: Vec<u32> = received.iter().map(|(_, id, _)| *id).collect();
        assert_eq!(ids, vec![SET_CLIENT_NAME, REQUEST_PROTOCOL_VERSION, REQUEST_CONTROLLER_COUNT, 1, 1, 1, 1, 1, SET_CUSTOM_MODE, UPDATE_ZONE_LEDS, UPDATE_LEDS]);
        assert_eq!(received[0].2, b"monolithd\0");
        assert_eq!(received[9], (4, UPDATE_ZONE_LEDS, colors_body(Some(0), &[Color::new(10, 20, 30); 5])));
    }

    /// Live, read-only: both clients must describe the running server identically.
    /// Temporary, until openrgb2 is removed. Run inside the lighting stack's network
    /// namespace (`nsenter -t <stack pid> -U -n --preserve-credentials`).
    #[tokio::test]
    #[ignore]
    async fn live_matches_the_openrgb2_client() {
        let ours = Client::connect("127.0.0.1:6742").await.unwrap().controllers().await.unwrap();
        let theirs: Vec<openrgb2::Controller> = openrgb2::OpenRgbClient::connect_to("127.0.0.1:6742", 6).await.unwrap().get_all_controllers().await.unwrap().into_iter().collect();
        assert_eq!(ours.len(), theirs.len());
        for (a, b) in ours.iter().zip(&theirs) {
            assert_eq!((a.id(), a.vendor(), a.location(), a.serial(), a.num_leds()), (b.id(), b.vendor(), b.location(), b.serial(), b.num_leds()));
            let zones = |index| b.get_zone(index).map(|zone| zone.num_leds()).ok();
            let count = a.description.zones.len();
            assert_eq!(a.description.zones.iter().map(|zone| Some(zone.num_leds())).collect::<Vec<_>>(), (0..count).map(zones).collect::<Vec<_>>());
            assert_eq!(zones(count), None, "openrgb2 sees no extra zone");
            println!("controller {} {:?}: {} LEDs, zones {:?}, mode {:?}", a.id(), a.name(), a.num_leds(), a.description.zones, a.active_mode());
        }
    }

    /// Live: the server accepts our LED packets. Writes back the colors already showing,
    /// so nothing visible changes. Same namespace as above.
    #[tokio::test]
    #[ignore]
    async fn live_server_accepts_our_color_packets() {
        let client = Client::connect("127.0.0.1:6742").await.unwrap();
        let controllers = client.controllers().await.unwrap();
        for controller in &controllers {
            controller.set_leds(controller.colors()).await.unwrap();
            let mut offset = 0;
            for (index, zone) in controller.description.zones.iter().enumerate() {
                controller.set_zone_leds(index, &controller.colors()[offset..offset + zone.num_leds()]).await.unwrap();
                offset += zone.num_leds();
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Any refusal is reported before this request is sent.
        client.link.request(0, REQUEST_PROTOCOL_VERSION, &PROTOCOL.to_le_bytes()).await.unwrap();
        println!("accepted by OpenRGB for {} controllers", controllers.len());
    }
}
