//! Your profile: a Nostr keypair that is unique to you, plus the display name.
//!
//! The public key is the profile id. Its `npub1…` form is the friend code people paste to
//! add you; a short tag from it is shown next to every name, so two "Alex" stay apart.

use nostr::prelude::{Event, EventBuilder, FinalizeEvent, Keys, Kind, PublicKey, ToBech32};

/// Kind of the event proving a party member's profile. Never published.
const PROOF_KIND: u16 = 25052;

/// The keys stored in `secret` (hex), or new ones if it is empty or broken. The bool says
/// whether new keys were made (and must be saved).
pub fn load_or_create(secret: &str) -> (Keys, bool) {
    match Keys::parse(secret.trim()) {
        Ok(keys) if !secret.trim().is_empty() => (keys, false),
        _ => (Keys::generate(), true),
    }
}

pub fn secret_hex(keys: &Keys) -> String {
    keys.secret_key().to_secret_hex()
}

/// `npub1…`
pub fn friend_code(pk: &PublicKey) -> String {
    pk.to_bech32().unwrap_or_else(|_| pk.to_hex())
}

/// Reads a pasted friend code: `npub1…`, `nprofile1…`, `nostr:` links or hex.
pub fn parse_code(input: &str) -> Option<PublicKey> {
    let code: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    PublicKey::parse(&code).ok()
}

/// Four characters from the friend code, shown as `Alex · k3f9`.
pub fn tag(pk: &PublicKey) -> String {
    let code = friend_code(pk);
    code.strip_prefix("npub1").unwrap_or(&code).chars().take(4).collect()
}

/// Up to two letters for the avatar circle.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let letters: String = match words.as_slice() {
        [] => "?".into(),
        [one] => one.chars().take(2).collect(),
        [first, .., last] => first.chars().take(1).chain(last.chars().take(1)).collect(),
    };
    letters.to_uppercase()
}

/// Avatar colour, stable per profile: an index into the UI's avatar palette.
pub fn color_index(pk: &PublicKey, palette_len: usize) -> usize {
    let bytes = pk.to_bytes();
    (u16::from_le_bytes([bytes[0], bytes[1]]) as usize) % palette_len.max(1)
}

/// Proof that the party member with `peer_id` owns this profile: a signed event whose
/// content is the peer id. The peer id belongs to the member's DTLS certificate, which the
/// connection itself verifies, so the proof can't be reused by anyone else.
pub fn party_proof(keys: &Keys, peer_id: &str) -> String {
    EventBuilder::new(Kind::Custom(PROOF_KIND), peer_id).finalize(keys).map(|e| e.as_json()).unwrap_or_default()
}

/// The profile key (hex) a proof shows for `peer_id`, if it is valid.
pub fn check_party_proof(proof: &str, peer_id: &str) -> Option<String> {
    if proof.is_empty() || proof.len() > 2000 {
        return None;
    }
    let event = Event::from_json(proof).ok()?;
    (event.kind == Kind::Custom(PROOF_KIND) && event.content == peer_id && event.verify().is_ok())
        .then(|| event.pubkey.to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_codes() {
        let (keys, created) = load_or_create("");
        assert!(created);
        let (again, created) = load_or_create(&secret_hex(&keys));
        assert!(!created);
        assert_eq!(again.public_key(), keys.public_key());
        // Broken secrets give a new profile rather than an error.
        assert!(load_or_create("nonsense").1);

        let pk = keys.public_key();
        let code = friend_code(&pk);
        assert!(code.starts_with("npub1"));
        assert_eq!(parse_code(&format!("  {code}\n")), Some(pk));
        assert_eq!(parse_code(&format!("nostr:{code}")), Some(pk));
        assert_eq!(parse_code(&pk.to_hex()), Some(pk));
        assert_eq!(parse_code("npub1nope"), None);
        assert_eq!(tag(&pk), code[5..9]);
    }

    #[test]
    fn party_proofs() {
        let keys = Keys::generate();
        let proof = party_proof(&keys, "peer-a");
        assert_eq!(check_party_proof(&proof, "peer-a"), Some(keys.public_key().to_hex()));
        // Bound to the connection's peer id.
        assert_eq!(check_party_proof(&proof, "peer-b"), None);
        // Forged content breaks the signature.
        let forged = proof.replace("peer-a", "peer-b");
        assert_eq!(check_party_proof(&forged, "peer-b"), None);
        assert_eq!(check_party_proof("", "peer-a"), None);
        assert_eq!(check_party_proof("{nonsense", "peer-a"), None);
    }

    #[test]
    fn initials_and_colours() {
        assert_eq!(initials("alex"), "AL");
        assert_eq!(initials("Sam van der Berg"), "SB");
        assert_eq!(initials("  "), "?");
        let pk = Keys::generate().public_key();
        assert!(color_index(&pk, 8) < 8);
        assert_eq!(color_index(&pk, 8), color_index(&pk, 8));
    }
}
