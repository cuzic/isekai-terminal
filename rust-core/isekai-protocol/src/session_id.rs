use crate::error::ProtocolError;

pub const SESSION_ID_LEN: usize = 16;

/// Opaque identifier the helper assigns to a resumable session
/// (`HELPER_PROTOCOL.md` §7.2). It is not a secret by itself — resume
/// requests must also carry a `resume_proof` derived from `session_secret`
/// (that frame is out of scope here, see S-4a).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId([u8; SESSION_ID_LEN]);

impl SessionId {
    pub fn from_bytes(bytes: [u8; SESSION_ID_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; SESSION_ID_LEN] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SessionId({})", self.to_hex())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

pub fn decode_session_id(buf: &[u8]) -> Result<SessionId, ProtocolError> {
    let arr: [u8; SESSION_ID_LEN] = buf.try_into().map_err(|_| ProtocolError::FrameLengthMismatch {
        got: buf.len(),
        expected: SESSION_ID_LEN,
    })?;
    Ok(SessionId(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_bytes() {
        let id = SessionId::from_bytes([7u8; SESSION_ID_LEN]);
        assert_eq!(decode_session_id(id.as_bytes()).unwrap(), id);
        assert_eq!(id.to_hex(), "07".repeat(SESSION_ID_LEN));
    }

    #[test]
    fn rejects_wrong_length() {
        let err = decode_session_id(&[0u8; 15]).unwrap_err();
        assert_eq!(err, ProtocolError::FrameLengthMismatch { got: 15, expected: SESSION_ID_LEN });
    }
}
