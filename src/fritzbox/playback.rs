// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Plays FRITZ!Box channels through a [`Relay`].
//!
//! The FRITZ!Box serves one stream per device, so Televido receives it
//! itself and hands it to every consumer:
//!
//! ```text
//! FRITZ!Box ─RTSP→ Relay ─┬→ feeder task → pipe → mpv (fd://0)
//!                         └→ LiveFeed (programme guide of the multiplex)
//! ```
//!
//! The relay holds the tuner, which pauses the programme guide sweep; the
//! [`LiveFeed`] keeps the guide of the playing multiplex current meanwhile.
//! A recording will be one more consumer of the same relay.
//!
//! What ends what:
//!
//! - **mpv exits** (closed by the user, or an error): its exit callback ends
//!   the session, which stops the feed and the relay.
//! - **The relay fails** (e.g. the FRITZ!Box stops sending): a toast tells
//!   why, mpv is terminated and the session ends.
//! - **Another channel, or [`stop`]:** mpv is terminated first (which closes
//!   the pipe), then the relay is stopped. Its TEARDOWN is awaited briefly,
//!   so that the FRITZ!Box accepts the next session.
//!
//! With the setting `fritztv-relay` off, mpv opens the RTSP stream itself as
//! before ([`mpv::play`]); then there is neither a live guide nor recording.

use std::{
    cell::{Cell, RefCell},
    time::Duration,
};

use eyre::WrapErr;
use gettextrs::gettext;
use televido_dvb::{Chunk, LiveFeed, Relay};
use tokio::{
    io::AsyncWriteExt,
    net::unix::pipe,
    sync::broadcast::{self, error::RecvError},
};

use crate::{
    application::TvApplication,
    mpv,
    settings::TvSettings,
    utils::{runtime, show_error, spawn, tokio},
};

/// How long stopping a session waits for the TEARDOWN of the relay.
const TEARDOWN_TIMEOUT: Duration = Duration::from_millis(1500);

/// A channel playing through the relay.
struct Session {
    /// Tells the session apart from a later one of the same channel.
    id: u64,
    name: String,
    url: String,
    relay: Relay,
    feed: LiveFeed,
}

impl Session {
    /// Stops the feed and the relay, without waiting for the TEARDOWN.
    fn stop(&self) {
        tracing::debug!("stopping the session of {} ({})", self.name, self.url);
        self.feed.stop();
        self.relay.stop();
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Session>> = const { RefCell::new(None) };
    static LAST_ID: Cell<u64> = const { Cell::new(0) };
}

/// Serializes [`play`]: a second channel waits until the first one has
/// started instead of competing with it for the tuner.
static PLAY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Plays the FRITZ!Box channel `name` with the stream `url` in mpv,
/// replacing whatever plays.
pub async fn play(name: &str, url: &str) -> eyre::Result<()> {
    let _lock = PLAY_LOCK.lock().await;

    // the FRITZ!Box serves one stream per device, the previous one has to
    // end first; the next playback takes over the tuner
    stop_session().await;

    if !TvSettings::get().fritztv_relay() {
        return mpv::play(name, url, true).await;
    }

    let fritzbox = TvApplication::get().fritzbox();
    let tuner = fritzbox.epg().tuner();
    let relay = tokio({
        let url = url.to_owned();
        async move { Relay::open(&url, tuner).await }
    })
    .await
    .wrap_err_with(|| could_not_play(name))?;

    let started = tokio({
        let relay = relay.clone();
        let url = url.to_owned();
        async move {
            // the tokio side of the pipe needs the runtime
            let (sender, receiver) = pipe::pipe()?;
            let stdin = receiver.into_blocking_fd()?;
            tokio::spawn(feed_player(relay.subscribe(), sender));
            let feed = fritzbox.epg().feed(&url, relay.subscribe());
            Ok::<_, std::io::Error>((stdin, feed))
        }
    })
    .await;
    let (stdin, feed) = match started {
        Ok(started) => started,
        Err(e) => {
            relay.stop();
            return Err(eyre::Report::new(e).wrap_err(could_not_play(name)));
        }
    };

    let id = LAST_ID.with(|last_id| {
        last_id.set(last_id.get() + 1);
        last_id.get()
    });
    let previous = CURRENT.with(|current| {
        current.replace(Some(Session {
            id,
            name: name.to_owned(),
            url: url.to_owned(),
            relay: relay.clone(),
            feed,
        }))
    });
    // can't happen while `play` is serialized, but a forgotten relay would
    // keep the tuner
    if let Some(previous) = previous {
        previous.stop();
    }

    // the relay ends with mpv
    let on_exit = move || {
        end_session(id);
    };
    if let Err(e) = mpv::play_stdin(name, stdin, on_exit).await {
        end_session(id);
        return Err(e);
    }

    spawn(watch(id, name.to_owned(), relay));
    Ok(())
}

/// Stops mpv (whatever it plays) and the relay, and waits (for a moment)
/// until the FRITZ!Box has released the stream.
#[allow(dead_code, reason = "for the recording")]
pub async fn stop() {
    if stop_session().await {
        TvApplication::get().fritzbox().epg().tuner().end_playback();
    }
}

/// Stops the relay on shutdown and waits (for a moment) until its TEARDOWN
/// has reached the FRITZ!Box, so that the tuner is free when Televido has
/// quit. Blocks the main thread.
pub fn shutdown() {
    let Some(session) = CURRENT.with(RefCell::take) else {
        return;
    };
    session.stop();

    let relay = session.relay.clone();
    let torn_down = runtime().block_on(async move {
        tokio::time::timeout(TEARDOWN_TIMEOUT, relay.finished())
            .await
            .is_ok()
    });
    if !torn_down {
        tracing::warn!("the relay of {} did not stop in time", session.url);
    }
}

/// The URL of the channel that plays through the relay.
#[allow(dead_code, reason = "for the recording")]
pub fn current_url() -> Option<String> {
    CURRENT.with(|current| current.borrow().as_ref().map(|session| session.url.clone()))
}

/// Stops mpv and the current session, see [`stop`].
///
/// Returns whether mpv played a FRITZ!Box channel directly and therefore
/// held the tuner, like [`mpv::stop`].
async fn stop_session() -> bool {
    // taken first, so that the exit callback of mpv doesn't end it again
    let session = CURRENT.with(RefCell::take);
    let held_tuner = mpv::stop().await;

    if let Some(session) = session {
        session.stop();
        let relay = session.relay.clone();
        let torn_down = tokio(async move {
            tokio::time::timeout(TEARDOWN_TIMEOUT, relay.finished())
                .await
                .is_ok()
        })
        .await;
        if !torn_down {
            tracing::warn!("the relay of {} did not stop in time", session.url);
        }
    }

    held_tuner
}

/// Ends the session `id` if it is still the current one. Returns whether it
/// was.
fn end_session(id: u64) -> bool {
    let session = CURRENT.with(|current| current.borrow_mut().take_if(|session| session.id == id));
    if let Some(session) = &session {
        session.stop();
    }
    session.is_some()
}

/// Writes the stream into the pipe to mpv until the relay or mpv ends.
async fn feed_player(mut chunks: broadcast::Receiver<Chunk>, mut pipe: pipe::Sender) {
    loop {
        let chunk = match chunks.recv().await {
            Ok(chunk) => chunk,
            // mpv didn't read (e.g. paused with a full cache): the oldest
            // chunks are lost, like overflowing UDP
            Err(RecvError::Lagged(count)) => {
                tracing::debug!("mpv fell behind, {count} chunks dropped");
                continue;
            }
            Err(RecvError::Closed) => break,
        };
        // fails with EPIPE once mpv has exited
        if pipe.write_all(&chunk).await.is_err() {
            break;
        }
    }
}

/// Reports a failure of the relay of session `id` and ends the session.
async fn watch(id: u64, name: String, relay: Relay) {
    let Err(e) = tokio(async move { relay.finished().await }).await else {
        return;
    };

    if end_session(id) {
        mpv::stop().await;
    }

    show_error(eyre::Report::msg(format!("{e:#}")).wrap_err(
        // translators: `{}` is replaced by the channel name
        gettext("“{}” stopped").replace("{}", &name),
    ));
}

fn could_not_play(name: &str) -> String {
    // translators: `{}` is replaced by the channel name
    gettext("Could not play “{}”").replace("{}", name)
}
