// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! The DVB-C core of Televido, independent of GTK.
//!
//! - [`m3u`] reads the channel lists of a FRITZ!Box Cable.
//! - [`rtsp`] is a minimal RTSP client that receives the transport stream of
//!   a channel (SAT>IP, RTP over UDP).
//! - [`ts`], [`si`] and [`text`] parse the transport stream, the service
//!   information tables and their text.
//! - [`epg`] builds the programme guide from the service information and
//!   shares the tuner of the FRITZ!Box with playback.
//! - [`relay`] owns the RTSP session of the channel that plays and hands
//!   its transport stream to several consumers: the player, a
//!   [`Recording`] ([`recorder`]) and a [`LiveFeed`] that reads the
//!   programme guide from the stream.
//!
//! ```text
//! FRITZ!Box ─RTP/UDP→ Relay ─broadcast<Chunk>─┬→ pipe to the player (drops the oldest chunks)
//!                                              ├→ Recording (.ts file)
//!                                              └→ LiveFeed (EpgStore)
//! ```

pub mod epg;
pub mod m3u;
pub mod recorder;
pub mod relay;
pub mod rtsp;
pub mod si;
pub mod text;
pub mod ts;

pub use self::{
    epg::{identity, unix_now, Epg, LiveFeed, NowNext, Tuner},
    m3u::FritzChannel,
    recorder::{Recording, RecordingError},
    relay::{Chunk, Relay, RelayError},
};
