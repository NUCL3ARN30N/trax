//! Profiles and friends: see what friends are listening to, invite them to your party, or
//! ask to join theirs.
//!
//! Like listening parties this needs no trax server. Each profile is a Nostr keypair
//! (`profile`). Friends exchange private messages as gift wraps through public relays
//! (`messages`), and publish what they play as an encrypted replaceable event that only
//! friends can open (`presence`). The friend list itself is plain state (`friends`), driven
//! by a background thread that talks to the relays (`service`).

pub mod friends;
pub mod messages;
pub mod presence;
pub mod profile;
pub mod service;
