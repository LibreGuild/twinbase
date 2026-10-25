//! Server side of SCRAM-SHA-256 and SCRAM-SHA-1 (RFC 5802 / 7677) as MongoDB uses them.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};

const ITERATIONS: u32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mechanism {
    Sha1,
    Sha256,
}

impl Mechanism {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "SCRAM-SHA-256" => Some(Mechanism::Sha256),
            "SCRAM-SHA-1" => Some(Mechanism::Sha1),
            _ => None,
        }
    }
}

/// State between `saslStart` and `saslContinue`.
pub struct Conversation {
    mechanism: Mechanism,
    user: String,
    client_first_bare: String,
    server_first: String,
    nonce: String,
    salt: Vec<u8>,
}

fn hmac(m: Mechanism, key: &[u8], data: &[u8]) -> Vec<u8> {
    match m {
        Mechanism::Sha256 => {
            let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
        Mechanism::Sha1 => {
            let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("any key length");
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
    }
}

fn hash(m: Mechanism, data: &[u8]) -> Vec<u8> {
    match m {
        Mechanism::Sha256 => Sha256::digest(data).to_vec(),
        Mechanism::Sha1 => Sha1::digest(data).to_vec(),
    }
}

fn salted_password(m: Mechanism, user: &str, password: &str, salt: &[u8]) -> Vec<u8> {
    match m {
        Mechanism::Sha256 => {
            let mut out = [0u8; 32];
            pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, ITERATIONS, &mut out);
            out.to_vec()
        }
        Mechanism::Sha1 => {
            // MongoDB's SCRAM-SHA-1 uses the legacy MD5 password digest as the password.
            let digest = md5::Md5::digest(format!("{user}:mongo:{password}").as_bytes());
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            let mut out = [0u8; 20];
            pbkdf2::pbkdf2_hmac::<Sha1>(hex.as_bytes(), salt, ITERATIONS, &mut out);
            out.to_vec()
        }
    }
}

fn attr(msg: &str, key: char) -> Option<&str> {
    msg.split(',').find_map(|p| p.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

fn unescape_user(u: &str) -> String {
    u.replace("=2C", ",").replace("=3D", "=")
}

/// Handle the client-first message. Returns the conversation and the server-first message.
pub fn start(mechanism: Mechanism, client_first: &[u8]) -> Result<(Conversation, Vec<u8>), String> {
    let msg = std::str::from_utf8(client_first).map_err(|_| "invalid SCRAM message")?;
    // gs2 header: "n,," (no channel binding); "y" or "p=" are not supported.
    let bare = msg.strip_prefix("n,,").or_else(|| msg.strip_prefix("y,,")).ok_or("SCRAM channel binding is not supported")?;
    let user = attr(bare, 'n').ok_or("missing user name")?;
    let client_nonce = attr(bare, 'r').ok_or("missing nonce")?;
    let mut salt = vec![0u8; 16];
    let mut server_nonce = [0u8; 24];
    rand::fill(&mut salt[..]);
    rand::fill(&mut server_nonce[..]);
    let nonce = format!("{client_nonce}{}", B64.encode(server_nonce));
    let server_first = format!("r={nonce},s={},i={ITERATIONS}", B64.encode(&salt));
    let conv = Conversation {
        mechanism,
        user: unescape_user(user),
        client_first_bare: bare.to_owned(),
        server_first: server_first.clone(),
        nonce,
        salt,
    };
    Ok((conv, server_first.into_bytes()))
}

impl Conversation {
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Verify the client-final message against the expected password; returns server-final.
    pub fn finish(&self, client_final: &[u8], password: &str) -> Result<Vec<u8>, String> {
        let msg = std::str::from_utf8(client_final).map_err(|_| "invalid SCRAM message")?;
        let (without_proof, proof) = msg.rsplit_once(",p=").ok_or("missing proof")?;
        if attr(without_proof, 'r') != Some(self.nonce.as_str()) {
            return Err("nonce mismatch".into());
        }
        let proof = B64.decode(proof).map_err(|_| "invalid proof")?;
        let m = self.mechanism;
        let salted = salted_password(m, &self.user, password, &self.salt);
        let client_key = hmac(m, &salted, b"Client Key");
        let stored_key = hash(m, &client_key);
        let auth_message = format!("{},{},{}", self.client_first_bare, self.server_first, without_proof);
        let client_signature = hmac(m, &stored_key, auth_message.as_bytes());
        if proof.len() != client_key.len() {
            return Err("invalid proof".into());
        }
        // ClientKey = ClientProof XOR ClientSignature; it must hash to StoredKey.
        let recovered: Vec<u8> = proof.iter().zip(&client_signature).map(|(a, b)| a ^ b).collect();
        let candidate = hash(m, &recovered);
        let diff = candidate.iter().zip(&stored_key).fold(0u8, |acc, (a, b)| acc | (a ^ b));
        if diff != 0 {
            return Err("Authentication failed.".into());
        }
        let server_key = hmac(m, &salted, b"Server Key");
        let server_signature = hmac(m, &server_key, auth_message.as_bytes());
        Ok(format!("v={}", B64.encode(server_signature)).into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Client side of SCRAM, to check the server against the RFC's computations.
    fn client_final(m: Mechanism, user: &str, password: &str, client_first_bare: &str, server_first: &str) -> (String, Vec<u8>) {
        let salt = B64.decode(attr(server_first, 's').unwrap()).unwrap();
        let nonce = attr(server_first, 'r').unwrap();
        let salted = salted_password(m, user, password, &salt);
        let client_key = hmac(m, &salted, b"Client Key");
        let stored_key = hash(m, &client_key);
        let without_proof = format!("c=biws,r={nonce}");
        let auth = format!("{client_first_bare},{server_first},{without_proof}");
        let sig = hmac(m, &stored_key, auth.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(&sig).map(|(a, b)| a ^ b).collect();
        let server_sig = hmac(m, &hmac(m, &salted, b"Server Key"), auth.as_bytes());
        (format!("{without_proof},p={}", B64.encode(proof)), server_sig)
    }

    #[test]
    fn round_trip_both_mechanisms_and_wrong_password() {
        for m in [Mechanism::Sha256, Mechanism::Sha1] {
            let bare = "n=app,r=fyko+d2lbbFgONRv9qkxdawL";
            let (conv, server_first) = start(m, format!("n,,{bare}").as_bytes()).unwrap();
            let server_first = String::from_utf8(server_first).unwrap();
            assert!(server_first.starts_with("r=fyko+d2lbbFgONRv9qkxdawL"));
            let (fin, expected_sig) = client_final(m, "app", "s3cret", bare, &server_first);
            let server_final = conv.finish(fin.as_bytes(), "s3cret").unwrap();
            assert_eq!(server_final, format!("v={}", B64.encode(expected_sig)).into_bytes());
            assert!(conv.finish(fin.as_bytes(), "wrong").is_err());
        }
        assert!(start(Mechanism::Sha256, b"p=tls-unique,,n=a,r=x").is_err());
    }
}
