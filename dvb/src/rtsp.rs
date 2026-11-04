// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! A minimal RTSP client for SAT>IP streams (RTP over UDP), as provided by
//! the FRITZ!Box. It follows the sequence used by FFmpeg and mpv: `OPTIONS`,
//! `DESCRIBE`, `SETUP` and `PLAY`, and hands out the transport stream
//! packets of the RTP payload. `TEARDOWN` frees the tuner of the FRITZ!Box.
//!
//! The server ends a session that sees no request for the `timeout` of its
//! `Session` header (60 seconds by default, SAT>IP 3.5.11). Short scans
//! don't care, but a long-running session (see [`crate::relay`]) sends
//! [`Control::keepalive`] regularly. As the keepalive waits for the answer
//! of the server, [`Stream::parts`] splits the session into the control
//! connection and the receiver, so that the keepalive doesn't hold up
//! receiving the stream.

use std::{fmt, net::SocketAddr, time::Duration};

use eyre::WrapErr;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpStream, UdpSocket},
    time::timeout,
};
use url::Url;

use crate::ts::PACKET_SIZE;

const DEFAULT_PORT: u16 = 554;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(1);
/// Upper bound for the headers of a response.
const MAX_HEADER_LINES: usize = 64;
const MAX_BODY_SIZE: usize = 64 * 1024;
const USER_AGENT: &str = "Televido";
/// The session timeout if the server doesn't name one (RFC 2326, 12.37).
const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(60);
/// Lower bound of the keepalive interval, against servers that announce a
/// tiny timeout.
const MIN_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);

/// The server answered a request with an error status.
#[derive(Debug)]
pub struct StatusError {
    pub method: &'static str,
    pub code: u16,
    pub reason: String,
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {} {}", self.method, self.code, self.reason)
    }
}

impl std::error::Error for StatusError {}

#[derive(Debug)]
struct Response {
    code: u16,
    reason: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// An established RTSP session that receives a transport stream over UDP.
#[derive(Debug)]
pub struct Stream {
    control: Control,
    receiver: Receiver,
}

/// The RTSP connection of a [`Stream`], which controls the session.
#[derive(Debug)]
pub struct Control {
    reader: BufReader<TcpStream>,
    cseq: u32,
    session: Option<String>,
    /// From the `Session` header of the `SETUP` response.
    session_timeout: Duration,
    play_url: String,
}

/// Receives the RTP packets of a [`Stream`].
#[derive(Debug)]
pub struct Receiver {
    rtp: UdpSocket,
    /// Keeps the RTCP port reserved.
    _rtcp: UdpSocket,
    buffer: Vec<u8>,
}

impl Stream {
    /// Opens `url` and starts playing it.
    pub async fn open(url: &str) -> eyre::Result<Self> {
        let url = Url::parse(url).wrap_err("invalid RTSP URL")?;
        if url.scheme() != "rtsp" {
            eyre::bail!("not an RTSP URL: {url}");
        }
        let host = url
            .host_str()
            .ok_or_else(|| eyre::eyre!("no host in {url}"))?;
        let port = url.port().unwrap_or(DEFAULT_PORT);

        let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
            .await
            .wrap_err("connection timed out")?
            .wrap_err_with(|| format!("failed to connect to {host}:{port}"))?;
        tcp.set_nodelay(true)?;
        let peer = tcp.peer_addr()?;

        let (rtp, rtcp, client_port) = bind_udp_pair(peer).await?;

        let mut stream = Self {
            control: Control {
                reader: BufReader::new(tcp),
                cseq: 0,
                session: None,
                session_timeout: DEFAULT_SESSION_TIMEOUT,
                play_url: url.to_string(),
            },
            receiver: Receiver {
                rtp,
                _rtcp: rtcp,
                buffer: vec![0; 4096],
            },
        };

        if let Err(e) = stream.control.start(&url, client_port).await {
            // free the tuner if the session was already set up
            stream.teardown().await;
            return Err(e);
        }
        Ok(stream)
    }

    /// Returns the next chunk of transport stream data, a multiple of 188
    /// bytes.
    pub async fn recv(&mut self) -> eyre::Result<&[u8]> {
        self.receiver.recv().await
    }

    /// Ends the session, which frees the tuner. Errors are ignored.
    pub async fn teardown(&mut self) {
        self.control.teardown().await;
    }

    /// See [`Control::keepalive`].
    pub async fn keepalive(&mut self) -> eyre::Result<()> {
        self.control.keepalive().await
    }

    /// See [`Control::session_timeout`].
    pub fn session_timeout(&self) -> Duration {
        self.control.session_timeout
    }

    /// Splits the stream into its connection and its receiver, which can be
    /// used at the same time, e.g. to send a keepalive while receiving.
    pub fn parts(&mut self) -> (&mut Control, &mut Receiver) {
        (&mut self.control, &mut self.receiver)
    }
}

impl Control {
    async fn start(&mut self, url: &Url, client_port: u16) -> eyre::Result<()> {
        let url_string = url.to_string();

        // some servers don't implement OPTIONS
        let _ = self.request("OPTIONS", &url_string, &[]).await;

        let describe = self
            .request("DESCRIBE", &url_string, &[("Accept", "application/sdp")])
            .await?;
        ensure_ok("DESCRIBE", &describe)?;

        let base = describe
            .header("Content-Base")
            .or_else(|| describe.header("Content-Location"))
            .and_then(|base| Url::parse(base.trim()).ok())
            .unwrap_or_else(|| url.clone());
        let control = sdp_control(&String::from_utf8_lossy(&describe.body))
            .and_then(|control| resolve_control(&base, &control))
            .unwrap_or_else(|| base.to_string());
        self.play_url = base.to_string();

        let transport = format!(
            "RTP/AVP;unicast;client_port={client_port}-{}",
            client_port + 1
        );
        let setup = self
            .request("SETUP", &control, &[("Transport", &transport)])
            .await?;
        ensure_ok("SETUP", &setup)?;

        let session = setup.header("Session");
        self.session = session.map(|session| session_id(session).to_owned());
        self.session_timeout = session
            .and_then(session_timeout)
            .unwrap_or(DEFAULT_SESSION_TIMEOUT);
        tracing::debug!(
            "RTSP session {:?} (timeout {:?}), transport {:?}",
            self.session,
            self.session_timeout,
            setup.header("Transport")
        );

        let mut play = self.play(&self.play_url.clone()).await?;
        if play.code >= 400 {
            // SAT>IP servers address the stream by its ID
            if let Some(stream_url) = setup
                .header("com.ses.streamID")
                .and_then(|id| resolve_control(&base, &format!("stream={}", id.trim())))
            {
                play = self.play(&stream_url).await?;
                self.play_url = stream_url;
            }
        }
        ensure_ok("PLAY", &play)
    }

    async fn play(&mut self, url: &str) -> eyre::Result<Response> {
        self.request("PLAY", url, &[("Range", "npt=0.000-")]).await
    }

    /// The time after which the server ends the session without a request.
    pub fn session_timeout(&self) -> Duration {
        self.session_timeout
    }

    /// How often [`Control::keepalive`] should be sent: half the session
    /// timeout, which leaves time for a slow answer.
    pub fn keepalive_interval(&self) -> Duration {
        (self.session_timeout / 2).max(MIN_KEEPALIVE_INTERVAL)
    }

    /// Tells the server that the session is still in use.
    ///
    /// SAT>IP (3.5.11) names `OPTIONS` with the `Session` header as the
    /// keepalive request; FFmpeg tries `GET_PARAMETER` first, which not every
    /// SAT>IP server implements.
    pub async fn keepalive(&mut self) -> eyre::Result<()> {
        let url = self.play_url.clone();
        let response = self.request("OPTIONS", &url, &[]).await?;
        ensure_ok("OPTIONS", &response)
    }

    /// Ends the session, which frees the tuner. Errors are ignored.
    async fn teardown(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let _ = timeout(TEARDOWN_TIMEOUT, async {
            let request =
                self.build_request("TEARDOWN", &self.play_url.clone(), &[], Some(&session));
            self.reader.get_mut().write_all(request.as_bytes()).await?;
            self.read_response().await
        })
        .await;
    }

    async fn request(
        &mut self,
        method: &'static str,
        url: &str,
        headers: &[(&str, &str)],
    ) -> eyre::Result<Response> {
        let session = self.session.clone();
        let request = self.build_request(method, url, headers, session.as_deref());
        tracing::trace!("RTSP request:\n{request}");

        timeout(REQUEST_TIMEOUT, async {
            self.reader.get_mut().write_all(request.as_bytes()).await?;
            self.read_response().await
        })
        .await
        .wrap_err_with(|| format!("{method} timed out"))?
        .wrap_err_with(|| format!("{method} failed"))
    }

    fn build_request(
        &mut self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        session: Option<&str>,
    ) -> String {
        self.cseq += 1;
        let mut request = format!(
            "{method} {url} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: {USER_AGENT}\r\n",
            self.cseq
        );
        if let Some(session) = session {
            request.push_str(&format!("Session: {session}\r\n"));
        }
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request
    }

    async fn read_response(&mut self) -> eyre::Result<Response> {
        let mut line = String::new();

        // skip empty lines before the status line
        let status = loop {
            line.clear();
            if self.reader.read_line(&mut line).await? == 0 {
                eyre::bail!("connection closed");
            }
            if !line.trim().is_empty() {
                break line.trim().to_owned();
            }
        };

        let mut head = status;
        for _ in 0..MAX_HEADER_LINES {
            line.clear();
            if self.reader.read_line(&mut line).await? == 0 {
                eyre::bail!("connection closed");
            }
            if line.trim().is_empty() {
                break;
            }
            head.push('\n');
            head.push_str(line.trim_end());
        }

        let mut response = parse_response_head(&head)?;
        tracing::trace!("RTSP response: {response:?}");

        if let Some(length) = response
            .header("Content-Length")
            .and_then(|length| length.trim().parse::<usize>().ok())
        {
            if length > MAX_BODY_SIZE {
                eyre::bail!("response body too large");
            }
            response.body = vec![0; length];
            self.reader.read_exact(&mut response.body).await?;
        }

        Ok(response)
    }
}

impl Receiver {
    /// Returns the next chunk of transport stream data, a multiple of 188
    /// bytes.
    ///
    /// Cancel safe: a packet is either returned or left in the socket.
    pub async fn recv(&mut self) -> eyre::Result<&[u8]> {
        let (start, end) = loop {
            let (length, _) = self.rtp.recv_from(&mut self.buffer).await?;
            if let Some(range) = rtp_payload(&self.buffer[..length]) {
                break range;
            }
        };
        Ok(&self.buffer[start..end])
    }
}

fn ensure_ok(method: &'static str, response: &Response) -> eyre::Result<()> {
    if (200..300).contains(&response.code) {
        Ok(())
    } else {
        Err(StatusError {
            method,
            code: response.code,
            reason: response.reason.clone(),
        }
        .into())
    }
}

/// Binds an even UDP port for RTP and the following port for RTCP.
async fn bind_udp_pair(peer: SocketAddr) -> eyre::Result<(UdpSocket, UdpSocket, u16)> {
    let any = if peer.is_ipv4() { "0.0.0.0" } else { "::" };

    for _ in 0..32 {
        let rtp = UdpSocket::bind((any, 0)).await?;
        let port = rtp.local_addr()?.port();
        if port % 2 != 0 || port == u16::MAX {
            continue;
        }
        if let Ok(rtcp) = UdpSocket::bind((any, port + 1)).await {
            return Ok((rtp, rtcp, port));
        }
    }
    eyre::bail!("no free UDP port pair for RTP/RTCP")
}

fn parse_response_head(head: &str) -> eyre::Result<Response> {
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default();

    // `RTSP/1.0 200 OK`
    let mut parts = status.splitn(3, ' ');
    let protocol = parts.next().unwrap_or_default();
    let code = parts.next().and_then(|code| code.parse::<u16>().ok());
    let (true, Some(code)) = (protocol.starts_with("RTSP/"), code) else {
        eyre::bail!("invalid RTSP response: {status:?}");
    };

    Ok(Response {
        code,
        reason: parts.next().unwrap_or_default().trim().to_owned(),
        headers: lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
            .collect(),
        body: Vec::new(),
    })
}

/// Returns the session ID of a `Session` header, e.g. `abc` for
/// `abc;timeout=60`.
fn session_id(header: &str) -> &str {
    header.split(';').next().unwrap_or_default().trim()
}

/// Returns the `timeout` parameter of a `Session` header, e.g. 60 seconds
/// for `abc;timeout=60`.
fn session_timeout(header: &str) -> Option<Duration> {
    header.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        let seconds: u64 = name
            .trim()
            .eq_ignore_ascii_case("timeout")
            .then(|| value.trim().parse().ok())??;
        (seconds > 0).then(|| Duration::from_secs(seconds))
    })
}

/// Returns the `a=control:` attribute of the first media description, or the
/// session-level one.
fn sdp_control(sdp: &str) -> Option<String> {
    let mut session_level = None;
    let mut in_media = false;

    for line in sdp.lines().map(str::trim) {
        if line.starts_with("m=") {
            in_media = true;
        } else if let Some(control) = line.strip_prefix("a=control:") {
            if in_media {
                return Some(control.trim().to_owned());
            }
            session_level.get_or_insert_with(|| control.trim().to_owned());
        }
    }

    session_level
}

/// Resolves a control attribute against the base URL (RFC 2326, C.1.1).
fn resolve_control(base: &Url, control: &str) -> Option<String> {
    if control.is_empty() || control == "*" {
        return Some(base.to_string());
    }
    if control.starts_with("rtsp://") {
        return Some(control.to_owned());
    }
    base.join(control).ok().map(|url| url.to_string())
}

/// Returns the range of the transport stream data within an RTP packet.
fn rtp_payload(packet: &[u8]) -> Option<(usize, usize)> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return None;
    }
    let padding = packet[0] & 0x20 != 0;
    let extension = packet[0] & 0x10 != 0;
    let csrc_count = (packet[0] & 0x0F) as usize;

    let mut start = 12 + 4 * csrc_count;
    if extension {
        let length = u16::from_be_bytes([*packet.get(start + 2)?, *packet.get(start + 3)?]);
        start += 4 + 4 * length as usize;
    }
    let mut end = packet.len();
    if padding {
        end = end.checked_sub(*packet.last()? as usize)?;
    }
    if start >= end || !(end - start).is_multiple_of(PACKET_SIZE) {
        return None;
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_responses() {
        let response = parse_response_head(
            "RTSP/1.0 200 OK\nCSeq: 3\nSession: 0A1B2C;timeout=60\ncom.ses.streamID: 4\nContent-Base: rtsp://192.168.178.1/",
        )
        .unwrap();

        assert_eq!(response.code, 200);
        assert_eq!(response.reason, "OK");
        assert_eq!(response.header("session"), Some("0A1B2C;timeout=60"));
        assert_eq!(response.header("COM.SES.STREAMID"), Some("4"));
        assert_eq!(session_id(response.header("Session").unwrap()), "0A1B2C");
        assert_eq!(
            session_timeout(response.header("Session").unwrap()),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            session_timeout("0A1B2C ; Timeout = 30"),
            Some(Duration::from_secs(30))
        );
        assert_eq!(session_timeout("0A1B2C"), None);
        assert_eq!(session_timeout("0A1B2C;timeout=0"), None);
        assert_eq!(session_timeout("0A1B2C;timeout=abc"), None);

        let busy = parse_response_head("RTSP/1.0 503 Service Unavailable").unwrap();
        assert_eq!(busy.code, 503);
        assert_eq!(busy.reason, "Service Unavailable");

        assert!(parse_response_head("HTTP/1.1 200 OK").is_err());
        assert!(parse_response_head("RTSP/1.0 abc").is_err());
        assert!(parse_response_head("").is_err());
    }

    #[test]
    fn resolves_control_urls() {
        let sdp = "v=0\r\ns=SatIPServer:1 4\r\na=control:*\r\nm=video 0 RTP/AVP 33\r\na=control:stream=4\r\na=sendonly\r\n";
        assert_eq!(sdp_control(sdp).as_deref(), Some("stream=4"));
        assert_eq!(
            sdp_control("v=0\r\na=control:rtsp://h/x\r\nm=video 0 RTP/AVP 33\r\n").as_deref(),
            Some("rtsp://h/x")
        );
        assert_eq!(sdp_control("v=0\r\nm=video 0 RTP/AVP 33\r\n"), None);

        let base = Url::parse("rtsp://192.168.178.1:554/").unwrap();
        assert_eq!(
            resolve_control(&base, "stream=4").as_deref(),
            Some("rtsp://192.168.178.1:554/stream=4")
        );
        assert_eq!(
            resolve_control(&base, "*").as_deref(),
            Some("rtsp://192.168.178.1:554/")
        );
        assert_eq!(
            resolve_control(&base, "rtsp://other/stream=1").as_deref(),
            Some("rtsp://other/stream=1")
        );

        // the request URL with a query is replaced
        let base = Url::parse("rtsp://192.168.178.1:554/?avm=1&freq=450&pids=0").unwrap();
        assert_eq!(
            resolve_control(&base, "stream=2").as_deref(),
            Some("rtsp://192.168.178.1:554/stream=2")
        );
    }

    #[test]
    fn extracts_rtp_payload() {
        let ts = [0x47u8; PACKET_SIZE];

        // plain header
        let mut packet = vec![0x80, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        packet.extend_from_slice(&ts);
        assert_eq!(rtp_payload(&packet), Some((12, 12 + PACKET_SIZE)));

        // CSRC, extension and padding
        let mut packet = vec![0xB2, 33, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        packet.extend_from_slice(&[0; 8]); // two CSRCs
        packet.extend_from_slice(&[0xBE, 0xDE, 0, 1, 1, 2, 3, 4]); // extension, one word
        packet.extend_from_slice(&ts);
        packet.extend_from_slice(&[0, 0, 3]); // padding
        assert_eq!(rtp_payload(&packet), Some((28, 28 + PACKET_SIZE)));

        // wrong version, truncated payload, RTCP-like and empty packets
        assert_eq!(rtp_payload(&[0x40; 200]), None);
        assert_eq!(rtp_payload(&packet[..100]), None);
        assert_eq!(
            rtp_payload(&[0x80, 200, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            None
        );
        assert_eq!(rtp_payload(&[]), None);
    }
}
