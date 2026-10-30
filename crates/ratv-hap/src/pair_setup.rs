//! Pair-setup: create long-term [`Credentials`] using the PIN shown on the device.
//!
//! Each step consumes the previous state, so steps cannot run out of order:
//!
//! ```text
//! start() -> M1
//!   AwaitingM2::handle_m2(M2) -> NeedPin
//!   NeedPin::submit_pin(pin) -> M3 + AwaitingM4
//!   AwaitingM4::handle_m4(M4) -> Verified
//!     Verified::into_transient()          (transient pairing ends here)
//!     Verified::exchange(identity) -> M5 + AwaitingM6
//!   AwaitingM6::handle_m6(M6) -> Credentials
//! ```
//!
//! Messages are raw TLV8. The protocol layer adds its own framing.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use ratv_codec::tlv8::{Tlv8, Writer, tag};
use zeroize::Zeroizing;

use crate::cipher::{open_message, seal_message};
use crate::kdf::{hkdf_sha512, label};
use crate::srp::SrpClient;
use crate::{
    Credentials, CryptoRng, Error, FLAG_TRANSIENT, SharedSecret, method, random_32, required,
    required_32,
};

/// Starts pair-setup. Returns M1 and the next state.
///
/// Transient pairing (AirPlay) creates session keys without storing credentials.
#[must_use]
pub fn start(transient: bool) -> (Vec<u8>, AwaitingM2) {
    let mut w = Writer::with_capacity(9)
        .byte(tag::METHOD, method::PAIR_SETUP)
        .byte(tag::STATE, 1);
    if transient {
        w = w.byte(tag::FLAGS, FLAG_TRANSIENT);
    }
    (w.finish(), AwaitingM2 { transient })
}

/// Waiting for M2 (salt and device SRP public key).
#[derive(Debug)]
pub struct AwaitingM2 {
    transient: bool,
}

impl AwaitingM2 {
    /// Handles M2.
    pub fn handle_m2(self, msg: &[u8]) -> Result<NeedPin, Error> {
        let tlv = crate::expect_state(msg, 2)?;
        Ok(NeedPin {
            salt: required(&tlv, tag::SALT)?.to_vec(),
            b_pub: required(&tlv, tag::PUBLIC_KEY)?.to_vec(),
            transient: self.transient,
        })
    }
}

/// Device is showing a PIN. Call [`NeedPin::submit_pin`] with it.
#[derive(Debug)]
pub struct NeedPin {
    salt: Vec<u8>,
    b_pub: Vec<u8>,
    transient: bool,
}

impl NeedPin {
    /// Computes M3 from the PIN.
    pub fn submit_pin(
        self,
        pin: &str,
        rng: &mut impl CryptoRng,
    ) -> Result<(Vec<u8>, AwaitingM4), Error> {
        let a_private = Zeroizing::new(random_32(rng));
        let srp = SrpClient::new(pin.as_bytes(), &self.salt, &self.b_pub, &a_private)?;
        let msg = Writer::with_capacity(470)
            .byte(tag::STATE, 3)
            .item(tag::PUBLIC_KEY, srp.public_key())
            .item(tag::PROOF, srp.proof())
            .finish();
        Ok((
            msg,
            AwaitingM4 {
                srp,
                transient: self.transient,
            },
        ))
    }
}

/// Waiting for M4 (device SRP proof).
pub struct AwaitingM4 {
    srp: SrpClient,
    transient: bool,
}

impl core::fmt::Debug for AwaitingM4 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AwaitingM4").finish_non_exhaustive()
    }
}

impl AwaitingM4 {
    /// Checks the device proof.
    pub fn handle_m4(self, msg: &[u8]) -> Result<Verified, Error> {
        let tlv = crate::expect_state(msg, 4)?;
        let key = *self.srp.verify(required(&tlv, tag::PROOF)?)?;
        Ok(Verified {
            key: Zeroizing::new(key),
            transient: self.transient,
        })
    }
}

/// SRP succeeded; both sides share the session key `K`.
pub struct Verified {
    key: Zeroizing<[u8; 64]>,
    transient: bool,
}

impl core::fmt::Debug for Verified {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Verified")
            .field("transient", &self.transient)
            .finish_non_exhaustive()
    }
}

/// Our identity sent to the device in M5.
#[derive(Debug, Clone, Copy)]
pub struct ClientIdentity<'a> {
    /// Pairing identifier, usually a UUID string.
    pub id: &'a [u8],
    /// Extra data in the `NAME` item. Companion sends OPACK device info here.
    pub metadata: Option<&'a [u8]>,
}

impl Verified {
    /// Returns true if pairing was started as transient.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        self.transient
    }

    /// Ends a transient pairing and returns the SRP session key.
    #[must_use]
    pub fn into_transient(self) -> SharedSecret {
        SharedSecret::new(self.key.as_ref())
    }

    /// Creates M5 with a new long-term key for us.
    #[must_use]
    pub fn exchange(
        self,
        identity: ClientIdentity<'_>,
        rng: &mut impl CryptoRng,
    ) -> (Vec<u8>, AwaitingM6) {
        let seed = Zeroizing::new(random_32(rng));
        let signing = SigningKey::from_bytes(&seed);
        let ltpk = signing.verifying_key().to_bytes();
        let enc_key: [u8; 32] = hkdf_sha512(
            label::PAIR_SETUP_ENCRYPT_SALT,
            label::PAIR_SETUP_ENCRYPT_INFO,
            self.key.as_ref(),
        );
        let x: [u8; 32] = hkdf_sha512(
            label::PAIR_SETUP_CONTROLLER_SIGN_SALT,
            label::PAIR_SETUP_CONTROLLER_SIGN_INFO,
            self.key.as_ref(),
        );
        let mut info = Vec::with_capacity(64 + identity.id.len());
        info.extend_from_slice(&x);
        info.extend_from_slice(identity.id);
        info.extend_from_slice(&ltpk);
        let signature = signing.sign(&info).to_bytes();

        let mut w = Writer::with_capacity(160 + identity.id.len())
            .item(tag::IDENTIFIER, identity.id)
            .item(tag::PUBLIC_KEY, &ltpk)
            .item(tag::SIGNATURE, &signature);
        if let Some(meta) = identity.metadata {
            w = w.item(tag::NAME, meta);
        }
        let mut sub = w.finish();
        seal_message(&enc_key, label::PS_MSG05, &mut sub);
        let msg = Writer::with_capacity(sub.len() + 8)
            .byte(tag::STATE, 5)
            .item(tag::ENCRYPTED_DATA, &sub)
            .finish();
        let next = AwaitingM6 {
            enc_key: Zeroizing::new(enc_key),
            srp_key: self.key,
            seed,
            client_id: identity.id.to_vec(),
        };
        (msg, next)
    }
}

/// Waiting for M6 (device long-term key).
pub struct AwaitingM6 {
    enc_key: Zeroizing<[u8; 32]>,
    srp_key: Zeroizing<[u8; 64]>,
    seed: Zeroizing<[u8; 32]>,
    client_id: Vec<u8>,
}

impl core::fmt::Debug for AwaitingM6 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AwaitingM6").finish_non_exhaustive()
    }
}

impl AwaitingM6 {
    /// Decrypts M6, checks the device signature and returns the credentials.
    pub fn handle_m6(self, msg: &[u8]) -> Result<Credentials, Error> {
        let tlv = crate::expect_state(msg, 6)?;
        let plain = open_message(
            &self.enc_key,
            label::PS_MSG06,
            required(&tlv, tag::ENCRYPTED_DATA)?,
        )?;
        let sub = Tlv8::decode(&plain)?;
        let accessory_id = required(&sub, tag::IDENTIFIER)?;
        let ltpk = required_32(&sub, tag::PUBLIC_KEY)?;
        let signature = Signature::from_slice(required(&sub, tag::SIGNATURE)?)
            .map_err(|_| Error::BadSignature)?;

        let x: [u8; 32] = hkdf_sha512(
            label::PAIR_SETUP_ACCESSORY_SIGN_SALT,
            label::PAIR_SETUP_ACCESSORY_SIGN_INFO,
            self.srp_key.as_ref(),
        );
        let mut info = Vec::with_capacity(64 + accessory_id.len());
        info.extend_from_slice(&x);
        info.extend_from_slice(accessory_id);
        info.extend_from_slice(&ltpk);
        VerifyingKey::from_bytes(&ltpk)
            .map_err(|_| Error::InvalidPublicKey)?
            .verify(&info, &signature)
            .map_err(|_| Error::BadSignature)?;

        Ok(Credentials {
            accessory_ltpk: ltpk,
            client_ltsk: self.seed,
            accessory_id: accessory_id.to_vec(),
            client_id: self.client_id,
        })
    }
}
