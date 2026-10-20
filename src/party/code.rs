//! Room codes, and what is derived from them for signaling.
//!
//! The code names the room; it is never sent anywhere. Relays only see a hash of it (the
//! topic), and signaling messages are encrypted with a key derived from the code and the
//! optional room password.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

/// A–Z without I and O, so codes are easy to read out.
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
pub const CODE_LEN: usize = 12;

pub fn generate() -> String {
    (0..CODE_LEN).map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char).collect()
}

/// `XYAS-KDHG-ASDF`.
pub fn display(code: &str) -> String {
    code.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-")
}

/// Accepts any case, spaces and dashes. None if it isn't a valid code.
pub fn normalize(input: &str) -> Option<String> {
    let code: String = input.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_uppercase();
    (code.len() == CODE_LEN && code.bytes().all(|b| ALPHABET.contains(&b))).then_some(code)
}

/// Public tag the relays filter on.
pub fn topic(code: &str) -> String {
    let digest = Sha256::digest(format!("trax-party/{code}"));
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn derive(material: &[u8], info: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(b"trax-party-v1"), material)
        .expand(info, &mut key)
        .expect("32 bytes is a valid HKDF length");
    key
}

/// Key for all room signaling: code and password (none = empty).
pub fn room_key(code: &str, password: &str) -> [u8; 32] {
    derive(format!("{code}\0{password}").as_bytes(), b"room")
}

/// Key known to everyone with the code; only used to say "wrong password".
pub fn code_key(code: &str) -> [u8; 32] {
    derive(code.as_bytes(), b"code-only")
}

/// ChaCha20-Poly1305 with a random nonce in front of the ciphertext.
pub fn seal(key: &[u8; 32], plain: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let nonce: [u8; 12] = rand::random();
    let mut out = nonce.to_vec();
    out.extend(cipher.encrypt(&Nonce::from(nonce), plain).unwrap_or_default());
    out
}

pub fn open(key: &[u8; 32], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < 12 + 16 {
        return None;
    }
    let nonce: [u8; 12] = sealed[..12].try_into().ok()?;
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    cipher.decrypt(&Nonce::from(nonce), &sealed[12..]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let c = generate();
        assert_eq!(c.len(), CODE_LEN);
        assert_eq!(normalize(&display(&c).to_lowercase()), Some(c.clone()));
        assert_eq!(normalize(" xyas-kdhg-asdf "), Some("XYASKDHGASDF".into()));
        assert!(normalize("XYASKDHGASD").is_none());
        assert!(normalize("XYASKDHGASD0").is_none());
        assert_eq!(display("XYASKDHGASDF"), "XYAS-KDHG-ASDF");
    }

    #[test]
    fn keys_and_sealing() {
        let key = room_key("XYASKDHGASDF", "");
        let sealed = seal(&key, b"hello");
        assert_eq!(open(&key, &sealed).as_deref(), Some(&b"hello"[..]));
        // Wrong password or code: can't open.
        assert!(open(&room_key("XYASKDHGASDF", "secret"), &sealed).is_none());
        assert!(open(&room_key("XYASKDHGASDG", ""), &sealed).is_none());
        // The code-only key differs from the room key even without a password.
        assert_ne!(code_key("XYASKDHGASDF"), key);
        // Tampering is detected.
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(open(&key, &bad).is_none());
        assert_ne!(topic("XYASKDHGASDF"), topic("XYASKDHGASDG"));
    }
}
