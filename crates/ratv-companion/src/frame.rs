use bytes::Bytes;

/// Frame header length: 1 byte type + 3 bytes big-endian payload length.
pub const HEADER_LEN: usize = 4;

/// Companion frame type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameType(pub u8);

#[allow(missing_docs)]
impl FrameType {
    pub const NO_OP: Self = Self(1);
    pub const PAIR_SETUP_START: Self = Self(3);
    pub const PAIR_SETUP_NEXT: Self = Self(4);
    pub const PAIR_VERIFY_START: Self = Self(5);
    pub const PAIR_VERIFY_NEXT: Self = Self(6);
    /// Unencrypted OPACK.
    pub const U_OPACK: Self = Self(7);
    /// Encrypted OPACK. Used for all messages after pair-verify.
    pub const E_OPACK: Self = Self(8);
    /// Packed OPACK.
    pub const P_OPACK: Self = Self(9);
    pub const PAIR_AUTH_REQUEST: Self = Self(10);
    pub const PAIR_AUTH_RESPONSE: Self = Self(11);
    pub const SESSION_START_REQUEST: Self = Self(16);
    pub const SESSION_START_RESPONSE: Self = Self(17);
    pub const SESSION_DATA: Self = Self(18);
    pub const FAMILY_IDENTITY_REQUEST: Self = Self(32);
    pub const FAMILY_IDENTITY_RESPONSE: Self = Self(33);
    pub const FAMILY_IDENTITY_UPDATE: Self = Self(34);
}

/// A received frame with its payload already decrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Frame type.
    pub ty: FrameType,
    /// Payload.
    pub payload: Bytes,
}

pub(crate) const fn header(ty: FrameType, len: usize) -> [u8; HEADER_LEN] {
    let l = (len as u32).to_be_bytes();
    [ty.0, l[1], l[2], l[3]]
}
