//! Apple GSA/SRP proof generation.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use num_bigint::BigUint;
use pbkdf2::pbkdf2_hmac;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

const GROUP_SIZE_BYTES: usize = 256;
const GROUP_GENERATOR: u8 = 2;
const GROUP_MODULUS_HEX: &str = concat!(
    "AC6BDB41324A9A9BF166DE5E1389582FAF72B6651987EE07FC3192943DB56050",
    "A37329CBB4A099ED8193E0757767A13DD52312AB4B03310DCD7F48A9DA04FD50",
    "E8083969EDB767B0CF6095179A163AB3661A05FBD5FAAAE82918A9962F0B93B855",
    "F97993EC975EEAA80D740ADBF4FF747359D041D5C33EA71D281E446B14773BCA97",
    "B43A23FB801676BD207A436C6481F1D2B9078717461A5B9D32E688F87748544523",
    "B524B0D57D5EA77A2775D2ECFA032CFBDBF52FB3786160279004E57AE6AF874E73",
    "03CE53299CCC041C7BC308D82A5698F3A8D0C38271AE35F8E9DBFBB694B5C803D",
    "89F7AE435DE236D525F54759B65E372FCD68EF20FA7111F9E4AFF73"
);

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SrpInitResponse {
    pub iteration: u32,
    pub salt: String,
    pub protocol: String,
    pub b: String,
    pub c: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SrpProof {
    pub account_name: String,
    pub c: String,
    pub m1: String,
    pub m2: String,
    pub remember_me: bool,
    pub trust_tokens: Vec<String>,
}

pub(crate) struct AppleSrp {
    username: String,
    private_key: BigUint,
    public_key: BigUint,
}

impl AppleSrp {
    pub fn new(username: &str) -> Result<Self> {
        let mut private_key = [0_u8; GROUP_SIZE_BYTES];
        getrandom::fill(&mut private_key)
            .map_err(|error| Error::InvalidSrp(format!("random generation failed: {error}")))?;
        Ok(Self::with_private_key(username, &private_key))
    }

    fn with_private_key(username: &str, private_key: &[u8]) -> Self {
        let modulus = group_modulus();
        let private_key = BigUint::from_bytes_be(private_key);
        let public_key = BigUint::from(GROUP_GENERATOR).modpow(&private_key, &modulus);
        Self {
            username: username.to_owned(),
            private_key,
            public_key,
        }
    }

    pub fn public_key_base64(&self) -> String {
        BASE64.encode(self.public_key.to_bytes_be())
    }

    pub fn proof(
        &self,
        password: &SecretString,
        response: &SrpInitResponse,
        trust_token: Option<&str>,
    ) -> Result<SrpProof> {
        let salt = BASE64.decode(&response.salt)?;
        let server_public_bytes = BASE64.decode(&response.b)?;
        let server_public = BigUint::from_bytes_be(&server_public_bytes);
        let modulus = group_modulus();

        if (&server_public % &modulus) == BigUint::default() {
            return Err(Error::InvalidSrp(
                "server public key is zero modulo N".into(),
            ));
        }

        let mut password_hash = Sha256::digest(password.expose_secret().as_bytes()).to_vec();
        match response.protocol.as_str() {
            "s2k" => {}
            "s2k_fo" => password_hash = lowercase_hex(&password_hash).into_bytes(),
            protocol => return Err(Error::UnsupportedSrpProtocol(protocol.to_owned())),
        }

        let mut derived_password = [0_u8; 32];
        pbkdf2_hmac::<Sha256>(
            &password_hash,
            &salt,
            response.iteration,
            &mut derived_password,
        );

        let generator = BigUint::from(GROUP_GENERATOR);
        let multiplier = hash_biguint(&[&modulus.to_bytes_be(), &pad(&generator)]);
        let scrambling = hash_biguint(&[&pad(&self.public_key), &pad(&server_public)]);
        if scrambling == BigUint::default() {
            return Err(Error::InvalidSrp("scrambling parameter is zero".into()));
        }

        let identity_hash = hash(&[b":", &derived_password]);
        let private_key = hash_biguint(&[&salt, &identity_hash]);
        let verifier = generator.modpow(&private_key, &modulus);
        let subtrahend = (&multiplier * verifier) % &modulus;
        let base = ((&modulus + &server_public) - subtrahend) % &modulus;
        let exponent = &self.private_key + &scrambling * private_key;
        let shared_secret = base.modpow(&exponent, &modulus);
        let session_key = hash(&[&shared_secret.to_bytes_be()]);

        let modulus_hash = hash(&[&modulus.to_bytes_be()]);
        let generator_hash = hash(&[&pad(&generator)]);
        let xor_hash: Vec<u8> = modulus_hash
            .iter()
            .zip(generator_hash)
            .map(|(left, right)| left ^ right)
            .collect();
        let username_hash = hash(&[self.username.as_bytes()]);
        let client_proof = hash(&[
            &xor_hash,
            &username_hash,
            &salt,
            &self.public_key.to_bytes_be(),
            &server_public.to_bytes_be(),
            &session_key,
        ]);
        let server_proof = hash(&[&self.public_key.to_bytes_be(), &client_proof, &session_key]);

        Ok(SrpProof {
            account_name: self.username.clone(),
            c: response.c.clone(),
            m1: BASE64.encode(client_proof),
            m2: BASE64.encode(server_proof),
            remember_me: true,
            trust_tokens: trust_token.map(ToOwned::to_owned).into_iter().collect(),
        })
    }
}

fn group_modulus() -> BigUint {
    BigUint::parse_bytes(GROUP_MODULUS_HEX.as_bytes(), 16)
        .expect("the RFC 5054 group modulus is valid hexadecimal")
}

fn pad(value: &BigUint) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    if bytes.len() >= GROUP_SIZE_BYTES {
        return bytes;
    }

    let mut padded = vec![0; GROUP_SIZE_BYTES - bytes.len()];
    padded.extend(bytes);
    padded
}

fn hash(parts: &[&[u8]]) -> Vec<u8> {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update(part);
    }
    digest.finalize().to_vec()
}

fn hash_biguint(parts: &[&[u8]]) -> BigUint {
    BigUint::from_bytes_be(&hash(parts))
}

fn lowercase_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_py_srp_apple_gsa_vector() {
        let private_key: Vec<u8> = (0..=u8::MAX).collect();
        let server_public = hex::decode(concat!(
            "108cccad309214177b6a7ecb08576d48bc6c57c913ad86ef087c5e161c1c9fe4",
            "6d86c600dc889b373d835693b86e26da561e996a8e06c4ca5d677d1bda8ac9ec",
            "8b03ea5b347f5349c393979b3479c4611fe0a43ad573815f18818b02cd5970859",
            "bf4df8128f95132a145d470276dfb9a218f5319f28682a116af0109a9c1dec80",
            "2bed1831383836510abd5fccc997ba57ac47a7eaf1301b2b9fff8564e3dfd63b6",
            "ca7190c499ef2709f173d2b87e8bb3d0176b2009b3c1febe1b026284e84e208c",
            "9aa596dfc1f034330f54a9ab30d772404051b4515e196a48d388040cda7644fe5",
            "2cfb86b2e7f1808e147d9db7c1016e29b3efb5804e8242ec4ba48adacefa1"
        ))
        .unwrap();
        let srp = AppleSrp::with_private_key("alice@example.com", &private_key);
        let response = SrpInitResponse {
            iteration: 1000,
            salt: BASE64.encode(hex::decode("00112233445566778899aabbccddeeff").unwrap()),
            protocol: "s2k_fo".into(),
            b: BASE64.encode(server_public),
            c: "challenge".into(),
        };

        let proof = srp
            .proof(
                &SecretString::from("correct horse battery staple"),
                &response,
                None,
            )
            .unwrap();

        assert_eq!(
            hex::encode(BASE64.decode(proof.m1).unwrap()),
            "ba5afe63ccea9ff9cb0456a5def002f3e3cccb60c9067b426490e14b8b6d321f"
        );
        assert_eq!(
            hex::encode(BASE64.decode(proof.m2).unwrap()),
            "8d52eb47fe5af9eec65dc3ae86281db58f5fa4e7faf1ddda9cfdb2d99690f878"
        );
    }
}
