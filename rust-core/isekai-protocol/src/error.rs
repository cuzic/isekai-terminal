#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("frame has length {got}, expected exactly {expected}")]
    FrameLengthMismatch { got: usize, expected: usize },

    #[error("frame declares a body length of {declared} bytes, exceeding the {max} byte limit")]
    FrameTooLarge { declared: usize, max: usize },

    #[error("frame declares a body length of {declared} bytes but only {available} are available")]
    FrameIncomplete { declared: usize, available: usize },

    #[error("unknown frame type byte {0:#04x}")]
    UnknownFrameType(u8),

    #[error("frame declares {declared} features, exceeding the limit of {max}")]
    TooManyFeatures { declared: usize, max: usize },

    #[error("arithmetic overflow while advancing a stream offset")]
    OffsetOverflow,

    #[error("handshake JSON is {got} bytes, exceeding the {max} byte limit")]
    HandshakeTooLarge { got: usize, max: usize },

    #[error("invalid handshake JSON: {0}")]
    HandshakeJson(String),

    #[error("invalid handshake field {field}: {reason}")]
    HandshakeField { field: &'static str, reason: String },

    #[error("unsupported version {got} (supported range {min}..={max})")]
    UnsupportedVersion { got: u32, min: u32, max: u32 },
}
