use std::io::{Read, Write};
use std::net::TcpStream;

pub const PROTOCOL_VERSION: u32 = 6;
const MAGIC: &[u8; 4] = b"ORGB";
const REQUEST_CONTROLLER_COUNT: u32 = 0;
const REQUEST_PROTOCOL_VERSION: u32 = 40;
const SET_CLIENT_NAME: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerId(pub u32);

fn send(stream: &mut TcpStream, device: u32, packet: u32, data: &[u8]) -> std::io::Result<()> {
    stream.write_all(MAGIC)?;
    stream.write_all(&device.to_le_bytes())?;
    stream.write_all(&packet.to_le_bytes())?;
    stream.write_all(&(data.len() as u32).to_le_bytes())?;
    stream.write_all(data)
}

fn receive(stream: &mut TcpStream, expected: u32) -> Result<(u32, Vec<u8>), String> {
    loop {
        let mut header = [0_u8; 16];
        stream.read_exact(&mut header).map_err(|e| e.to_string())?;
        if &header[0..4] != MAGIC { return Err("invalid SDK magic".into()); }
        let device = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let packet = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let size = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        let mut data = vec![0; size];
        stream.read_exact(&mut data).map_err(|e| e.to_string())?;
        if packet == expected { return Ok((device, data)); }
    }
}

pub fn discover() -> Result<Vec<ControllerId>, String> {
    let mut stream = TcpStream::connect("127.0.0.1:6742").map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).map_err(|e| e.to_string())?;
    send(&mut stream, 0, REQUEST_PROTOCOL_VERSION, &PROTOCOL_VERSION.to_le_bytes()).map_err(|e| e.to_string())?;
    let (_, version) = receive(&mut stream, REQUEST_PROTOCOL_VERSION)?;
    if version.len() != 4 || u32::from_le_bytes(version.try_into().unwrap()) < PROTOCOL_VERSION { return Err("SDK 6 unavailable".into()); }
    send(&mut stream, 0, SET_CLIENT_NAME, b"monolithd\0").map_err(|e| e.to_string())?;
    send(&mut stream, 0, REQUEST_CONTROLLER_COUNT, &[]).map_err(|e| e.to_string())?;
    let (_, data) = receive(&mut stream, REQUEST_CONTROLLER_COUNT)?;
    if data.len() < 4 { return Err("short controller response".into()); }
    let count = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if data.len() != 4 + count * 4 { return Err("invalid SDK 6 controller IDs".into()); }
    Ok((0..count).map(|i| ControllerId(u32::from_le_bytes(data[4+i*4..8+i*4].try_into().unwrap()))).collect())
}
