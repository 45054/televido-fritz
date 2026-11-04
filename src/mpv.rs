// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Video playback with mpv.
//!
//! There is at most one mpv process at a time: playing a new video terminates
//! the previous process first. This matters for FRITZ!Box channels, because
//! the FRITZ!Box only serves one stream per client.
//!
//! mpv is started in one of two ways:
//!
//! - [`play`] lets mpv open the URI itself. For FRITZ!Box channels (only if
//!   the setting `fritztv-relay` is off) mpv speaks RTSP with the FRITZ!Box
//!   and sends the TEARDOWN on exit. In that case this module announces the
//!   playback to the tuner of the programme guide (see
//!   [`crate::fritzbox::Epg`]), which pauses reading the guide meanwhile.
//! - [`play_stdin`] lets mpv read a transport stream from its stdin. Televido
//!   receives the stream itself with a relay, which also holds the tuner,
//!   see [`crate::fritzbox::playback`]. The caller learns about the end of
//!   mpv through a callback.

use std::{cell::RefCell, ffi::OsStr, os::fd::OwnedFd, time::Duration};

use adw::{gio, glib, prelude::*};
use futures_util::future::{select, Either};
use gettextrs::gettext;

use crate::{
    application::TvApplication,
    settings::TvSettings,
    utils::{spawn, tokio},
    window::TvWindow,
};

pub const MPV: &str = "mpv";

/// SIGTERM, lets mpv shut down gracefully.
const SIGTERM: i32 = 15;
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(3);
/// Number of output bytes kept for error messages.
const OUTPUT_TAIL_SIZE: usize = 8 * 1024;
/// mpv exit code when quitting due to a signal or the quit key bindings.
const EXIT_CODE_QUIT: i32 = 4;

/// Needed when mpv opens the RTSP stream of the FRITZ!Box itself: the
/// FRITZ!Box doesn't support RTSP over TCP, and the default receive buffer
/// overflows with HD channels. Meaningless for a pipe (the relay asks for
/// the same receive buffer on its own socket, see `televido_dvb::rtsp`), so
/// they aren't part of the FritzTV arguments in the settings.
const RTSP_ARGUMENTS: [(&str, &str); 2] = [
    ("--rtsp-transport=", "--rtsp-transport=udp"),
    // `-append` keeps other `--demuxer-lavf-o` options of the user
    (
        "buffer_size=",
        "--demuxer-lavf-o-append=buffer_size=4194304",
    ),
];

/// The running mpv process.
struct Current {
    process: gio::Subprocess,
    mode: Mode,
}

/// Where the running mpv process gets its media from.
enum Mode {
    /// mpv opens the URI itself.
    Direct {
        /// Whether it plays a FRITZ!Box channel and therefore holds the
        /// tuner.
        uses_tuner: bool,
    },
    /// mpv reads from its stdin.
    Piped {
        /// Called once mpv has ended, see [`play_stdin`].
        on_exit: Box<dyn FnOnce()>,
    },
}

impl Mode {
    /// Finishes the process that ran in this mode: calls the exit callback
    /// and returns whether the process held the tuner.
    fn finish(self) -> bool {
        match self {
            Mode::Direct { uses_tuner } => uses_tuner,
            Mode::Piped { on_exit } => {
                on_exit();
                false
            }
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
}

pub fn is_installed() -> bool {
    glib::find_program_in_path(MPV).is_some()
}

/// Plays `uri` in a new mpv process, replacing the currently running one.
///
/// `uses_tuner` must be set for FRITZ!Box channels, which mpv then opens
/// itself.
pub async fn play(title: &str, uri: &str, uses_tuner: bool) -> eyre::Result<()> {
    let tuner = TvApplication::get().fritzbox().epg().tuner();

    if uses_tuner {
        // the programme guide needs the tuner, too
        let tuner = tuner.clone();
        tokio(async move { tuner.begin_playback().await }).await;
        stop().await;
    } else if stop().await {
        tuner.end_playback();
    }

    let arguments = if uses_tuner {
        rtsp_arguments(&TvSettings::get().mpv_arguments_fritzbox())
    } else {
        Vec::new()
    };

    match start(title, uri, uses_tuner, &arguments, None) {
        Ok(process) => {
            watch_new(process, title, Mode::Direct { uses_tuner });
            Ok(())
        }
        Err(e) => {
            if uses_tuner {
                tuner.end_playback();
            }
            Err(e)
        }
    }
}

/// Plays the transport stream that Televido writes to `stdin` (the reading
/// end of a pipe) in a new mpv process, replacing the currently running one.
///
/// `on_exit` is called exactly once on the main thread when this process is
/// gone: when it exited on its own (before an error is reported), after
/// [`stop`] has terminated it (also when another video replaces it), or when
/// [`terminate`] sent the signal. It isn't called if mpv couldn't be
/// started; the error is returned instead.
///
/// The caller holds the tuner (the relay), so stopping a FRITZ!Box channel
/// that mpv played directly doesn't end the playback at the tuner.
pub async fn play_stdin(
    title: &str,
    stdin: OwnedFd,
    on_exit: impl FnOnce() + 'static,
) -> eyre::Result<()> {
    stop().await;

    // the pipe is no terminal to read key presses from
    let process = start(title, "fd://0", true, &["--input-terminal=no"], Some(stdin))?;
    watch_new(
        process,
        title,
        Mode::Piped {
            on_exit: Box::new(on_exit),
        },
    );
    Ok(())
}

/// Returns the arguments of [`RTSP_ARGUMENTS`] that `fritzbox_arguments`
/// (from the settings) don't set already.
fn rtsp_arguments(fritzbox_arguments: &[String]) -> Vec<&'static str> {
    RTSP_ARGUMENTS
        .into_iter()
        .filter(|(pattern, _)| {
            !fritzbox_arguments
                .iter()
                .any(|argument| argument.contains(*pattern))
        })
        .map(|(_, argument)| argument)
        .collect()
}

/// Starts mpv with the arguments from the settings (including the FritzTV
/// arguments if `fritzbox` is set), followed by `extra_arguments`.
fn start(
    title: &str,
    uri: &str,
    fritzbox: bool,
    extra_arguments: &[&str],
    stdin: Option<OwnedFd>,
) -> eyre::Result<gio::Subprocess> {
    if !is_installed() {
        eyre::bail!(gettext(
            "mpv is not installed. Televido needs the mpv video player."
        ));
    }

    let settings = TvSettings::get();
    let mut arguments = settings.mpv_arguments();
    // tuned for the FRITZ!Box's stream, they only delay the start of HLS and
    // MP4 videos
    if fritzbox {
        arguments.extend(settings.mpv_arguments_fritzbox());
    }
    let title_argument = format!("--force-media-title={title}");

    let argv: Vec<&OsStr> = [MPV]
        .into_iter()
        .chain(
            arguments
                .iter()
                .map(|argument| argument.trim())
                .filter(|argument| !argument.is_empty()),
        )
        .chain(extra_arguments.iter().copied())
        .chain([title_argument.as_str(), "--", uri])
        .map(OsStr::new)
        .collect();

    tracing::debug!("starting {argv:?}");

    let launcher = gio::SubprocessLauncher::new(
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE,
    );
    if stdin.is_some() {
        launcher.take_stdin_fd(stdin);
    }

    launcher
        .spawn(&argv)
        .map_err(|e| eyre::eyre!(e).wrap_err(gettext("Failed to start mpv")))
}

/// Makes `process` the current one and watches it.
fn watch_new(process: gio::Subprocess, title: &str, mode: Mode) {
    // Like in `play()`: only a process that doesn't take over the tuner
    // releases it. A FRITZ!Box channel continues the playback, and with
    // `play_stdin` the caller's relay holds the tuner.
    let releases_tuner = matches!(mode, Mode::Direct { uses_tuner: false });
    let previous = CURRENT.with(|current| {
        current.replace(Some(Current {
            process: process.clone(),
            mode,
        }))
    });
    // started by a concurrent call while this one waited for `stop()`; its
    // `watch()` sees that it was replaced and leaves the tuner alone
    if let Some(previous) = previous {
        previous.process.send_signal(SIGTERM);
        if previous.mode.finish() && releases_tuner {
            TvApplication::get().fritzbox().epg().tuner().end_playback();
        }
    }
    spawn(watch(process, title.to_owned()));
}

/// Terminates the running mpv process and waits until it has exited.
///
/// Returns whether the terminated process held the FRITZ!Box tuner, i.e.
/// whether it played a FRITZ!Box channel with [`play`]. The exit callback of
/// a process started with [`play_stdin`] is called before this returns.
pub async fn stop() -> bool {
    let Some(Current { process, mode }) = CURRENT.with(|current| current.take()) else {
        return false;
    };

    process.send_signal(SIGTERM);

    let exited = select(
        Box::pin(process.wait_future()),
        Box::pin(glib::timeout_future(TERMINATE_TIMEOUT)),
    )
    .await;

    if let Either::Right(_) = exited {
        tracing::warn!("mpv did not exit after SIGTERM, killing it");
        process.force_exit();
        let _ = process.wait_future().await;
    }

    mode.finish()
}

/// Whether mpv plays a FRITZ!Box channel that it opened itself (see
/// [`play`]) and therefore holds the tuner.
pub fn holds_tuner() -> bool {
    CURRENT.with(|current| {
        matches!(
            current.borrow().as_ref(),
            Some(Current {
                mode: Mode::Direct { uses_tuner: true },
                ..
            })
        )
    })
}

/// Terminates the running mpv process without waiting, e.g. on shutdown.
pub fn terminate() {
    if let Some(current) = CURRENT.with(|current| current.take()) {
        current.process.send_signal(SIGTERM);
        current.mode.finish();
    }
}

fn is_current(process: &gio::Subprocess) -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|current| &current.process == process)
    })
}

/// Keeps the application alive while mpv is running and reports errors.
async fn watch(process: gio::Subprocess, title: String) {
    let _hold = TvApplication::get().hold();

    // Always drain the output, otherwise mpv blocks once the pipe is full.
    let mut tail: Vec<u8> = Vec::new();
    if let Some(stdout) = process.stdout_pipe() {
        loop {
            match stdout
                .read_bytes_future(4096, glib::Priority::DEFAULT)
                .await
            {
                Ok(bytes) if !bytes.is_empty() => {
                    tail.extend_from_slice(&bytes);
                    if tail.len() > OUTPUT_TAIL_SIZE {
                        tail.drain(..tail.len() - OUTPUT_TAIL_SIZE);
                    }
                }
                _ => break,
            }
        }
    }

    let _ = process.wait_future().await;

    // terminated on purpose by `stop()` or `terminate()`
    if !is_current(&process) {
        return;
    }
    let uses_tuner = CURRENT
        .with(|current| current.take())
        .is_some_and(|current| current.mode.finish());
    if uses_tuner {
        TvApplication::get().fritzbox().epg().tuner().end_playback();
    }

    if !process.has_exited() {
        return;
    }
    let status = process.exit_status();
    if status == 0 || status == EXIT_CODE_QUIT {
        return;
    }

    let output = String::from_utf8_lossy(&tail);
    tracing::error!("mpv exited with status {status}:\n{output}");

    let reason = output
        .lines()
        .flat_map(|line| line.split('\r'))
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.starts_with("Exiting..."))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("exit status {status}"));

    if let Some(window) = TvApplication::get()
        .active_window()
        .and_downcast::<TvWindow>()
    {
        window.add_toast(
            adw::Toast::builder()
                .title(
                    // translators: `{channel}` is replaced by the channel name or the title of the show, `{error}` by the error message of mpv
                    gettext("mpv could not play “{channel}”: {error}")
                        .replace("{channel}", &title)
                        .replace("{error}", &reason),
                )
                .use_markup(false)
                .timeout(10)
                .build(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_missing_rtsp_arguments() {
        let arguments = |settings: &[&str]| {
            rtsp_arguments(&settings.iter().map(ToString::to_string).collect::<Vec<_>>())
        };

        assert_eq!(
            arguments(&["--cache=yes"]),
            [
                "--rtsp-transport=udp",
                "--demuxer-lavf-o-append=buffer_size=4194304"
            ]
        );
        assert_eq!(
            arguments(&["--rtsp-transport=udp_multicast"]),
            ["--demuxer-lavf-o-append=buffer_size=4194304"]
        );
        assert_eq!(
            arguments(&[
                "--rtsp-transport=udp",
                "--demuxer-lavf-o=buffer_size=8388608"
            ]),
            [] as [&str; 0]
        );
    }
}
