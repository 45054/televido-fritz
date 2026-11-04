// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! A fake FRITZ!Box that serves SAT>IP streams with service information, for
//! tests.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, UdpSocket},
};

use crate::{
    si::{
        tests::{
            eit_section, event, extended_event_descriptor, pat_section, sdt_section,
            short_event_descriptor,
        },
        ServiceKey, PID_EIT, PID_PAT, PID_SDT,
    },
    ts::{tests::packetize, PACKET_SIZE},
};

/// A service of a fake multiplex.
#[derive(Clone, Debug)]
pub struct MockService {
    pub service_id: u16,
    pub pmt_pid: u16,
    pub name: &'static str,
    /// `(title, start, seconds)` of the present and following events.
    pub events: Vec<(&'static str, i64, i64)>,
    /// `(subtitle, description)` of the events, in the same order.
    pub details: Vec<(&'static str, &'static str)>,
}

/// How the fake FRITZ!Box reacts to a stream that only requests the service
/// information.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ServiceInformationOnly {
    #[default]
    Streams,
    Rejects,
    SendsNothing,
}

#[derive(Clone, Debug, Default)]
pub struct MockMux {
    pub service_information_only: ServiceInformationOnly,
    pub original_network_id: u16,
    pub transport_stream_id: u16,
    pub services: Vec<MockService>,
    /// Services of other multiplexes announced by `EIT other`.
    pub other: Vec<(ServiceKey, MockService)>,
}

impl MockMux {
    /// The sections to repeat, by PID.
    fn sections(&self) -> Vec<(u16, Vec<Vec<u8>>)> {
        let programs: Vec<_> = self
            .services
            .iter()
            .map(|service| (service.service_id, service.pmt_pid))
            .collect();
        let names: Vec<_> = self
            .services
            .iter()
            .map(|service| (service.service_id, service.name.as_bytes()))
            .collect();

        let mut eit = Vec::new();
        for (table_id, key, service) in self
            .services
            .iter()
            .map(|service| {
                (
                    0x4E,
                    ServiceKey {
                        original_network_id: self.original_network_id,
                        transport_stream_id: self.transport_stream_id,
                        service_id: service.service_id,
                    },
                    service,
                )
            })
            .chain(
                self.other
                    .iter()
                    .map(|(key, service)| (0x4F, *key, service)),
            )
        {
            for (number, (title, start, seconds)) in service.events.iter().enumerate() {
                let (subtitle, description) =
                    service.details.get(number).copied().unwrap_or_default();
                let mut descriptors =
                    short_event_descriptor(b"deu", title.as_bytes(), subtitle.as_bytes());
                if !description.is_empty() {
                    descriptors.extend(extended_event_descriptor(
                        b"deu",
                        0,
                        0,
                        description.as_bytes(),
                    ));
                }
                eit.push(eit_section(
                    table_id,
                    key,
                    number as u8,
                    &[event(number as u16 + 1, *start, *seconds, &descriptors)],
                ));
            }
        }

        vec![
            (
                PID_PAT,
                vec![pat_section(self.transport_stream_id, &programs)],
            ),
            (
                PID_SDT,
                vec![sdt_section(
                    self.original_network_id,
                    self.transport_stream_id,
                    &names,
                )],
            ),
            (PID_EIT, eit),
        ]
    }
}

#[derive(Debug)]
pub struct MockBox {
    pub port: u16,
    /// Number of `SETUP` requests.
    pub setups: Arc<AtomicUsize>,
    /// Number of `TEARDOWN` requests.
    pub teardowns: Arc<AtomicUsize>,
    /// The `pids` requested by the clients.
    pub requested_pids: Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockBox {
    /// Starts a server for `muxes`, by frequency.
    pub async fn start(muxes: HashMap<u32, MockMux>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let setups = Arc::new(AtomicUsize::new(0));
        let teardowns = Arc::new(AtomicUsize::new(0));
        let requested_pids = Arc::new(std::sync::Mutex::new(Vec::new()));

        let state = (
            Arc::new(muxes),
            setups.clone(),
            teardowns.clone(),
            requested_pids.clone(),
        );
        tokio::spawn(async move {
            loop {
                let Ok((socket, peer)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve(socket, peer.ip(), port, state.clone()));
            }
        });

        Self {
            port,
            setups,
            teardowns,
            requested_pids,
        }
    }

    pub fn url(&self, frequency: u32, pids: &str) -> String {
        format!(
            "rtsp://127.0.0.1:{}/?avm=1&freq={frequency}&bw=8&msys=dvbc&mtype=256qam&sr=6900&specinv=1&pids={pids}",
            self.port
        )
    }
}

type State = (
    Arc<HashMap<u32, MockMux>>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<Vec<String>>>,
);

async fn serve(
    socket: tokio::net::TcpStream,
    peer: std::net::IpAddr,
    port: u16,
    (muxes, setups, teardowns, requested_pids): State,
) {
    let (read, mut write) = socket.into_split();
    let mut reader = BufReader::new(read);
    let mut mux: Option<MockMux> = None;
    let mut client_port = 0;
    let mut sender: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        // request line and headers
        let mut request = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                if let Some(sender) = sender {
                    sender.abort();
                }
                return;
            }
            if line.trim().is_empty() {
                break;
            }
            request.push_str(&line);
        }

        let first_line = request.lines().next().unwrap_or_default();
        let method = first_line.split(' ').next().unwrap_or_default().to_owned();
        let url = first_line.split(' ').nth(1).unwrap_or_default().to_owned();
        let cseq = header(&request, "CSeq").unwrap_or_default();

        let (status, headers, body): (&str, String, String) = match method.as_str() {
            "OPTIONS" => (
                "200 OK",
                "Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n".into(),
                String::new(),
            ),
            "DESCRIBE" => {
                let frequency = query(&url, "freq").and_then(|f| f.parse().ok());
                let pids = query(&url, "pids").unwrap_or_default();
                requested_pids.lock().unwrap().push(pids.clone());

                match frequency.and_then(|f| muxes.get(&f)) {
                    Some(found) => {
                        let only_service_information = !pids
                            .split(',')
                            .any(|pid| pid.parse::<u16>().is_ok_and(|pid| pid > 20));
                        if only_service_information
                            && found.service_information_only == ServiceInformationOnly::Rejects
                        {
                            ("400 Bad Request", String::new(), String::new())
                        } else {
                            let mut found = found.clone();
                            if only_service_information
                                && found.service_information_only
                                    == ServiceInformationOnly::SendsNothing
                            {
                                found.services.clear();
                                found.other.clear();
                            }
                            mux = Some(found);
                            let sdp = "v=0\r\ns=SatIPServer:1 1\r\nm=video 0 RTP/AVP 33\r\na=control:stream=1\r\n";
                            (
                                "200 OK",
                                format!(
                                    "Content-Base: rtsp://127.0.0.1:{port}/\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n",
                                    sdp.len()
                                ),
                                sdp.to_owned(),
                            )
                        }
                    }
                    None => ("404 Not Found", String::new(), String::new()),
                }
            }
            "SETUP" => {
                setups.fetch_add(1, Ordering::SeqCst);
                let transport = header(&request, "Transport").unwrap_or_default();
                client_port = transport
                    .split("client_port=")
                    .nth(1)
                    .and_then(|ports| ports.split(['-', ';']).next())
                    .and_then(|port| port.parse().ok())
                    .unwrap_or_default();
                (
                    "200 OK",
                    format!(
                        "Session: 4D2F1A;timeout=30\r\nTransport: RTP/AVP;unicast;client_port={client_port}-{};server_port=6970-6971\r\ncom.ses.streamID: 1\r\n",
                        client_port + 1
                    ),
                    String::new(),
                )
            }
            "PLAY" => {
                // a multiplex without services is silent
                if let Some(mux) = mux.clone().filter(|mux| !mux.services.is_empty()) {
                    sender = Some(tokio::spawn(send_stream(mux, peer, client_port)));
                }
                ("200 OK", "Session: 4D2F1A\r\n".into(), String::new())
            }
            "TEARDOWN" => {
                teardowns.fetch_add(1, Ordering::SeqCst);
                if let Some(sender) = sender.take() {
                    sender.abort();
                }
                ("200 OK", String::new(), String::new())
            }
            _ => ("501 Not Implemented", String::new(), String::new()),
        };

        let response = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n{headers}\r\n{body}");
        if write.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn header(request: &str, name: &str) -> Option<String> {
    request.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

fn query(url: &str, name: &str) -> Option<String> {
    url.split_once('?')?.1.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}

/// Repeats the service information as RTP packets.
async fn send_stream(mux: MockMux, peer: std::net::IpAddr, client_port: u16) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sections = mux.sections();
    let mut counters: HashMap<u16, u8> = HashMap::new();
    let mut sequence = 0u16;

    loop {
        for (pid, sections) in &sections {
            // one section per RTP packet burst, like a real multiplexer
            for section in sections {
                let counter = counters.entry(*pid).or_default();
                let packets = packetize(*pid, std::slice::from_ref(section), *counter);
                *counter = (*counter + (packets.len() / PACKET_SIZE) as u8) & 0x0F;

                for chunk in packets.chunks(7 * PACKET_SIZE) {
                    let mut rtp = vec![0x80, 33];
                    rtp.extend_from_slice(&sequence.to_be_bytes());
                    rtp.extend_from_slice(&[0; 8]);
                    rtp.extend_from_slice(chunk);
                    sequence = sequence.wrapping_add(1);
                    let _ = socket.send_to(&rtp, (peer, client_port)).await;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
