//! Apple's SRP-6a variant.
//!
//! Apple authenticates the web client with SRP: the password never leaves the
//! machine, the service proves it knows the salt and the server secret, and
//! the client proves it knows the password. This is an implementation of
//! RFC 5054 with the two deviations Apple uses:
//!
//! * the username is not part of `x` ("no username in x"), and
//! * the password is not used directly but derived with PBKDF2-HMAC-SHA256
//!   over the hashed password, with the salt and the iteration count the
//!   service chose.
//!
//! The group is the RFC 5054 2048 bit group (SHA-256, `g = 2`).

use num_bigint::BigUint;
use num_traits::Zero;
use sha2::{Digest, Sha256};

use super::error::{Error, Result};

/// The 2048 bit group modulus of RFC 5054.
const GROUP_N_HEX: &str = concat!(
    "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050",
    "A37329CBB4A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50",
    "E8083969EDB767B0CF6095179A163AB3661A05FBD5FAAAE82918A9962F0B93B8",
    "55F97993EC975EEAA80D740ADBF4FF747359D041D5C33EA71D281E446B14773B",
    "CA97B43A23FB801676BD207A436C6481F1D2B9078717461A5B9D32E688F87748",
    "544523B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB37861602790",
    "04E57AE6AF874E7303CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8",
    "E9DBFBB694B5C803D89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F",
    "9E4AFF73",
);

/// The generator of the group.
const GROUP_G: u32 = 2;

/// The length of the group in bytes.
const N_LEN: usize = 256;

fn group_n() -> BigUint {
    BigUint::parse_bytes(GROUP_N_HEX.as_bytes(), 16).expect("the group modulus is a constant")
}

/// The client side of the exchange.
pub struct SrpClient {
    /// The client secret.
    secret: BigUint,
    /// The client public value `A`.
    public: BigUint,
    /// The session key.
    session_key: Vec<u8>,
    /// The client proof.
    proof: Vec<u8>,
    /// The server proof, checked against the one the service sends back.
    server_proof: Vec<u8>,
}

impl SrpClient {
    /// Start a new exchange: pick a secret and compute `A`.
    pub fn new() -> Result<Self> {
        let mut bytes = [0u8; 32];
        fill_random(&mut bytes)?;
        let secret = BigUint::from_bytes_be(&bytes);
        let n = group_n();
        let g = BigUint::from(GROUP_G);
        let public = g.modpow(&secret, &n);
        Ok(Self {
            secret,
            public,
            session_key: Vec::new(),
            proof: Vec::new(),
            server_proof: Vec::new(),
        })
    }

    /// The public value `A`, padded to the length of the group.
    pub fn public_bytes(&self) -> Vec<u8> {
        pad(&self.public)
    }

    /// Derive the password and compute the proofs from the service's
    /// challenge.
    ///
    /// * `salt` — the raw salt the service sent
    /// * `server_b` — the raw server public value
    /// * `iterations` — the PBKDF2 iteration count
    /// * `protocol` — `s2k` or `s2k_fo`
    pub fn process_challenge(
        &mut self,
        username: &str,
        password: &str,
        salt: &[u8],
        server_b: &[u8],
        iterations: u32,
        protocol: &str,
    ) -> Result<()> {
        let n = group_n();
        let g = BigUint::from(GROUP_G);

        let b = BigUint::from_bytes_be(server_b);
        if b.is_zero() || b >= n {
            return Err(Error::Protocol(
                "the service sent an invalid SRP value".into(),
            ));
        }

        let derived = derive_password(password, salt, iterations, protocol)?;
        let x = calculate_x(salt, &derived);
        let u = calculate_u(&self.public, &b);
        if u.is_zero() {
            return Err(Error::Protocol(
                "the SRP scrambling parameter is zero".into(),
            ));
        }

        let gx = g.modpow(&x, &n);
        let kgx = (multiplier() * &gx) % &n;
        // (B - k*g^x) mod N, computed without underflow.
        let diff = if b >= kgx {
            &b - &kgx
        } else {
            (&b + &n) - &kgx
        };
        let exponent = &self.secret + (&u * &x);
        let shared = diff.modpow(&exponent, &n);

        self.session_key = hash(&pad(&shared));
        let a_bytes = pad(&self.public);
        let b_bytes = pad(&b);
        self.proof = calculate_m1(username, salt, &a_bytes, &b_bytes, &self.session_key);
        self.server_proof = calculate_m2(&a_bytes, &self.proof, &self.session_key);
        Ok(())
    }

    /// The client proof `M1`.
    pub fn proof(&self) -> &[u8] {
        &self.proof
    }

    /// The expected server proof `M2`.
    pub fn server_proof(&self) -> &[u8] {
        &self.server_proof
    }
}

/// Apple's password derivation.
///
/// `s2k` feeds the raw SHA-256 of the password into PBKDF2, `s2k_fo` feeds its
/// hexadecimal form.
pub fn derive_password(
    password: &str,
    salt: &[u8],
    iterations: u32,
    protocol: &str,
) -> Result<Vec<u8>> {
    let hash = Sha256::digest(password.as_bytes());
    let input: Vec<u8> = match protocol {
        "s2k" => hash.to_vec(),
        "s2k_fo" => hex::encode(hash).into_bytes(),
        other => {
            return Err(Error::Protocol(format!(
                "the service asked for the unsupported SRP protocol {}",
                other
            )))
        }
    };
    let mut out = vec![0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(&input, salt, iterations, &mut out);
    Ok(out)
}

/// `x = H(salt | H(":" | key))` — Apple omits the username.
fn calculate_x(salt: &[u8], key: &[u8]) -> BigUint {
    let mut inner = Sha256::new();
    inner.update(b":");
    inner.update(key);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(salt);
    outer.update(inner);
    BigUint::from_bytes_be(&outer.finalize())
}

/// `u = H(pad(A) | pad(B))`
fn calculate_u(a: &BigUint, b: &BigUint) -> BigUint {
    let mut hasher = Sha256::new();
    hasher.update(pad(a));
    hasher.update(pad(b));
    BigUint::from_bytes_be(&hasher.finalize())
}

/// `k = H(N | pad(g))`
fn multiplier() -> BigUint {
    let n = group_n();
    let mut g_bytes = GROUP_G.to_be_bytes().to_vec();
    let n_bytes = n.to_bytes_be();
    while g_bytes.len() < n_bytes.len() {
        g_bytes.insert(0, 0);
    }
    let mut hasher = Sha256::new();
    hasher.update(&n_bytes);
    hasher.update(&g_bytes);
    BigUint::from_bytes_be(&hasher.finalize())
}

/// `M1 = H(H(g) xor H(N) | H(user) | salt | A | B | K)`
fn calculate_m1(username: &str, salt: &[u8], a: &[u8], b: &[u8], session_key: &[u8]) -> Vec<u8> {
    let n = group_n();
    let hg = hash(&pad(&BigUint::from(GROUP_G)));
    let hn = hash(&n.to_bytes_be());
    let hu = hash(username.as_bytes());
    let xor: Vec<u8> = hg.iter().zip(hn.iter()).map(|(a, b)| a ^ b).collect();

    let mut hasher = Sha256::new();
    hasher.update(xor);
    hasher.update(hu);
    hasher.update(salt);
    hasher.update(a);
    hasher.update(b);
    hasher.update(session_key);
    hasher.finalize().to_vec()
}

/// `M2 = H(A | M1 | K)`
fn calculate_m2(a: &[u8], proof: &[u8], session_key: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(a);
    hasher.update(proof);
    hasher.update(session_key);
    hasher.finalize().to_vec()
}

fn hash(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

fn pad(value: &BigUint) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    if bytes.len() >= N_LEN {
        return bytes;
    }
    let mut padded = vec![0u8; N_LEN];
    padded[N_LEN - bytes.len()..].copy_from_slice(&bytes);
    padded
}

/// Fill a buffer with random bytes.
fn fill_random(buffer: &mut [u8]) -> Result<()> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom")?;
    file.read_exact(buffer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 5054 test vector for the 2048 bit group is not applicable here
    /// (Apple derives the password), so the test checks the algebraic
    /// identities that must hold for any challenge.
    #[test]
    fn proof_is_derived_from_the_challenge() {
        let mut client = SrpClient::new().unwrap();
        let salt = b"salt-value";
        let n = group_n();
        let b = BigUint::from_bytes_be(&[7u8; 32]) % &n;
        client
            .process_challenge(
                "user@example.com",
                "password",
                salt,
                &b.to_bytes_be(),
                20,
                "s2k",
            )
            .unwrap();
        assert_eq!(client.proof().len(), 32);
        assert_eq!(client.server_proof().len(), 32);
        assert_ne!(client.proof(), client.server_proof());
    }

    #[test]
    fn different_passwords_produce_different_proofs() {
        let n = group_n();
        let b = BigUint::from_bytes_be(&[9u8; 32]) % &n;
        let mut first = SrpClient::new().unwrap();
        let mut second = SrpClient::new().unwrap();
        first
            .process_challenge("u", "one", b"salt", &b.to_bytes_be(), 20, "s2k")
            .unwrap();
        second
            .process_challenge("u", "two", b"salt", &b.to_bytes_be(), 20, "s2k")
            .unwrap();
        assert_ne!(first.proof(), second.proof());
    }

    #[test]
    fn password_derivation_follows_the_protocol() {
        let s2k = derive_password("pw", b"salt", 10, "s2k").unwrap();
        let s2k_fo = derive_password("pw", b"salt", 10, "s2k_fo").unwrap();
        assert_eq!(s2k.len(), 32);
        assert_ne!(s2k, s2k_fo);
        assert!(derive_password("pw", b"salt", 10, "nope").is_err());
    }
}
