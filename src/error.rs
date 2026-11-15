use std::fmt;
use std::io;
use std::path::Path;

pub type Result<T> = std::result::Result<T, VdashError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VdashError {
    pub code: u8,
    pub message: String,
}

impl VdashError {
    pub fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn io_path(path: impl AsRef<Path>, err: io::Error) -> Self {
        Self::new(5, format!("{}: {err}", path.as_ref().display()))
    }

    pub fn not_implemented(cmd: &str) -> Self {
        Self::new(
            2,
            format!("'{cmd}' is not implemented yet (see docs/ROADMAP.md)"),
        )
    }
}

impl fmt::Display for VdashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for VdashError {}

impl From<io::Error> for VdashError {
    fn from(err: io::Error) -> Self {
        Self::new(5, err.to_string())
    }
}
