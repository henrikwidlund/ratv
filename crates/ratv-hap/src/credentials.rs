use core::fmt;
use core::str::FromStr;

use zeroize::Zeroizing;

/// Long-term keys from a completed pair-setup.
///
/// The text form is `ltpk:ltsk:accessory_id:client_id`, each part hex encoded.
/// This is the same format pyatv uses, so credentials can be shared.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// Device long-term Ed25519 public key.
    pub accessory_ltpk: [u8; 32],
    /// Our long-term Ed25519 secret key (seed).
    pub client_ltsk: Zeroizing<[u8; 32]>,
    /// Device pairing identifier.
    pub accessory_id: Vec<u8>,
    /// Our pairing identifier.
    pub client_id: Vec<u8>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("accessory_id", &String::from_utf8_lossy(&self.accessory_id))
            .field("client_id", &String::from_utf8_lossy(&self.client_id))
            .finish_non_exhaustive()
    }
}

fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for b in bytes {
        write!(f, "{b:02x}")?;
    }
    Ok(())
}

impl fmt::Display for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.accessory_ltpk)?;
        f.write_str(":")?;
        write_hex(f, self.client_ltsk.as_ref())?;
        f.write_str(":")?;
        write_hex(f, &self.accessory_id)?;
        f.write_str(":")?;
        write_hex(f, &self.client_id)
    }
}

/// Error when parsing [`Credentials`] from text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid credentials string")]
pub struct ParseCredentialsError;

fn parse_hex(s: &str) -> Result<Vec<u8>, ParseCredentialsError> {
    if !s.len().is_multiple_of(2) {
        return Err(ParseCredentialsError);
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char)
                .to_digit(16)
                .ok_or(ParseCredentialsError)?;
            let lo = (pair[1] as char)
                .to_digit(16)
                .ok_or(ParseCredentialsError)?;
            Ok((hi * 16 + lo) as u8)
        })
        .collect()
}

impl FromStr for Credentials {
    type Err = ParseCredentialsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.trim().split(':');
        let mut next = || {
            parts
                .next()
                .ok_or(ParseCredentialsError)
                .and_then(parse_hex)
        };
        let ltpk = next()?;
        let secret = Zeroizing::new(next()?);
        let accessory_id = next()?;
        let client_id = next()?;
        if parts.next().is_some() {
            return Err(ParseCredentialsError);
        }
        Ok(Self {
            accessory_ltpk: ltpk.try_into().map_err(|_| ParseCredentialsError)?,
            client_ltsk: Zeroizing::new(
                secret
                    .as_slice()
                    .try_into()
                    .map_err(|_| ParseCredentialsError)?,
            ),
            accessory_id,
            client_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Credentials {
        Credentials {
            accessory_ltpk: [0xAB; 32],
            client_ltsk: Zeroizing::new([0x01; 32]),
            accessory_id: b"AA:BB".to_vec(),
            client_id: b"client".to_vec(),
        }
    }

    #[test]
    fn text_roundtrip() {
        let c = sample();
        let s = c.to_string();
        assert!(s.starts_with(&"ab".repeat(32)));
        assert!(s.ends_with(":636c69656e74"));
        assert_eq!(s.parse::<Credentials>().unwrap(), c);
    }

    #[test]
    fn debug_hides_secret() {
        let s = format!("{:?}", sample());
        assert!(s.contains("AA:BB"));
        assert!(!s.contains("ltsk"));
    }

    #[test]
    fn parse_errors() {
        for bad in [
            "",
            "ab:cd",
            "zz:00:00:00",
            "abc:00:00:00",
            &format!("{}:{}:00:00", "ab".repeat(31), "01".repeat(32)),
            &format!("{}:{}:00:00", "ab".repeat(32), "01".repeat(31)),
            &format!("{}:{}:00:00:00", "ab".repeat(32), "01".repeat(32)),
        ] {
            assert_eq!(
                bad.parse::<Credentials>(),
                Err(ParseCredentialsError),
                "{bad}"
            );
        }
    }
}
