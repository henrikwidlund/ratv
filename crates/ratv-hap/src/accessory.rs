//! Accessory (device) side of pairing. Used to build fake devices for tests.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use ratv_codec::tlv8::{Tlv8, Writer, tag};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::cipher::{open_message, seal_message};
use crate::kdf::{hkdf_sha512, label};
use crate::srp::SrpServer;
use crate::{Error, SharedSecret, required, required_32};
use core::convert::Infallible;
use rand_core::{TryCryptoRng, TryRng};

/// Controller that completed pair-setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedController {
    /// Controller pairing identifier.
    pub id: Vec<u8>,
    /// Controller long-term public key.
    pub ltpk: [u8; 32],
    /// Data from the `NAME` item, if sent.
    pub metadata: Option<Vec<u8>>,
}

fn error_reply(state: u8, code: u8) -> Vec<u8> {
    Writer::new()
        .byte(tag::STATE, state)
        .byte(tag::ERROR, code)
        .finish()
}

/// Accessory side of pair-setup.
pub struct PairSetupServer {
    id: Vec<u8>,
    signing: SigningKey,
    salt: [u8; 16],
    srp: SrpServer,
    key: Option<[u8; 64]>,
    transient: bool,
    controller: Option<PairedController>,
}

impl PairSetupServer {
    /// Creates a server with fixed keys.
    #[must_use]
    pub fn new(
        id: &[u8],
        ltsk: &[u8; 32],
        pin: &str,
        salt: [u8; 16],
        b_private: &[u8; 32],
    ) -> Self {
        Self {
            id: id.to_vec(),
            signing: SigningKey::from_bytes(ltsk),
            salt,
            srp: SrpServer::new(pin.as_bytes(), &salt, b_private),
            key: None,
            transient: false,
            controller: None,
        }
    }

    /// Accessory long-term public key.
    #[must_use]
    pub fn ltpk(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Controller stored after M5.
    #[must_use]
    pub const fn controller(&self) -> Option<&PairedController> {
        self.controller.as_ref()
    }

    /// SRP session key after M4 (used by transient pairing).
    #[must_use]
    pub fn srp_key(&self) -> Option<SharedSecret> {
        self.key.as_ref().map(|k| SharedSecret::new(k))
    }

    /// Handles M1, M3 or M5 and returns the reply.
    pub fn handle(&mut self, msg: &[u8]) -> Result<Vec<u8>, Error> {
        let tlv = Tlv8::decode(msg)?;
        match tlv.get_u8(tag::STATE) {
            Some(1) => {
                self.transient = tlv.get_u8(tag::FLAGS) == Some(crate::FLAG_TRANSIENT);
                Ok(Writer::new()
                    .byte(tag::STATE, 2)
                    .item(tag::SALT, &self.salt)
                    .item(tag::PUBLIC_KEY, self.srp.public_key())
                    .finish())
            }
            Some(3) => {
                let a = required(&tlv, tag::PUBLIC_KEY)?;
                let proof = required(&tlv, tag::PROOF)?;
                match self.srp.verify(a, proof) {
                    Ok((m2, key)) => {
                        self.key = Some(key);
                        Ok(Writer::new()
                            .byte(tag::STATE, 4)
                            .item(tag::PROOF, &m2)
                            .finish())
                    }
                    Err(_) => Ok(error_reply(4, 2)),
                }
            }
            Some(5) => self.handle_m5(&tlv),
            got => Err(Error::UnexpectedState { expected: 1, got }),
        }
    }

    fn handle_m5(&mut self, tlv: &Tlv8<'_>) -> Result<Vec<u8>, Error> {
        let key = self.key.ok_or(Error::UnexpectedState {
            expected: 3,
            got: Some(5),
        })?;
        let enc_key: [u8; 32] = hkdf_sha512(
            label::PAIR_SETUP_ENCRYPT_SALT,
            label::PAIR_SETUP_ENCRYPT_INFO,
            &key,
        );
        let plain = open_message(
            &enc_key,
            label::PS_MSG05,
            required(tlv, tag::ENCRYPTED_DATA)?,
        )?;
        let sub = Tlv8::decode(&plain)?;
        let id = required(&sub, tag::IDENTIFIER)?;
        let ltpk = required_32(&sub, tag::PUBLIC_KEY)?;
        let signature = Signature::from_slice(required(&sub, tag::SIGNATURE)?)
            .map_err(|_| Error::BadSignature)?;
        let x: [u8; 32] = hkdf_sha512(
            label::PAIR_SETUP_CONTROLLER_SIGN_SALT,
            label::PAIR_SETUP_CONTROLLER_SIGN_INFO,
            &key,
        );
        let info = [&x[..], id, &ltpk].concat();
        VerifyingKey::from_bytes(&ltpk)
            .map_err(|_| Error::InvalidPublicKey)?
            .verify_strict(&info, &signature)
            .map_err(|_| Error::BadSignature)?;
        self.controller = Some(PairedController {
            id: id.to_vec(),
            ltpk,
            metadata: sub.get(tag::NAME).map(<[u8]>::to_vec),
        });

        let x: [u8; 32] = hkdf_sha512(
            label::PAIR_SETUP_ACCESSORY_SIGN_SALT,
            label::PAIR_SETUP_ACCESSORY_SIGN_INFO,
            &key,
        );
        let own_ltpk = self.ltpk();
        let info = [&x[..], &self.id, &own_ltpk].concat();
        let signature = self.signing.sign(&info).to_bytes();
        let mut sub = Writer::new()
            .item(tag::IDENTIFIER, &self.id)
            .item(tag::PUBLIC_KEY, &own_ltpk)
            .item(tag::SIGNATURE, &signature)
            .finish();
        seal_message(&enc_key, label::PS_MSG06, &mut sub);
        Ok(Writer::new()
            .byte(tag::STATE, 6)
            .item(tag::ENCRYPTED_DATA, &sub)
            .finish())
    }
}

/// Accessory side of pair-verify.
pub struct PairVerifyServer {
    id: Vec<u8>,
    signing: SigningKey,
    controller_id: Vec<u8>,
    controller_ltpk: [u8; 32],
    secret: StaticSecret,
    state: Option<VerifyState>,
    shared: Option<SharedSecret>,
}

struct VerifyState {
    shared: SharedSecret,
    key: [u8; 32],
    client_public: [u8; 32],
}

impl PairVerifyServer {
    /// Creates a server that accepts one paired controller.
    #[must_use]
    pub fn new(
        id: &[u8],
        ltsk: &[u8; 32],
        controller_id: &[u8],
        controller_ltpk: [u8; 32],
        ephemeral: [u8; 32],
    ) -> Self {
        Self {
            id: id.to_vec(),
            signing: SigningKey::from_bytes(ltsk),
            controller_id: controller_id.to_vec(),
            controller_ltpk,
            secret: StaticSecret::from(ephemeral),
            state: None,
            shared: None,
        }
    }

    /// Shared secret after a successful M3.
    #[must_use]
    pub const fn shared_secret(&self) -> Option<&SharedSecret> {
        self.shared.as_ref()
    }

    /// Handles M1 or M3 and returns the reply.
    pub fn handle(&mut self, msg: &[u8]) -> Result<Vec<u8>, Error> {
        let tlv = Tlv8::decode(msg)?;
        match tlv.get_u8(tag::STATE) {
            Some(1) => {
                let client_public = required_32(&tlv, tag::PUBLIC_KEY)?;
                let own_public = PublicKey::from(&self.secret);
                let shared = SharedSecret::new(
                    self.secret
                        .diffie_hellman(&PublicKey::from(client_public))
                        .as_bytes(),
                );
                let key: [u8; 32] = hkdf_sha512(
                    label::PAIR_VERIFY_ENCRYPT_SALT,
                    label::PAIR_VERIFY_ENCRYPT_INFO,
                    shared.as_bytes(),
                );
                let info = [own_public.as_bytes(), &self.id[..], &client_public].concat();
                let signature = self.signing.sign(&info).to_bytes();
                let mut sub = Writer::new()
                    .item(tag::IDENTIFIER, &self.id)
                    .item(tag::SIGNATURE, &signature)
                    .finish();
                seal_message(&key, label::PV_MSG02, &mut sub);
                self.state = Some(VerifyState {
                    shared,
                    key,
                    client_public,
                });
                Ok(Writer::new()
                    .byte(tag::STATE, 2)
                    .item(tag::PUBLIC_KEY, own_public.as_bytes())
                    .item(tag::ENCRYPTED_DATA, &sub)
                    .finish())
            }
            Some(3) => {
                let st = self.state.take().ok_or(Error::UnexpectedState {
                    expected: 1,
                    got: Some(3),
                })?;
                let plain = open_message(
                    &st.key,
                    label::PV_MSG03,
                    required(&tlv, tag::ENCRYPTED_DATA)?,
                )?;
                let sub = Tlv8::decode(&plain)?;
                let id = required(&sub, tag::IDENTIFIER)?;
                let signature = Signature::from_slice(required(&sub, tag::SIGNATURE)?)
                    .map_err(|_| Error::BadSignature)?;
                let own_public = PublicKey::from(&self.secret);
                let info = [&st.client_public[..], id, own_public.as_bytes()].concat();
                let valid = id == self.controller_id
                    && VerifyingKey::from_bytes(&self.controller_ltpk)
                        .is_ok_and(|k| k.verify_strict(&info, &signature).is_ok());
                if !valid {
                    return Ok(error_reply(4, 2));
                }
                self.shared = Some(st.shared);
                Ok(Writer::new().byte(tag::STATE, 4).finish())
            }
            got => Err(Error::UnexpectedState { expected: 1, got }),
        }
    }
}

/// Deterministic generator for tests. Not secure.
#[derive(Debug, Clone)]
pub struct TestRng(pub u64);

impl TryRng for TestRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        Ok(self.try_next_u64()? as u32)
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Ok(z ^ (z >> 31))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        for chunk in dst.chunks_mut(8) {
            let v = self.try_next_u64()?.to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
        Ok(())
    }
}

impl TryCryptoRng for TestRng {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pair_setup::ClientIdentity;
    use crate::{Credentials, DeviceError, pair_setup, pair_verify};

    const DEVICE_ID: &[u8] = b"AA:BB:CC:DD:EE:FF";

    fn server() -> PairSetupServer {
        PairSetupServer::new(DEVICE_ID, &[0x22; 32], "1234", [5; 16], &[0xBB; 32])
    }

    fn pair(server: &mut PairSetupServer, rng: &mut TestRng) -> Credentials {
        let (m1, st) = pair_setup::start(false);
        let st = st.handle_m2(&server.handle(&m1).unwrap()).unwrap();
        let (m3, st) = st.submit_pin("1234", rng).unwrap();
        let verified = st.handle_m4(&server.handle(&m3).unwrap()).unwrap();
        assert!(!verified.is_transient());
        let identity = ClientIdentity {
            id: b"client-1",
            metadata: Some(b"meta"),
        };
        let (m5, st) = verified.exchange(identity, rng);
        st.handle_m6(&server.handle(&m5).unwrap()).unwrap()
    }

    #[test]
    fn full_pair_setup() {
        let mut rng = TestRng(1);
        let mut server = server();
        let creds = pair(&mut server, &mut rng);
        assert_eq!(creds.accessory_id, DEVICE_ID);
        assert_eq!(creds.accessory_ltpk, server.ltpk());
        let controller = server.controller().unwrap();
        assert_eq!(controller.id, b"client-1");
        assert_eq!(controller.metadata.as_deref(), Some(&b"meta"[..]));
        assert_eq!(
            controller.ltpk,
            SigningKey::from_bytes(&creds.client_ltsk)
                .verifying_key()
                .to_bytes()
        );
    }

    #[test]
    fn transient_pair_setup() {
        let mut rng = TestRng(2);
        let mut server = server();
        let (m1, st) = pair_setup::start(true);
        let st = st.handle_m2(&server.handle(&m1).unwrap()).unwrap();
        let (m3, st) = st.submit_pin("1234", &mut rng).unwrap();
        let verified = st.handle_m4(&server.handle(&m3).unwrap()).unwrap();
        assert!(verified.is_transient());
        assert!(format!("{verified:?}").contains("transient: true"));
        let key = verified.into_transient();
        assert_eq!(key.as_bytes().len(), 64);
        assert_eq!(key.as_bytes(), server.srp_key().unwrap().as_bytes());
    }

    #[test]
    fn wrong_pin() {
        let mut rng = TestRng(3);
        let mut server = server();
        let (m1, st) = pair_setup::start(false);
        let st = st.handle_m2(&server.handle(&m1).unwrap()).unwrap();
        let (m3, st) = st.submit_pin("0000", &mut rng).unwrap();
        let err = st.handle_m4(&server.handle(&m3).unwrap()).unwrap_err();
        assert_eq!(err, Error::Device(DeviceError::Authentication));
    }

    #[test]
    fn tampered_m6_fails() {
        let mut rng = TestRng(4);
        let mut server = server();
        let (m1, st) = pair_setup::start(false);
        let st = st.handle_m2(&server.handle(&m1).unwrap()).unwrap();
        let (m3, st) = st.submit_pin("1234", &mut rng).unwrap();
        let verified = st.handle_m4(&server.handle(&m3).unwrap()).unwrap();
        let (m5, st) = verified.exchange(
            ClientIdentity {
                id: b"c",
                metadata: None,
            },
            &mut rng,
        );
        let mut m6 = server.handle(&m5).unwrap();
        let last = m6.len() - 1;
        m6[last] ^= 1;
        assert_eq!(st.handle_m6(&m6).unwrap_err(), Error::Decrypt);
    }

    #[test]
    fn unexpected_states() {
        let (_, st) = pair_setup::start(false);
        let m4 = Writer::new().byte(tag::STATE, 4).finish();
        assert_eq!(
            st.handle_m2(&m4).unwrap_err(),
            Error::UnexpectedState {
                expected: 2,
                got: Some(4)
            }
        );
        let (_, st) = pair_setup::start(false);
        let m2 = Writer::new().byte(tag::STATE, 2).finish();
        assert_eq!(
            st.handle_m2(&m2).unwrap_err(),
            Error::MissingField(tag::SALT)
        );
        let busy = Writer::new()
            .byte(tag::STATE, 2)
            .byte(tag::ERROR, 3)
            .item(tag::BACKOFF, &[10, 0])
            .finish();
        let (_, st) = pair_setup::start(false);
        assert_eq!(
            st.handle_m2(&busy).unwrap_err(),
            Error::Device(DeviceError::Backoff(Some(10)))
        );
        assert!(server().handle(&m4).is_err());
    }

    #[test]
    fn device_error_codes() {
        for code in 1..=8 {
            assert_eq!(DeviceError::from_code(code, None).code(), code);
        }
    }

    fn verify_server(creds: &Credentials) -> PairVerifyServer {
        let controller_ltpk = SigningKey::from_bytes(&creds.client_ltsk)
            .verifying_key()
            .to_bytes();
        PairVerifyServer::new(
            DEVICE_ID,
            &[0x22; 32],
            &creds.client_id,
            controller_ltpk,
            [0x33; 32],
        )
    }

    #[test]
    fn pair_verify_agrees_on_secret() {
        let mut rng = TestRng(5);
        let creds = pair(&mut server(), &mut rng);
        let mut device = verify_server(&creds);
        let (m1, st) = pair_verify::start(&creds, &mut rng);
        let (m3, st) = st.handle_m2(&device.handle(&m1).unwrap()).unwrap();
        let secret = st.handle_m4(&device.handle(&m3).unwrap()).unwrap();
        assert_eq!(
            secret.as_bytes(),
            device.shared_secret().unwrap().as_bytes()
        );
        assert_eq!(
            secret.derive(b"a", b"b"),
            device.shared_secret().unwrap().derive(b"a", b"b")
        );
        assert_eq!(format!("{secret:?}"), "SharedSecret(..)");
    }

    #[test]
    fn pair_verify_rejects_wrong_device() {
        let mut rng = TestRng(6);
        let mut creds = pair(&mut server(), &mut rng);
        let mut device = verify_server(&creds);
        creds.accessory_id = b"other".to_vec();
        let (m1, st) = pair_verify::start(&creds, &mut rng);
        assert_eq!(
            st.handle_m2(&device.handle(&m1).unwrap()).unwrap_err(),
            Error::UnknownAccessory
        );
    }

    #[test]
    fn pair_verify_rejects_wrong_ltpk() {
        let mut rng = TestRng(7);
        let mut creds = pair(&mut server(), &mut rng);
        let mut device = verify_server(&creds);
        creds.accessory_ltpk = SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes();
        let (m1, st) = pair_verify::start(&creds, &mut rng);
        assert_eq!(
            st.handle_m2(&device.handle(&m1).unwrap()).unwrap_err(),
            Error::BadSignature
        );
    }

    #[test]
    fn device_rejects_unknown_controller() {
        let mut rng = TestRng(8);
        let creds = pair(&mut server(), &mut rng);
        let mut device =
            PairVerifyServer::new(DEVICE_ID, &[0x22; 32], b"someone-else", [0; 32], [0x33; 32]);
        let (m1, st) = pair_verify::start(&creds, &mut rng);
        let (m3, st) = st.handle_m2(&device.handle(&m1).unwrap()).unwrap();
        assert_eq!(
            st.handle_m4(&device.handle(&m3).unwrap()).unwrap_err(),
            Error::Device(DeviceError::Authentication)
        );
        assert!(device.shared_secret().is_none());
    }

    #[test]
    fn pair_verify_rejects_low_order_point() {
        let mut rng = TestRng(9);
        let creds = pair(&mut server(), &mut rng);
        let (_, st) = pair_verify::start(&creds, &mut rng);
        let m2 = Writer::new()
            .byte(tag::STATE, 2)
            .item(tag::PUBLIC_KEY, &[0; 32])
            .item(tag::ENCRYPTED_DATA, &[0; 20])
            .finish();
        assert_eq!(st.handle_m2(&m2).unwrap_err(), Error::InvalidPublicKey);
    }
}
