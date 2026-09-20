pub const PROTOCOL_VERSION: u32 = 6;
pub const MAGIC: [u8; 4] = *b"ORGB";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControllerId(pub u32);

#[derive(Debug, Clone)]
pub struct ControllerIdentity {
    pub id: ControllerId,
    pub name: String,
    pub vendor: String,
    pub serial: String,
    pub location: String,
}
