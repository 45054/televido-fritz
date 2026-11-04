// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! Records the transport stream of a [`Relay`](crate::Relay) to a file.
//!
//! A [`Recording`] is one more consumer of the relay: it writes every
//! [`Chunk`] unchanged, so the file is a plain MPEG transport stream (`.ts`)
//! that every player can open, with the programme guide tables included. It
//! never holds up the relay; if the disk can't keep up for longer than the
//! relay buffers, chunks are lost and the gap is logged.
//!
//! A recording ends with the relay, on [`Recording::stop`] or on a write
//! error. Dropping the handle doesn't stop it.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use eyre::WrapErr;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
    sync::{broadcast, broadcast::error::RecvError, watch},
    time::{interval_at, Instant, MissedTickBehavior},
};

use crate::{unix_now, Chunk};

/// The written data reaches the file at least this often, so that a crash
/// loses little.
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const BUFFER_SIZE: usize = 1024 * 1024;
/// Upper bound for the length of a file name without the extension, in
/// bytes. File systems allow 255 bytes, which leaves room for ` (2)`.
const MAX_NAME_LENGTH: usize = 150;
const EXTENSION: &str = "ts";

/// Why a recording ended with an error. Cheap to clone, as every handle can
/// ask.
pub type RecordingError = Arc<eyre::Report>;

/// A running or finished recording. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Recording {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    path: PathBuf,
    started_at: i64,
    bytes: AtomicU64,
    /// Number of chunks lost because writing fell behind.
    lost_chunks: AtomicU64,
    stop: watch::Sender<bool>,
    /// `None` while recording.
    outcome: watch::Sender<Option<Result<Summary, RecordingError>>>,
}

/// What a finished recording wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The size of the file.
    pub bytes: u64,
    /// How long the recording ran.
    pub duration: Duration,
    /// Chunks that were lost because writing fell behind.
    pub lost_chunks: u64,
}

impl Recording {
    /// Starts writing the chunks of `receiver` to `path`.
    ///
    /// Creates the directory if needed. If the file exists, a free name is
    /// chosen with [`unique_path`]; [`Recording::path`] returns the actual
    /// one. Must be called within the tokio runtime.
    pub async fn start(receiver: broadcast::Receiver<Chunk>, path: PathBuf) -> eyre::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent)
                .await
                .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
        }

        let (file, path) = create_unique(&path).await?;
        tracing::info!("recording to {}", path.display());

        let inner = Arc::new(Inner {
            path,
            started_at: unix_now(),
            bytes: AtomicU64::new(0),
            lost_chunks: AtomicU64::new(0),
            stop: watch::Sender::new(false),
            outcome: watch::Sender::new(None),
        });
        tokio::spawn(run(inner.clone(), receiver, file));

        Ok(Self { inner })
    }

    /// Stops recording. Returns immediately; [`Recording::finished`]
    /// resolves when the file is complete.
    pub fn stop(&self) {
        self.inner.stop.send_replace(true);
    }

    /// Resolves when the recording has ended and the file is complete.
    pub async fn finished(&self) -> Result<Summary, RecordingError> {
        let mut outcome = self.inner.outcome.subscribe();
        let outcome = outcome
            .wait_for(Option::is_some)
            .await
            .expect("the sender lives in the handle");
        outcome.clone().expect("checked by wait_for")
    }

    /// Returns `false` once the recording has ended and the file is
    /// complete.
    pub fn is_running(&self) -> bool {
        self.inner.outcome.borrow().is_none()
    }

    /// The file the recording is written to.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// When the recording started, in seconds since the Unix epoch.
    pub fn started_at(&self) -> i64 {
        self.inner.started_at
    }

    /// The bytes written so far (some may still be buffered).
    pub fn bytes_written(&self) -> u64 {
        self.inner.bytes.load(Ordering::Relaxed)
    }

    /// Chunks lost so far because writing fell behind.
    pub fn lost_chunks(&self) -> u64 {
        self.inner.lost_chunks.load(Ordering::Relaxed)
    }
}

/// Creates the file at `path`, or at the next free name if it exists.
async fn create_unique(path: &Path) -> eyre::Result<(File, PathBuf)> {
    // another program may create the file between the check and creating it
    for _ in 0..8 {
        let path = unique_path(path);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).wrap_err_with(|| format!("failed to create {}", path.display()))
            }
        }
    }
    eyre::bail!("failed to find a free file name for {}", path.display())
}

/// Resolves when `stop` is set.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stop| *stop).await;
}

async fn run(inner: Arc<Inner>, mut receiver: broadcast::Receiver<Chunk>, file: File) {
    let start = Instant::now();
    let mut stop = inner.stop.subscribe();
    let mut writer = BufWriter::with_capacity(BUFFER_SIZE, file);
    let mut flush = interval_at(start + FLUSH_INTERVAL, FLUSH_INTERVAL);
    flush.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let result: io::Result<()> = async {
        loop {
            tokio::select! {
                biased;
                () = stopped(&mut stop) => break,
                received = receiver.recv() => match received {
                    Ok(chunk) => {
                        writer.write_all(&chunk).await?;
                        inner.bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    }
                    Err(RecvError::Lagged(lost)) => {
                        tracing::warn!(
                            "recording has a gap: {lost} chunks lost in {}",
                            inner.path.display()
                        );
                        inner.lost_chunks.fetch_add(lost, Ordering::Relaxed);
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = flush.tick() => writer.flush().await?,
            }
        }
        writer.flush().await?;
        writer.get_ref().sync_data().await
    }
    .await;

    let summary = Summary {
        bytes: inner.bytes.load(Ordering::Relaxed),
        duration: start.elapsed(),
        lost_chunks: inner.lost_chunks.load(Ordering::Relaxed),
    };
    let outcome = match result {
        Ok(()) => {
            tracing::info!("recorded {}: {summary:?}", inner.path.display());
            Ok(summary)
        }
        Err(e) => {
            let e = eyre::Report::new(e).wrap_err(format!(
                "failed to write the recording {}",
                inner.path.display()
            ));
            tracing::warn!("{e:#}");
            Err(Arc::new(e))
        }
    };
    inner.outcome.send_replace(Some(outcome));
}

/// Returns `path` if no file exists there, and otherwise the first free one
/// of `name (2).ts`, `name (3).ts`, …
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_owned();
    }
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();

    (2..)
        .map(|number| path.with_file_name(format!("{stem} ({number}){extension}")))
        .find(|candidate| !candidate.exists())
        .expect("a free name exists")
}

/// Returns the file name of a recording, e.g.
/// `Das Erste HD 2026-11-04 20-15 Tagesschau.ts`, with the local time of
/// `start` (seconds since the Unix epoch).
///
/// Characters that file systems reject (`/`, `:` and the like, also for
/// FAT-formatted drives and network shares) are replaced, and the name is
/// shortened to stay well within the limits of file systems.
pub fn file_name(channel: &str, start: i64, title: Option<&str>) -> String {
    let offset = time::OffsetDateTime::from_unix_timestamp(start)
        .ok()
        .and_then(|time| time::UtcOffset::local_offset_at(time).ok())
        .unwrap_or(time::UtcOffset::UTC);
    file_name_with_offset(channel, start, offset, title)
}

/// [`file_name`] with the given offset from UTC instead of the local one.
pub fn file_name_with_offset(
    channel: &str,
    start: i64,
    offset: time::UtcOffset,
    title: Option<&str>,
) -> String {
    let time = time::OffsetDateTime::from_unix_timestamp(start)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .to_offset(offset);
    let mut name = format!(
        "{} {:04}-{:02}-{:02} {:02}-{:02}",
        channel.trim(),
        time.year(),
        u8::from(time.month()),
        time.day(),
        time.hour(),
        time.minute()
    );
    if let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) {
        name.push(' ');
        name.push_str(title);
    }

    let mut name = sanitize(&name);
    if name.len() > MAX_NAME_LENGTH {
        let mut end = MAX_NAME_LENGTH;
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        // the cut may leave a dot, like `sanitize` removes
        name = name.trim_end_matches(['.', ' ']).to_owned();
    }
    format!("{name}.{EXTENSION}")
}

/// Replaces the characters that are invalid in file names on common file
/// systems and collapses whitespace.
fn sanitize(name: &str) -> String {
    let replaced: String = name
        .chars()
        .filter_map(|c| match c {
            '/' | '\\' | ':' | '|' => Some('-'),
            '*' | '?' | '"' | '<' | '>' => None,
            c if c.is_control() => Some(' '),
            c => Some(c),
        })
        .collect();
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    // no hidden files, and Windows drops trailing dots
    collapsed
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::{
        epg::mock::{MockBox, MockMux, MockOptions, MockService},
        relay::Relay,
        ts::PACKET_SIZE,
        Tuner,
    };

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "televido-recorder-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn names_files() {
        // 2026-11-04 19:15:30 UTC
        let start = 1_793_819_730;
        let cet = time::UtcOffset::from_hms(1, 0, 0).unwrap();

        assert_eq!(
            file_name_with_offset("Das Erste HD", start, cet, Some("Tagesschau")),
            "Das Erste HD 2026-11-04 20-15 Tagesschau.ts"
        );
        assert_eq!(
            file_name_with_offset("ZDF HD", start, time::UtcOffset::UTC, None),
            "ZDF HD 2026-11-04 19-15.ts"
        );
        assert_eq!(
            file_name_with_offset("ZDF HD", start, time::UtcOffset::UTC, Some("  ")),
            "ZDF HD 2026-11-04 19-15.ts"
        );
        assert_eq!(
            file_name_with_offset(
                "AC/DC: Live",
                start,
                cet,
                Some("Was?\n \"Wer\" <kommt> 1/2\0...")
            ),
            "AC-DC- Live 2026-11-04 20-15 Was Wer kommt 1-2.ts"
        );
        assert_eq!(
            file_name_with_offset(".hidden", start, cet, None),
            "hidden 2026-11-04 20-15.ts"
        );

        // long titles are shortened at a character boundary
        let name = file_name_with_offset("ARTE HD", start, cet, Some(&"Ü".repeat(200)));
        assert!(name.len() <= MAX_NAME_LENGTH + 3, "{name}");
        assert!(name.starts_with("ARTE HD 2026-11-04 20-15 ÜÜ"));
        assert!(name.ends_with("Ü.ts"));

        // no `..ts` or ` .ts` if the cut comes after a dot or a space
        let prefix = "ARTE HD 2026-11-04 20-15 ";
        let kept = "x".repeat(MAX_NAME_LENGTH - prefix.len() - 1);
        for filler in [". ", ".", " ."] {
            let title = format!("{kept}{filler}Rest");
            assert_eq!(
                file_name_with_offset("ARTE HD", start, cet, Some(&title)),
                format!("{prefix}{kept}.ts"),
                "{title:?}"
            );
        }

        // the local time
        assert!(file_name("ZDF HD", start, None).starts_with("ZDF HD 2026-11-0"));
    }

    #[test]
    fn finds_unique_paths() {
        let dir = temp_dir("unique");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Show.ts");
        assert_eq!(unique_path(&path), path);

        std::fs::write(&path, b"").unwrap();
        assert_eq!(unique_path(&path), dir.join("Show (2).ts"));
        std::fs::write(dir.join("Show (2).ts"), b"").unwrap();
        assert_eq!(unique_path(&path), dir.join("Show (3).ts"));

        let path = dir.join("no extension");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(unique_path(&path), dir.join("no extension (2)"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn writes_the_received_chunks() {
        let dir = temp_dir("writes");
        let (sender, receiver) = broadcast::channel(64);

        let recording = Recording::start(receiver, dir.join("sub/Show.ts"))
            .await
            .unwrap();
        assert!(recording.is_running());
        assert_eq!(recording.path(), dir.join("sub/Show.ts"));
        assert!((recording.started_at() - unix_now()).abs() <= 1);

        let mut expected = Vec::new();
        for i in 0..100u8 {
            let chunk: Chunk = vec![i; 3 * PACKET_SIZE].into();
            expected.extend_from_slice(&chunk);
            sender.send(chunk).unwrap();
            if i % 10 == 0 {
                tokio::task::yield_now().await;
            }
        }
        drop(sender);

        let summary = recording.finished().await.unwrap();
        assert_eq!(summary.bytes, expected.len() as u64);
        assert_eq!(summary.lost_chunks, 0);
        assert_eq!(recording.bytes_written(), expected.len() as u64);
        assert!(!recording.is_running());
        assert_eq!(std::fs::read(recording.path()).unwrap(), expected);

        // a second recording doesn't overwrite the first
        let (sender, receiver) = broadcast::channel(4);
        let second = Recording::start(receiver, dir.join("sub/Show.ts"))
            .await
            .unwrap();
        assert_eq!(second.path(), dir.join("sub/Show (2).ts"));
        sender.send(vec![0x47; PACKET_SIZE].into()).unwrap();
        tokio::task::yield_now().await;
        second.stop();
        assert_eq!(second.finished().await.unwrap().bytes, PACKET_SIZE as u64);
        drop(sender);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn records_a_relay() {
        let dir = temp_dir("relay");
        let server = MockBox::start_with(
            HashMap::from([(
                450,
                MockMux {
                    original_network_id: 1,
                    transport_stream_id: 2,
                    services: vec![MockService {
                        service_id: 0x4001,
                        pmt_pid: 0x100,
                        name: "Das Erste HD",
                        events: vec![("Tagesschau", unix_now() - 600, 1200)],
                        details: vec![],
                    }],
                    ..Default::default()
                },
            )]),
            MockOptions {
                payload_packets: 21,
                ..Default::default()
            },
        )
        .await;
        let relay = Relay::open(
            &server.url(450, "0,16,17,18,20,256,257"),
            Arc::new(Tuner::default()),
        )
        .await
        .unwrap();

        // a second consumer sees the same chunks
        let mut witness = relay.subscribe();
        let recording = Recording::start(relay.subscribe(), dir.join("Das Erste.ts"))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        relay.stop();

        let mut expected = Vec::new();
        while let Ok(chunk) = witness.recv().await {
            expected.extend_from_slice(&chunk);
        }
        let summary = recording.finished().await.unwrap();
        let data = std::fs::read(recording.path()).unwrap();

        assert!(summary.bytes > 10 * PACKET_SIZE as u64);
        assert_eq!(summary.bytes, data.len() as u64);
        assert_eq!(data, expected);
        assert!(data
            .as_chunks::<PACKET_SIZE>()
            .0
            .iter()
            .all(|packet| packet[0] == 0x47));
        assert!(data.len().is_multiple_of(PACKET_SIZE));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
