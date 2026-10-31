use core::fmt;
use core::str::FromStr;

use secrecy::SecretString;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// Long-term keys from a completed pair-setup.
///
/// The text form is `ltpk:ltsk:accessory_id:client_id`, each part hex encoded.
/// This is the same format pyatv uses, so credentials can be shared.
/// Create it with [`to_secret_string`](Self::to_secret_string) and read it with `parse`.
#[derive(Clone)]
pub struct Credentials {
    /// Device long-term Ed25519 public key.
    pub(crate) accessory_ltpk: [u8; 32],
    /// Our long-term Ed25519 secret key (seed).
    pub(crate) client_ltsk: Zeroizing<[u8; 32]>,
    /// Device pairing identifier.
    pub(crate) accessory_id: Vec<u8>,
    /// Our pairing identifier.
    pub(crate) client_id: Vec<u8>,
}

impl PartialEq for Credentials {
    fn eq(&self, other: &Self) -> bool {
        // The secret key is always compared in full, in constant time.
        let secret = bool::from(self.client_ltsk.ct_eq(other.client_ltsk.as_ref()));
        secret
            && self.accessory_ltpk == other.accessory_ltpk
            && self.accessory_id == other.accessory_id
            && self.client_id == other.client_id
    }
}

impl Eq for Credentials {}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("accessory_id", &String::from_utf8_lossy(&self.accessory_id))
            .field("client_id", &String::from_utf8_lossy(&self.client_id))
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Creates credentials from their parts, for example from another library.
    /// Usually they come from pairing or from [`to_secret_string`](Self::to_secret_string) text.
    #[must_use]
    pub const fn new(
        accessory_ltpk: [u8; 32],
        client_ltsk: Zeroizing<[u8; 32]>,
        accessory_id: Vec<u8>,
        client_id: Vec<u8>,
    ) -> Self {
        Self {
            accessory_ltpk,
            client_ltsk,
            accessory_id,
            client_id,
        }
    }

    /// Device long-term Ed25519 public key.
    #[must_use]
    pub const fn accessory_ltpk(&self) -> &[u8; 32] {
        &self.accessory_ltpk
    }

    /// Device pairing identifier.
    #[must_use]
    pub fn accessory_id(&self) -> &[u8] {
        &self.accessory_id
    }

    /// Our pairing identifier.
    #[must_use]
    pub fn client_id(&self) -> &[u8] {
        &self.client_id
    }

    /// Text form with the secret key. Store it so that only the owner can read it.
    ///
    /// Get the text with `expose_secret`. It is cleared from memory when dropped.
    #[must_use]
    pub fn to_secret_string(&self) -> SecretString {
        let parts: [&[u8]; 4] = [
            &self.accessory_ltpk,
            self.client_ltsk.as_ref(),
            &self.accessory_id,
            &self.client_id,
        ];
        let len = parts.iter().map(|p| 2 * p.len()).sum::<usize>() + parts.len() - 1;
        // Exact length, so the buffer is never moved and no copy of the key is left in freed memory.
        let mut buf = Zeroizing::new(vec![b':'; len]);
        let mut pos = 0;
        for part in parts {
            let end = pos + 2 * part.len();
            base16ct::lower::encode(part, &mut buf[pos..end]).expect("buffer has the exact length");
            pos = end + 1;
        }
        let text = String::from_utf8(core::mem::take(&mut *buf)).expect("hex is ASCII");
        SecretString::from(text)
    }
}

/// Error when parsing [`Credentials`] from text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid credentials string")]
pub struct ParseCredentialsError;

/// Decodes hex that must fill `out`.
fn decode_exact(s: &str, out: &mut [u8]) -> Result<(), ParseCredentialsError> {
    let len = out.len();
    match base16ct::mixed::decode(s, out) {
        Ok(decoded) if decoded.len() == len => Ok(()),
        _ => Err(ParseCredentialsError),
    }
}

impl FromStr for Credentials {
    type Err = ParseCredentialsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.trim().split(':');
        let mut next = || parts.next().ok_or(ParseCredentialsError);
        let mut accessory_ltpk = [0; 32];
        decode_exact(next()?, &mut accessory_ltpk)?;
        let mut client_ltsk = Zeroizing::new([0; 32]);
        decode_exact(next()?, client_ltsk.as_mut())?;
        let accessory_id =
            base16ct::mixed::decode_vec(next()?).map_err(|_| ParseCredentialsError)?;
        let client_id = base16ct::mixed::decode_vec(next()?).map_err(|_| ParseCredentialsError)?;
        if parts.next().is_some() {
            return Err(ParseCredentialsError);
        }
        Ok(Self {
            accessory_ltpk,
            client_ltsk,
            accessory_id,
            client_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

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
        let secret = c.to_secret_string();
        let s = secret.expose_secret();
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

    proptest::proptest! {
        #[test]
        fn text_roundtrip_any(
            ltpk: [u8; 32],
            ltsk: [u8; 32],
            accessory_id: Vec<u8>,
            client_id: Vec<u8>,
            upper: bool,
        ) {
            let c = Credentials::new(ltpk, ltsk.into(), accessory_id, client_id);
            let secret = c.to_secret_string();
            let mut text = secret.expose_secret().to_owned();
            if upper {
                text.make_ascii_uppercase();
            }
            proptest::prop_assert_eq!(text.parse::<Credentials>().unwrap(), c);
        }

        #[test]
        fn parse_any_text(text: String) {
            // Must not panic. Valid text must give the same text back.
            if let Ok(c) = text.parse::<Credentials>() {
                let again = c.to_secret_string();
                proptest::prop_assert_eq!(again.expose_secret(), text.trim().to_ascii_lowercase());
            }
        }
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
