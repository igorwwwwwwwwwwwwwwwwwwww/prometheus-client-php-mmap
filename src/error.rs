use thiserror::Error;

#[derive(Debug, Error)]
pub enum MmapError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid utf-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("offset {offset} out of bounds for len {len}")]
    OutOfBounds { offset: usize, len: usize },

    #[error("entry key length exceeds i32::MAX")]
    KeyTooLong,

    #[error("invalid used header {used} for len {len}")]
    InvalidUsed { used: usize, len: usize },

    #[error("entry length {needed} exceeds provided slice {available}")]
    EntryTooLarge { needed: usize, available: usize },

    #[error("invalid c string: interior NUL")]
    InvalidCString,

    #[error("null pointer")]
    NullPointer,

    #[error("numeric overflow")]
    Overflow,

    #[error("{0}")]
    Message(String),
}

pub type Result<T> = std::result::Result<T, MmapError>;

pub fn checked_add(a: usize, b: usize) -> Result<usize> {
    a.checked_add(b).ok_or(MmapError::Overflow)
}
