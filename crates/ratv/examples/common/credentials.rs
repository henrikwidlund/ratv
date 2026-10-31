//! Reads and writes the credentials file.

use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use ratv::{Credentials, ExposeSecret};

/// Reads credentials written by [`save`].
#[allow(dead_code)]
pub(crate) fn load(path: &str) -> Result<Credentials, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text.parse().map_err(|_| "invalid credentials file")?)
}

/// Writes credentials to a new file that only the owner can read.
#[allow(dead_code)]
pub(crate) fn save(path: &str, credentials: &Credentials) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)?
        .write_all(credentials.to_secret_string().expose_secret().as_bytes())
}
