//! SRP-6a with the RFC 5054 3072-bit group and SHA-512, as used by HAP.
//!
//! Values that enter a hash are padded to the group size (384 bytes),
//! except `S` in `K = H(S)`: Apple devices hash `S` without leading zero bytes.

use std::sync::LazyLock;

use num_bigint::BigUint;
use sha2::{Digest, Sha512};
use zeroize::Zeroizing;

use crate::Error;

/// Size of the group modulus in bytes.
pub const GROUP_LEN: usize = 384;

/// User name for HAP pair-setup.
pub const USERNAME: &[u8] = b"Pair-Setup";

const N_HEX: &[u8] = b"\
FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA6\
3B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245\
E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F2411\
7C4B1FE649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F\
83655D23DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08\
CA18217C32905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9\
DE2BCBF6955817183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D\
04507A33A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7\
ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D8760273\
3EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB31\
43DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";

struct Group {
    n: BigUint,
    g: BigUint,
    k: BigUint,
    // H(N) xor H(g)
    hn_xor_hg: [u8; 64],
}

static GROUP: LazyLock<Group> = LazyLock::new(|| {
    let n = BigUint::parse_bytes(N_HEX, 16).expect("valid prime");
    let g = BigUint::from(5u32);
    let k = BigUint::from_bytes_be(&hash(&[&pad(&n), &pad(&g)]));
    let hn = hash(&[&n.to_bytes_be()]);
    let hg = hash(&[&g.to_bytes_be()]);
    let mut hn_xor_hg = [0u8; 64];
    for (i, b) in hn_xor_hg.iter_mut().enumerate() {
        *b = hn[i] ^ hg[i];
    }
    Group { n, g, k, hn_xor_hg }
});

fn hash(parts: &[&[u8]]) -> [u8; 64] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn pad(v: &BigUint) -> [u8; GROUP_LEN] {
    let bytes = v.to_bytes_be();
    let mut out = [0u8; GROUP_LEN];
    out[GROUP_LEN - bytes.len()..].copy_from_slice(&bytes);
    out
}

// K = H(S), with `S` not padded.
fn session_key(s: &BigUint) -> [u8; 64] {
    hash(&[&Zeroizing::new(s.to_bytes_be())])
}

// x = H(salt | H(username ":" password))
fn private_x(salt: &[u8], password: &[u8]) -> BigUint {
    let inner = hash(&[USERNAME, b":", password]);
    BigUint::from_bytes_be(&hash(&[salt, &inner]))
}

fn proofs(
    a_pub: &[u8; GROUP_LEN],
    b_pub: &[u8; GROUP_LEN],
    salt: &[u8],
    key: &[u8; 64],
) -> ([u8; 64], [u8; 64]) {
    let g = &*GROUP;
    let m1 = hash(&[&g.hn_xor_hg, &hash(&[USERNAME]), salt, a_pub, b_pub, key]);
    let m2 = hash(&[a_pub, &m1, key]);
    (m1, m2)
}

/// Client side of an SRP exchange.
pub struct SrpClient {
    a_pub: [u8; GROUP_LEN],
    m1: [u8; 64],
    m2: [u8; 64],
    key: Zeroizing<[u8; 64]>,
}

impl SrpClient {
    /// Runs the client computation.
    ///
    /// `salt` and `b_pub` come from the device, `a_private` must be random.
    #[allow(clippy::many_single_char_names)]
    pub fn new(
        password: &[u8],
        salt: &[u8],
        b_pub: &[u8],
        a_private: &[u8; 32],
    ) -> Result<Self, Error> {
        let grp = &*GROUP;
        let b = BigUint::from_bytes_be(b_pub);
        if b_pub.len() > GROUP_LEN || (&b % &grp.n) == BigUint::ZERO {
            return Err(Error::InvalidSrpParameters);
        }
        let a = BigUint::from_bytes_be(a_private);
        let a_pub = pad(&grp.g.modpow(&a, &grp.n));
        let b_pad = pad(&b);
        let u = BigUint::from_bytes_be(&hash(&[&a_pub, &b_pad]));
        if u == BigUint::ZERO {
            return Err(Error::InvalidSrpParameters);
        }
        let x = private_x(salt, password);
        let kgx = (&grp.k * grp.g.modpow(&x, &grp.n)) % &grp.n;
        let base = (&b + &grp.n - kgx) % &grp.n;
        let s = base.modpow(&(a + u * x), &grp.n);
        let key = Zeroizing::new(session_key(&s));
        let (m1, m2) = proofs(&a_pub, &b_pad, salt, &key);
        Ok(Self { a_pub, m1, m2, key })
    }

    /// Client public value `A`.
    #[must_use]
    pub const fn public_key(&self) -> &[u8; GROUP_LEN] {
        &self.a_pub
    }

    /// Client proof `M1`.
    #[must_use]
    pub const fn proof(&self) -> &[u8; 64] {
        &self.m1
    }

    /// Checks the device proof `M2` and returns the session key `K`.
    pub fn verify(&self, server_proof: &[u8]) -> Result<&[u8; 64], Error> {
        if server_proof == self.m2 {
            Ok(&self.key)
        } else {
            Err(Error::BadProof)
        }
    }
}

/// Server (accessory) side of an SRP exchange.
#[cfg(any(test, feature = "accessory"))]
pub struct SrpServer {
    salt: Vec<u8>,
    verifier: BigUint,
    b: BigUint,
    b_pub: [u8; GROUP_LEN],
}

#[cfg(any(test, feature = "accessory"))]
impl SrpServer {
    /// Creates the server state for `password`. `b_private` must be random.
    #[must_use]
    pub fn new(password: &[u8], salt: &[u8], b_private: &[u8; 32]) -> Self {
        let grp = &*GROUP;
        let verifier = grp.g.modpow(&private_x(salt, password), &grp.n);
        let b = BigUint::from_bytes_be(b_private);
        let b_pub = pad(&((&grp.k * &verifier + grp.g.modpow(&b, &grp.n)) % &grp.n));
        Self {
            salt: salt.to_vec(),
            verifier,
            b,
            b_pub,
        }
    }

    /// Server public value `B`.
    #[must_use]
    pub const fn public_key(&self) -> &[u8; GROUP_LEN] {
        &self.b_pub
    }

    /// Checks the client proof. Returns the server proof `M2` and session key `K`.
    pub fn verify(&self, a_pub: &[u8], client_proof: &[u8]) -> Result<([u8; 64], [u8; 64]), Error> {
        let grp = &*GROUP;
        let a = BigUint::from_bytes_be(a_pub);
        if (&a % &grp.n) == BigUint::ZERO {
            return Err(Error::InvalidSrpParameters);
        }
        let a_pad = pad(&a);
        let u = BigUint::from_bytes_be(&hash(&[&a_pad, &self.b_pub]));
        let s = (a * self.verifier.modpow(&u, &grp.n)).modpow(&self.b, &grp.n);
        let key = session_key(&s);
        let (m1, m2) = proofs(&a_pad, &self.b_pub, &self.salt, &key);
        if m1.as_slice() == client_proof {
            Ok((m2, key))
        } else {
            Err(Error::BadProof)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_and_server_agree() {
        let salt = [7u8; 16];
        let server = SrpServer::new(b"1234", &salt, &[3; 32]);
        let client = SrpClient::new(b"1234", &salt, server.public_key(), &[9; 32]).unwrap();
        let (m2, server_key) = server.verify(client.public_key(), client.proof()).unwrap();
        assert_eq!(client.verify(&m2).unwrap(), &server_key);
    }

    #[test]
    fn wrong_pin_fails() {
        let salt = [7u8; 16];
        let server = SrpServer::new(b"1234", &salt, &[3; 32]);
        let client = SrpClient::new(b"4321", &salt, server.public_key(), &[9; 32]).unwrap();
        assert_eq!(
            server.verify(client.public_key(), client.proof()),
            Err(Error::BadProof)
        );
        assert_eq!(client.verify(&[0; 64]).err(), Some(Error::BadProof));
    }

    #[test]
    fn rejects_zero_public_keys() {
        let n = GROUP.n.to_bytes_be();
        assert_eq!(
            SrpClient::new(b"1", &[0; 16], &n, &[1; 32]).err(),
            Some(Error::InvalidSrpParameters)
        );
        assert_eq!(
            SrpClient::new(b"1", &[0; 16], &[0], &[1; 32]).err(),
            Some(Error::InvalidSrpParameters)
        );
        assert_eq!(
            SrpClient::new(b"1", &[0; 16], &[1; GROUP_LEN + 1], &[1; 32]).err(),
            Some(Error::InvalidSrpParameters)
        );
        let server = SrpServer::new(b"1", &[0; 16], &[1; 32]);
        assert_eq!(
            server.verify(&n, &[0; 64]),
            Err(Error::InvalidSrpParameters)
        );
    }

    // Generated with Python srptools (unpadded variant). All values here have no
    // leading zero bytes, so padded and unpadded SRP give the same result.
    #[test]
    fn matches_srptools() {
        let b_pub = hex(concat!(
            "2bc0328d1f5d293a4c1ecaa9cf40c960769620d0bb41c41682df4aabf9cff5b3530058defd89bf2a314c580ae34a4f8b",
            "f8a01b7d43beb519c0155ed91414eba5851741ae9d0042e62b175fc86602cfe1f21a221bfa0a1f7b06fdb0b726684c5b",
            "b130a175f5189fcd404d457e25c31f8ee56a29f50fe6fc32c62a235d843de1178febff7eb1f98aeca2b040b57aa3aac6",
            "e2a1806cc3197ffbd117d436f655f153acc4538c220af6a32b2d3a6b3c3145205b3007abf7206f5614100c783bcebee9",
            "47481ea0ed78d9d7effcff387d683d0869e949008b5ec134a9d66eaf4203d248a27a8e0101d90ea2c46336f87b66d037",
            "ce81929270884da788b65cd7270d90443592bacb086dfa05eb4436f59ee612ed9cf4c38b752aa7f226e0003b5c39998f",
            "3dd7e877974343545db21d03ee2fd6805df1932a7114c5fbd3be7ae83e3585a7802739e079c9ff28302e1a5e8f531aec",
            "0cfdb7d761431d935a1ffd2cd76d64a59f223f5780d81eedf5a3cdafa516aa4814dfae131e10f0a64710ed553417e3fb",
        ));
        let a_pub = hex(concat!(
            "527b274487f59bd8eefbd84492170ca8674b908b14fa16cb80b59b843fe8f4f78621852e6d3af2eb5ad52f223942e7d3",
            "de2352e43a022190f17f2f23c02b6b2af09eae23ed2e52074b73eefefa8573fed184dcea95c99a20d01046dbffe5fd17",
            "d3a8c7616de49a10b990183111c8b2032386cec4ec5c3e8d70ca7696d3cea2c2a9ab970c22e9706ee34f7ffff1a0d4af",
            "09605dc1814fd4ee6dbea2d430b6a25fe47bffbe221a95db81bdbc0a213d263c5e13e10c89e9d3f46155a3f3b944064c",
            "819376140a09b61a2f4e62c21665e4d4e2b991f92a23720888e72726456d6b3f79805532cef564307c7d119e05d06572",
            "387109d46d88008419ad80a4da83f6fa5a25b1e955b7dd14bb06fbbc54553cd955174c5df33eb1e540f00389a06f0e9a",
            "2ceec8fd1c67bc750d3abbdacf39b75194fbb35a36c9a1657539ca7d82dbcb5aa25bf45f8ca78ae894fdbf30feaf6ad4",
            "e1a97a57a86f4f0cd46b22b0c4e74ff42a5c65143a2f91043e00ad29c60094526cac43354c4895bbcdcc5a0db12818c3",
        ));
        let salt: Vec<u8> = (1..=16).collect();
        let client = SrpClient::new(b"1234", &salt, &b_pub, &[0xAA; 32]).unwrap();
        assert_eq!(client.public_key().to_vec(), a_pub);
        assert_eq!(
            client.proof().to_vec(),
            hex(
                "c21c32b00adfaae5327aababbbbff69e9d8e0bdbfd9b6be3dd36a9a8a84af7fd1bc21d12f0685ea9891ef4a9055d512757f12af2ef63d9efdb8e6e222093fa35"
            )
        );
        let key = client.verify(&hex("7cc04b5156679b82dafd0a06a77d6d9423a686cb691820b59ab32e58dc21ab0da2628bde730fcd75889cc60a0d0f01a3892b21e40b7c1a1af472919bab5aafe8")).unwrap();
        assert_eq!(
            key.to_vec(),
            hex(
                "bd0ed62e5fda9f34ea37f4256dc8d58a1d53b56e7bae00eedfb49a51ef38429f143059c504a3e30bb6dd94bf18e23d7e05cfdf421c5e468355e59056c7f23f54"
            )
        );
        let server = SrpServer::new(b"1234", &salt, &[0xBB; 32]);
        assert_eq!(server.public_key().to_vec(), b_pub);
    }

    // Generated with plain Python big integers. `S` starts with a zero byte here,
    // which is the case where padded and unpadded `K = H(S)` differ.
    #[test]
    fn session_key_without_padding() {
        let mut a_private = [0xAA; 32];
        a_private[30..].copy_from_slice(&[0xab, 0xa4]);
        let salt: Vec<u8> = (1..=16).collect();
        let server = SrpServer::new(b"1234", &salt, &[0xBB; 32]);
        let client = SrpClient::new(b"1234", &salt, server.public_key(), &a_private).unwrap();
        assert_eq!(
            client.proof().to_vec(),
            hex(
                "c792a208ffe2bde3e54f6673b2431272657fbc88afa2d5228d428a15bee02359811d10da5f40880ac8d556e88859ea018d0ab291c3560477ce8bc59412cff206"
            )
        );
        let (m2, key) = server.verify(client.public_key(), client.proof()).unwrap();
        let expected = hex(
            "c41a5d72c6599455695acce5a95c36287e8ed7a4d9febaba6e8f3f7b6710b7e420f2c043c24f77833a1300046d50eda8531c2b241d13c1e7d13c2688e25eeddb",
        );
        assert_eq!(key.to_vec(), expected);
        assert_eq!(client.verify(&m2).unwrap().to_vec(), expected);
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn group_constants() {
        assert_eq!(GROUP.n.to_bytes_be().len(), GROUP_LEN);
        assert_eq!(pad(&BigUint::from(1u8))[GROUP_LEN - 1], 1);
    }
}
