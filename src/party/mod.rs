//! Listening parties: a host plays, guests follow.
//!
//! Peers find each other through public Nostr relays (`signaling`) and then talk directly
//! over WebRTC DataChannels (`transport`). The host is the only writer: guests send requests,
//! the host validates them (`host`) and broadcasts what happened; guests apply that in order
//! and keep their playback in sync (`guest`, `clock`). Messages are defined in `protocol`.

pub mod clock;
pub mod code;
pub mod guest;
pub mod host;
pub mod identity;
pub mod protocol;
pub mod signaling;
pub mod transport;
