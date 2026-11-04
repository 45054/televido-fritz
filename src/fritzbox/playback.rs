// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Plays and records FRITZ!Box channels through a [`Relay`].
//!
//! The FRITZ!Box serves one stream per device, so Televido receives it
//! itself and hands it to every consumer:
//!
//! ```text
//! FRITZ!Box ─RTSP→ Relay ─┬→ feeder task → pipe → mpv (fd://0)   while playing
//!                         ├→ Recording → .ts file                 while recording
//!                         └→ LiveFeed (programme guide of the multiplex)
//! ```
//!
//! A session is the relay of one channel. The relay holds the tuner, which
//! pauses the programme guide sweep; the [`LiveFeed`] keeps the guide of the
//! channel's multiplex current meanwhile. mpv and a recording are optional
//! consumers of the session: playing the channel that is being recorded
//! attaches mpv to the same relay, and recording the playing channel
//! attaches a recording to it. There is one session at a time.
//!
//! What ends what:
//!
//! - **mpv exits** (closed by the user, or an error): its exit callback
//!   detaches it. Without a recording the session ends, which stops the feed
//!   and the relay; otherwise the recording goes on in the background.
//! - **The recording ends** (stopped by the user, at the planned end, or on
//!   an error): without mpv the session ends, so that the tuner is free
//!   again.
//! - **The relay fails** (e.g. the FRITZ!Box stops sending): a toast tells
//!   why, mpv is terminated, the recording completes its file, and the
//!   session ends.
//! - **Another channel** (playing or recording it): the user is asked first
//!   if a recording would end (see [`confirm_quit`] for quitting). mpv is
//!   terminated (which closes the pipe), then the relay is stopped. Its
//!   TEARDOWN is awaited briefly, so that the FRITZ!Box accepts the next
//!   session.
//!
//! With the setting `fritztv-relay` off, mpv opens the RTSP stream itself as
//! before ([`mpv::play`]); then there is neither a live guide nor recording,
//! unless the channel is being recorded already.

use std::{
    cell::{Cell, RefCell},
    path::Path,
    time::Duration,
};

use adw::{glib, prelude::*};
use eyre::WrapErr;
use gettextrs::gettext;
use televido_dvb::{recorder, unix_now, Chunk, LiveFeed, Recording, Relay};
use tokio::{
    io::AsyncWriteExt,
    net::unix::pipe,
    sync::broadcast::{self, error::RecvError},
};

use crate::{
    application::TvApplication,
    mpv,
    settings::TvSettings,
    utils::{
        format_timestamp_time, runtime, show_error, show_file_toast, show_toast, spawn, tokio,
    },
};

use super::recording::{recordings_directory, TvRecordingState};

/// How long stopping a session waits for the TEARDOWN of the relay.
const TEARDOWN_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long quitting waits for recordings to complete their files.
const FINISH_TIMEOUT: Duration = Duration::from_secs(3);
/// A recording of a show that the programme guide knows ends this long
/// after the planned end, because shows often start and end a few minutes
/// late. A constant until there is a setting for it.
const POST_PADDING_SECONDS: i64 = 5 * 60;
/// How often a recording checks whether its planned end has come. The
/// wall clock is compared, which, unlike a timer, also holds after the
/// computer was suspended.
const END_CHECK_SECONDS: u32 = 15;

/// A channel received through the relay.
struct Session {
    /// Tells the session apart from a later one of the same channel.
    id: u64,
    name: String,
    url: String,
    relay: Relay,
    feed: LiveFeed,
    /// The mpv process that plays the session, see [`attach_player`]; 0 if
    /// mpv doesn't play it.
    player: u64,
    recording: Option<ActiveRecording>,
}

/// The recording of the session.
struct ActiveRecording {
    /// Tells the recording apart from a later one of the same session.
    id: u64,
    recording: Recording,
    /// The show, or the channel name if the programme guide doesn't know
    /// the show.
    title: String,
    /// The planned end in seconds since the Unix epoch, 0 if it runs until
    /// it is stopped.
    until: i64,
}

impl Session {
    /// Stops the recording, the feed and the relay, without waiting for the
    /// TEARDOWN.
    fn stop(&self) {
        tracing::debug!("stopping the session of {} ({})", self.name, self.url);
        if let Some(active) = &self.recording {
            active.recording.stop();
        }
        self.feed.stop();
        self.relay.stop();
    }

    /// Whether neither mpv nor a recording uses the session.
    fn is_idle(&self) -> bool {
        self.player == 0 && self.recording.is_none()
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Session>> = const { RefCell::new(None) };
    static LAST_ID: Cell<u64> = const { Cell::new(0) };
    /// The recordings that haven't completed their files yet, also after
    /// they were stopped, so that quitting can wait for them.
    static UNFINISHED: RefCell<Vec<Recording>> = const { RefCell::new(Vec::new()) };
}

/// Serializes starting and ending sessions: a second channel waits until
/// the first one has started instead of competing with it for the tuner.
static PLAY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Returns a new ID for a session, an mpv process or a recording.
fn next_id() -> u64 {
    LAST_ID.with(|last_id| {
        last_id.set(last_id.get() + 1);
        last_id.get()
    })
}

/// Plays the FRITZ!Box channel `name` with the stream `url` in mpv,
/// replacing whatever plays.
///
/// If the channel is being recorded, mpv joins the recording's relay.
/// Asks before another channel's recording ends.
pub async fn play(name: &str, url: &str) -> eyre::Result<()> {
    // asked before taking the lock, the user may take their time
    if !confirm_replacing(url).await {
        return Ok(());
    }
    let _lock = PLAY_LOCK.lock().await;

    let id = match session_of(url) {
        Some(id) => id,
        None if !TvSettings::get().fritztv_relay() => {
            stop_session(true).await;
            return mpv::play(name, url, true).await;
        }
        None => open_session(name, url, true)
            .await
            .wrap_err_with(|| could_not_play(name))?,
    };
    attach_player(id, name).await
}

/// Records the FRITZ!Box channel `name` with the stream `url`, or stops its
/// recording if it is being recorded.
///
/// If the channel plays through the relay, the recording joins it;
/// otherwise the channel is recorded in the background and mpv only stops
/// if it played a FRITZ!Box channel. Asks before another channel's
/// recording ends.
pub async fn record(name: &str, url: &str) -> eyre::Result<()> {
    // the user clicked “Stop Recording” deliberately, no need to ask
    if TvRecordingState::get().is_recording_url(url) {
        stop_recording();
        return Ok(());
    }

    // the recording takes over the tuner from another channel
    let confirmed = if TvRecordingState::get().is_recording() {
        confirm_replacing(url).await
    } else if plays_other_channel(url) {
        confirm_stop_playback(name).await
    } else {
        true
    };
    if !confirmed {
        return Ok(());
    }
    let _lock = PLAY_LOCK.lock().await;

    let id = match session_of(url) {
        Some(id) => id,
        None => open_session(name, url, false)
            .await
            .wrap_err_with(|| could_not_record(name))?,
    };
    if let Err(e) = start_recording(id, name, url).await {
        // the tuner is free again, unless mpv plays the channel
        spawn(release(id));
        return Err(e.wrap_err(could_not_record(name)));
    }
    Ok(())
}

/// Stops the running recording. The session ends once the file is
/// complete, unless mpv plays it.
pub fn stop_recording() {
    let active = CURRENT.with(|current| {
        current
            .borrow_mut()
            .as_mut()
            .and_then(|session| session.recording.take())
    });
    if let Some(active) = active {
        tracing::info!(
            "stopping the recording {}",
            active.recording.path().display()
        );
        active.recording.stop();
        update_state();
    }
}

/// Asks whether to quit although a recording is running. Returns `true`
/// right away if nothing records; the caller stops the recording if the
/// user agrees.
pub async fn confirm_quit() -> bool {
    confirm_stop_recording(&gettext(
        "Televido records only while it is running. The recording is kept up to this point.",
    ))
    .await
}

/// Stops the relay and the recording on shutdown and waits (for a moment)
/// until the TEARDOWN of the relay has reached the FRITZ!Box, so that the
/// tuner is free when Televido has quit, and until the recordings have
/// completed their files. Blocks the main thread.
pub fn shutdown() {
    let session = CURRENT.with(RefCell::take);
    let recordings = UNFINISHED.with(RefCell::take);
    if session.is_none() && recordings.is_empty() {
        return;
    }

    let relay = session.as_ref().map(|session| {
        session.stop();
        session.relay.clone()
    });
    for recording in &recordings {
        recording.stop();
    }

    let finished = runtime().block_on(async move {
        if let Some(relay) = relay {
            await_teardown(relay).await;
        }
        tokio::time::timeout(FINISH_TIMEOUT, async {
            for recording in &recordings {
                let _ = recording.finished().await;
            }
        })
        .await
        .is_ok()
    });
    if !finished {
        tracing::warn!("a recording did not complete its file in time");
    }
}

/// Whether mpv plays a FRITZ!Box channel other than `url`, or `url`
/// without the relay, which a recording of `url` would stop.
fn plays_other_channel(url: &str) -> bool {
    mpv::holds_tuner()
        || CURRENT.with(|current| {
            current
                .borrow()
                .as_ref()
                .is_some_and(|session| session.player != 0 && session.url != url)
        })
}

/// Returns the session that receives `url`, if its relay is running.
fn session_of(url: &str) -> Option<u64> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .filter(|session| session.url == url && session.relay.is_running())
            .map(|session| session.id)
    })
}

/// Opens the relay of `url` as the current session, after ending the
/// previous one. Returns the ID of the session.
///
/// `stop_any_player`: whether mpv stops whatever it plays, as it is about
/// to play the channel; otherwise only if it played a FRITZ!Box channel
/// (it would hold the tuner).
async fn open_session(name: &str, url: &str, stop_any_player: bool) -> eyre::Result<u64> {
    // the FRITZ!Box serves one stream per device, the previous one has to
    // end first; the next session takes over the tuner
    stop_session(stop_any_player).await;

    let fritzbox = TvApplication::get().fritzbox();
    let tuner = fritzbox.epg().tuner();
    let relay = tokio({
        let url = url.to_owned();
        async move { Relay::open(&url, tuner).await }
    })
    .await?;

    let feed = tokio({
        let relay = relay.clone();
        let url = url.to_owned();
        // the feed needs the runtime
        async move { fritzbox.epg().feed(&url, relay.subscribe()) }
    })
    .await;

    let id = next_id();
    let previous = CURRENT.with(|current| {
        current.replace(Some(Session {
            id,
            name: name.to_owned(),
            url: url.to_owned(),
            relay: relay.clone(),
            feed,
            player: 0,
            recording: None,
        }))
    });
    // can't happen while sessions are serialized, but a forgotten relay
    // would keep the tuner
    if let Some(previous) = previous {
        previous.stop();
        update_state();
    }

    spawn(watch(id, name.to_owned(), relay));
    Ok(id)
}

/// Plays the session `id` in mpv, replacing whatever mpv plays (also an
/// earlier mpv process of the same session).
async fn attach_player(id: u64, name: &str) -> eyre::Result<()> {
    let Some(relay) = CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .filter(|session| session.id == id)
            .map(|session| session.relay.clone())
    }) else {
        // the relay failed meanwhile, which was reported
        return Ok(());
    };

    let stdin = tokio(async move {
        // the tokio side of the pipe needs the runtime
        let (sender, receiver) = pipe::pipe()?;
        let stdin = receiver.into_blocking_fd()?;
        tokio::spawn(feed_player(relay.subscribe(), sender));
        Ok::<_, std::io::Error>(stdin)
    })
    .await;
    let stdin = match stdin {
        Ok(stdin) => stdin,
        Err(e) => {
            spawn(release(id));
            return Err(eyre::Report::new(e).wrap_err(could_not_play(name)));
        }
    };

    // set before mpv starts, so that the exit of the previous process of
    // this session doesn't detach the new one
    let player = next_id();
    let attached = CURRENT.with(|current| {
        match current
            .borrow_mut()
            .as_mut()
            .filter(|session| session.id == id)
        {
            Some(session) => {
                session.player = player;
                true
            }
            None => false,
        }
    });
    if !attached {
        return Ok(());
    }

    if let Err(e) = mpv::play_stdin(name, stdin, move || player_exited(id, player)).await {
        player_exited(id, player);
        return Err(e);
    }
    Ok(())
}

/// Detaches the mpv process `player` from the session `id`, which ends
/// unless it is being recorded. Called when that process has exited.
fn player_exited(id: u64, player: u64) {
    let idle = CURRENT.with(|current| {
        match current
            .borrow_mut()
            .as_mut()
            .filter(|session| session.id == id && session.player == player)
        {
            Some(session) => {
                session.player = 0;
                session.is_idle()
            }
            None => false,
        }
    });
    if idle {
        spawn(release(id));
    }
}

/// Ends the session `id` if it is still the current one and neither mpv
/// nor a recording uses it, and waits (for a moment) for its TEARDOWN.
async fn release(id: u64) {
    let _lock = PLAY_LOCK.lock().await;
    let session = CURRENT.with(|current| {
        current
            .borrow_mut()
            .take_if(|session| session.id == id && session.is_idle())
    });
    if let Some(session) = session {
        tear_down(session).await;
    }
}

/// Stops the current session and waits (for a moment) until the FRITZ!Box
/// has released the stream. mpv is stopped if it plays the session or
/// another FRITZ!Box channel, or with `stop_any_player` whatever it plays.
async fn stop_session(stop_any_player: bool) {
    // taken first, so that the exit callback of mpv doesn't end it again
    let session = CURRENT.with(RefCell::take);
    if session
        .as_ref()
        .is_some_and(|session| session.recording.is_some())
    {
        update_state();
    }

    let stop_player = stop_any_player
        || session.as_ref().is_some_and(|session| session.player != 0)
        || mpv::holds_tuner();
    // if mpv held the tuner itself, the relay of the next session takes it
    // over, and releases it if it fails to open
    if stop_player {
        mpv::stop().await;
    }

    if let Some(session) = session {
        tear_down(session).await;
    }
}

/// Stops `session` and waits (for a moment) for the TEARDOWN of its relay.
async fn tear_down(session: Session) {
    session.stop();
    tokio(await_teardown(session.relay.clone())).await;
}

/// Waits (for a moment) until the stopped `relay` has ended, i.e. its
/// TEARDOWN reached the FRITZ!Box, and warns if it takes too long. Must run
/// on the tokio runtime.
async fn await_teardown(relay: Relay) {
    if tokio::time::timeout(TEARDOWN_TIMEOUT, relay.finished())
        .await
        .is_err()
    {
        tracing::warn!("the relay of {} did not stop in time", relay.url());
    }
}

/// Starts recording the session `id` of the channel `name` with the stream
/// `url` into the recordings folder.
async fn start_recording(id: u64, name: &str, url: &str) -> eyre::Result<()> {
    let Some(relay) = CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .filter(|session| session.id == id && session.recording.is_none())
            .map(|session| session.relay.clone())
    }) else {
        // already recording, or the relay failed meanwhile, which was
        // reported
        return Ok(());
    };

    let now = unix_now();
    let show = TvApplication::get()
        .fritzbox()
        .epg()
        .now_next(url, now)
        .and_then(|now_next| now_next.current)
        .filter(|show| show.end > now);
    let until = show
        .as_ref()
        .map_or(0, |show| show.end + POST_PADDING_SECONDS);
    let show_title = show.map(|show| show.title);
    let path = recordings_directory().join(recorder::file_name(name, now, show_title.as_deref()));

    let receiver = relay.subscribe();
    let recording = tokio(async move { Recording::start(receiver, path).await }).await?;
    UNFINISHED.with(|unfinished| unfinished.borrow_mut().push(recording.clone()));

    let recording_id = next_id();
    let title = show_title.unwrap_or_else(|| name.to_owned());
    let stored = CURRENT.with(|current| {
        match current
            .borrow_mut()
            .as_mut()
            .filter(|session| session.id == id && session.recording.is_none())
        {
            Some(session) => {
                session.recording = Some(ActiveRecording {
                    id: recording_id,
                    recording: recording.clone(),
                    title: title.clone(),
                    until,
                });
                true
            }
            None => false,
        }
    });
    // the relay failed while the file was created; the recording ends
    // with it anyway
    if !stored {
        recording.stop();
    }
    update_state();
    spawn(watch_recording(id, recording_id, recording, title.clone()));
    if !stored {
        return Ok(());
    }

    match format_timestamp_time(until).filter(|_| until > 0) {
        Some(time) => show_toast(
            // translators: `{title}` is replaced by the title of the show, `{time}` by the time when the recording ends, e.g. 21:50
            &gettext("Recording “{title}” until {time}")
                .replace("{title}", &title)
                .replace("{time}", &time),
        ),
        None => show_toast(
            // translators: `{}` is replaced by the channel name or the title of the show
            &gettext("Recording “{}”").replace("{}", &title),
        ),
    }
    if until > 0 {
        spawn(end_recording_at(recording_id, until));
    }
    Ok(())
}

/// Returns the ID of the running recording.
fn current_recording_id() -> Option<u64> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .and_then(|session| session.recording.as_ref())
            .map(|active| active.id)
    })
}

/// Stops the recording `recording_id` at `until` (seconds since the Unix
/// epoch). Returns early once that recording has ended otherwise.
async fn end_recording_at(recording_id: u64, until: i64) {
    loop {
        let remaining = until - unix_now();
        if remaining <= 0 {
            break;
        }
        let seconds = u32::try_from(remaining)
            .unwrap_or(END_CHECK_SECONDS)
            .min(END_CHECK_SECONDS);
        glib::timeout_future_seconds(seconds).await;
        if current_recording_id() != Some(recording_id) {
            return;
        }
    }
    if current_recording_id() == Some(recording_id) {
        tracing::info!("the recorded show has ended");
        stop_recording();
    }
}

/// Waits until the recording `recording_id` of the session `id` has
/// completed its file, reports the result and ends the session unless mpv
/// plays it.
async fn watch_recording(id: u64, recording_id: u64, recording: Recording, title: String) {
    // a recording keeps Televido running until its file is complete, e.g.
    // after the window was closed
    let _hold = TvApplication::get().hold();

    let outcome = tokio({
        let recording = recording.clone();
        async move { recording.finished().await }
    })
    .await;

    UNFINISHED.with(|unfinished| {
        unfinished
            .borrow_mut()
            .retain(|unfinished| unfinished.path() != recording.path())
    });
    // ended on its own, e.g. with the relay or on a write error
    let ended = CURRENT.with(|current| {
        current.borrow_mut().as_mut().is_some_and(|session| {
            session
                .recording
                .take_if(|active| active.id == recording_id)
                .is_some()
        })
    });
    if ended {
        update_state();
    }
    spawn(release(id));

    match outcome {
        Ok(summary) => {
            if summary.lost_chunks > 0 {
                tracing::warn!(
                    "{} chunks are missing in {}",
                    summary.lost_chunks,
                    recording.path().display()
                );
            }
            show_recorded(&title, recording.path(), summary.bytes);
        }
        Err(e) => show_error(eyre::Report::msg(format!("{e:#}")).wrap_err(
            // translators: `{}` is replaced by the channel name or the title of the show
            gettext("The recording of “{}” failed").replace("{}", &title),
        )),
    }
}

fn show_recorded(title: &str, path: &Path, bytes: u64) {
    show_file_toast(
        // translators: `{title}` is replaced by the channel name or the title of the show, `{size}` by the size of the file, e.g. 1.2 GB
        &gettext("Recorded “{title}” ({size})")
            .replace("{title}", title)
            .replace("{size}", &glib::format_size(bytes)),
        path,
    );
}

/// Updates [`TvRecordingState`] from the current session.
fn update_state() {
    let (url, title, until) = CURRENT.with(|current| {
        match current
            .borrow()
            .as_ref()
            .and_then(|session| session.recording.as_ref().map(|active| (session, active)))
        {
            Some((session, active)) => (
                Some(session.url.clone()),
                Some(active.title.clone()),
                active.until,
            ),
            None => (None, None, 0),
        }
    });
    // after the borrow ended, as observers may call into this module
    TvRecordingState::get().set(url.as_deref(), title.as_deref(), until);
}

/// Asks whether to play or record `url` although another channel is being
/// recorded. Returns `true` if nothing else records.
async fn confirm_replacing(url: &str) -> bool {
    let state = TvRecordingState::get();
    if !state.is_recording() || state.is_recording_url(url) {
        return true;
    }
    confirm_stop_recording(&gettext(
        "The FRITZ!Box sends Televido one channel at a time. Watching or recording another channel ends the recording.",
    ))
    .await
}

/// Asks whether to stop the running recording, explaining why with `body`.
/// Returns `true` if the user agrees or nothing records.
async fn confirm_stop_recording(body: &str) -> bool {
    let Some(title) = TvRecordingState::get().title() else {
        return true;
    };
    confirm(
        // translators: `{}` is replaced by the channel name or the title of the show
        &gettext("Stop the recording of “{}”?").replace("{}", &title),
        body,
        &gettext("_Stop Recording"),
    )
    .await
}

/// Asks whether to stop the channel that mpv plays, so that `name` can be
/// recorded.
async fn confirm_stop_playback(name: &str) -> bool {
    confirm(
        &gettext("Stop Playback?"),
        // translators: `{}` is replaced by the channel name
        &gettext("The FRITZ!Box sends Televido one channel at a time. Recording “{}” ends the playback of the current channel.")
            .replace("{}", name),
        &gettext("_Stop Playback"),
    )
    .await
}

/// Asks a question in a dialog with “Cancel” and the destructive response
/// `confirm_label`. Returns whether the user chose the latter.
async fn confirm(heading: &str, body: &str, confirm_label: &str) -> bool {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("cancel", &gettext("_Cancel")), ("confirm", confirm_label)]);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let window = TvApplication::get().window();
    dialog.choose_future(Some(&window)).await == "confirm"
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

    let session = CURRENT.with(|current| current.borrow_mut().take_if(|session| session.id == id));
    if let Some(session) = session {
        // the recording ends with the relay and completes its file
        session.stop();
        if session.recording.is_some() {
            update_state();
        }
        if session.player != 0 {
            mpv::stop().await;
        }
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

fn could_not_record(name: &str) -> String {
    // translators: `{}` is replaced by the channel name
    gettext("Could not record “{}”").replace("{}", name)
}
