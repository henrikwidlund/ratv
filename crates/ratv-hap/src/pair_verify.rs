//! Pair-verify: agree on a per-connection secret using stored [`Credentials`].
//!
//! ```text
//! start(creds) -> M1
//!   AwaitingM2::handle_m2(M2) -> M3 + AwaitingM4
//!   AwaitingM4::handle_m4(M4) -> SharedSecret
//! ```

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use ratv_codec::tlv8::{Tlv8, Writer, tag};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::cipher::{open_message, seal_message};
use crate::kdf::{hkdf_sha512, label};
use crate::{Credentials, CryptoRng, Error, SharedSecret, random_32, required, required_32};

/// Starts pair-verify. Returns M1 and the next state.
#[must_use]
pub fn start<'c>(
    credentials: &'c Credentials,
    rng: &mut impl CryptoRng,
) -> (Vec<u8>, AwaitingM2<'c>) {
    let secret = StaticSecret::from(random_32(rng));
    let public = PublicKey::from(&secret);
    let msg = Writer::with_capacity(37)
        .byte(tag::STATE, 1)
        .item(tag::PUBLIC_KEY, public.as_bytes())
        .finish();
    (
        msg,
        AwaitingM2 {
            credentials,
            secret,
            public,
        },
    )
}

/// Waiting for M2 (device ephemeral key and signed proof).
pub struct AwaitingM2<'c> {
    credentials: &'c Credentials,
    secret: StaticSecret,
    public: PublicKey,
}

impl core::fmt::Debug for AwaitingM2<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AwaitingM2").finish_non_exhaustive()
    }
}

impl AwaitingM2<'_> {
    /// Checks the device proof and creates M3.
    pub fn handle_m2(self, msg: &[u8]) -> Result<(Vec<u8>, AwaitingM4), Error> {
        let tlv = crate::expect_state(msg, 2)?;
        let device_public = required_32(&tlv, tag::PUBLIC_KEY)?;
        let shared = self.secret.diffie_hellman(&PublicKey::from(device_public));
        if !shared.was_contributory() {
            return Err(Error::InvalidPublicKey);
        }
        let shared = SharedSecret::new(shared.as_bytes());
        let key: [u8; 32] = hkdf_sha512(
            label::PAIR_VERIFY_ENCRYPT_SALT,
            label::PAIR_VERIFY_ENCRYPT_INFO,
            shared.as_bytes(),
        );

        let plain = open_message(&key, label::PV_MSG02, required(&tlv, tag::ENCRYPTED_DATA)?)?;
        let sub = Tlv8::decode(&plain)?;
        let device_id = required(&sub, tag::IDENTIFIER)?;
        if device_id != self.credentials.accessory_id {
            return Err(Error::UnknownAccessory);
        }
        let signature = Signature::from_slice(required(&sub, tag::SIGNATURE)?)
            .map_err(|_| Error::BadSignature)?;
        let mut info = Vec::with_capacity(64 + device_id.len());
        info.extend_from_slice(&device_public);
        info.extend_from_slice(device_id);
        info.extend_from_slice(self.public.as_bytes());
        VerifyingKey::from_bytes(&self.credentials.accessory_ltpk)
            .map_err(|_| Error::InvalidPublicKey)?
            .verify(&info, &signature)
            .map_err(|_| Error::BadSignature)?;

        let client_id = &self.credentials.client_id;
        info.clear();
        info.extend_from_slice(self.public.as_bytes());
        info.extend_from_slice(client_id);
        info.extend_from_slice(&device_public);
        let signing = SigningKey::from_bytes(&self.credentials.client_ltsk);
        let signature = signing.sign(&info).to_bytes();

        let mut sub = Writer::with_capacity(70 + client_id.len())
            .item(tag::IDENTIFIER, client_id)
            .item(tag::SIGNATURE, &signature)
            .finish();
        seal_message(&key, label::PV_MSG03, &mut sub);
        let msg = Writer::with_capacity(sub.len() + 8)
            .byte(tag::STATE, 3)
            .item(tag::ENCRYPTED_DATA, &sub)
            .finish();
        Ok((msg, AwaitingM4 { shared }))
    }
}

/// Waiting for M4 (confirmation).
#[derive(Debug)]
pub struct AwaitingM4 {
    shared: SharedSecret,
}

impl AwaitingM4 {
    /// Checks M4 and returns the shared secret.
    pub fn handle_m4(self, msg: &[u8]) -> Result<SharedSecret, Error> {
        crate::expect_state(msg, 4)?;
        Ok(self.shared)
    }
}
