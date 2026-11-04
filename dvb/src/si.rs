// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! The DVB tables needed for the programme guide: PAT, SDT and EIT
//! (ISO/IEC 13818-1 and ETSI EN 300 468).

use crate::{text, ts::Section};

pub const PID_PAT: u16 = 0x00;
pub const PID_SDT: u16 = 0x11;
pub const PID_EIT: u16 = 0x12;

const TABLE_PAT: u8 = 0x00;
const TABLE_SDT_ACTUAL: u8 = 0x42;
const TABLE_EIT_PF_ACTUAL: u8 = 0x4E;
const TABLE_EIT_PF_OTHER: u8 = 0x4F;
const TABLE_EIT_SCHEDULE_FIRST: u8 = 0x50;
const TABLE_EIT_SCHEDULE_LAST: u8 = 0x6F;

const DESCRIPTOR_SERVICE: u8 = 0x48;
const DESCRIPTOR_SHORT_EVENT: u8 = 0x4D;
const DESCRIPTOR_EXTENDED_EVENT: u8 = 0x4E;

/// Seconds between 1970-01-01 and the Modified Julian Date epoch offset used
/// by the DVB start times (MJD 40587 is 1970-01-01).
const MJD_UNIX_EPOCH: i64 = 40587;

/// Identifies a service in the whole network.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct ServiceKey {
    pub original_network_id: u16,
    pub transport_stream_id: u16,
    pub service_id: u16,
}

/// Program Association Table: the services of a transport stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pat {
    pub transport_stream_id: u16,
    /// `(service_id, pmt_pid)`
    pub programs: Vec<(u16, u16)>,
}

impl Pat {
    pub fn parse(section: &Section) -> Option<Self> {
        if section.table_id() != TABLE_PAT || !section.current_next() {
            return None;
        }
        let programs = section
            .body()
            .as_chunks::<4>()
            .0
            .iter()
            .filter_map(|entry| {
                let service_id = u16::from_be_bytes([entry[0], entry[1]]);
                let pid = u16::from_be_bytes([entry[2] & 0x1F, entry[3]]);
                // program number 0 refers to the network PID
                (service_id != 0).then_some((service_id, pid))
            })
            .collect();

        Some(Self {
            transport_stream_id: section.id_extension(),
            programs,
        })
    }
}

/// Service Description Table of the current transport stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sdt {
    pub original_network_id: u16,
    pub transport_stream_id: u16,
    /// `(service_id, service_name)`
    pub services: Vec<(u16, String)>,
}

impl Sdt {
    pub fn parse(section: &Section) -> Option<Self> {
        if section.table_id() != TABLE_SDT_ACTUAL || !section.current_next() {
            return None;
        }
        let body = section.body();
        // original_network_id, reserved
        let original_network_id = u16::from_be_bytes([*body.first()?, *body.get(1)?]);
        let mut rest = body.get(3..)?;
        let mut services = Vec::new();

        // service_id (2), flags (1), running_status/free_CA/loop length (2)
        while rest.len() >= 5 {
            let service_id = u16::from_be_bytes([rest[0], rest[1]]);
            let descriptors_length = (((rest[3] & 0x0F) as usize) << 8) | rest[4] as usize;
            let descriptors = rest.get(5..5 + descriptors_length)?;
            rest = &rest[5 + descriptors_length..];

            let name = descriptors_iter(descriptors)
                .find(|(tag, _)| *tag == DESCRIPTOR_SERVICE)
                .and_then(|(_, data)| {
                    // service_type, provider name, service name
                    let provider_length = *data.get(1)? as usize;
                    let name_length = *data.get(2 + provider_length)? as usize;
                    let name = data.get(3 + provider_length..3 + provider_length + name_length)?;
                    Some(text::decode(name))
                })
                .unwrap_or_default();

            services.push((service_id, name));
        }

        Some(Self {
            original_network_id,
            transport_stream_id: section.id_extension(),
            services,
        })
    }
}

/// A section of the Event Information Table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Eit {
    pub table_id: u8,
    pub service: ServiceKey,
    pub version: u8,
    pub section_number: u8,
    pub events: Vec<EitEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EitEvent {
    pub event_id: u16,
    /// Unix timestamp.
    pub start: i64,
    /// Unix timestamp.
    pub end: i64,
    pub title: String,
    pub subtitle: Option<String>,
    pub description: Option<String>,
}

impl Eit {
    /// Returns `true` for the present/following tables (actual and other).
    pub fn is_present_following(&self) -> bool {
        matches!(self.table_id, TABLE_EIT_PF_ACTUAL | TABLE_EIT_PF_OTHER)
    }

    pub fn parse(section: &Section) -> Option<Self> {
        let table_id = section.table_id();
        if !matches!(
            table_id,
            TABLE_EIT_PF_ACTUAL..=TABLE_EIT_PF_OTHER
                | TABLE_EIT_SCHEDULE_FIRST..=TABLE_EIT_SCHEDULE_LAST
        ) || !section.current_next()
        {
            return None;
        }

        let body = section.body();
        // transport_stream_id, original_network_id, segment_last_section_number,
        // last_table_id
        let mut rest = body.get(6..)?;
        let service = ServiceKey {
            original_network_id: u16::from_be_bytes([body[2], body[3]]),
            transport_stream_id: u16::from_be_bytes([body[0], body[1]]),
            service_id: section.id_extension(),
        };

        let mut events = Vec::new();
        // event_id (2), start_time (5), duration (3), loop length (2)
        while rest.len() >= 12 {
            let event_id = u16::from_be_bytes([rest[0], rest[1]]);
            let start = parse_start_time(rest[2..7].try_into().ok()?);
            let duration = parse_duration(rest[7..10].try_into().ok()?);
            let descriptors_length = (((rest[10] & 0x0F) as usize) << 8) | rest[11] as usize;
            let descriptors = rest.get(12..12 + descriptors_length)?;
            rest = &rest[12 + descriptors_length..];

            // events without a start time are placeholders
            let (Some(start), Some(duration)) = (start, duration) else {
                continue;
            };
            let Some(texts) = EventTexts::parse(descriptors) else {
                continue;
            };

            events.push(EitEvent {
                event_id,
                start,
                end: start + duration,
                title: texts.title,
                subtitle: texts.subtitle,
                description: texts.description,
            });
        }

        Some(Self {
            table_id,
            service,
            version: section.version(),
            section_number: section.section_number(),
            events,
        })
    }
}

/// The texts of an event.
struct EventTexts {
    title: String,
    subtitle: Option<String>,
    description: Option<String>,
}

/// Language codes of descriptors that are preferred if an event has the
/// same texts in several languages.
const PREFERRED_LANGUAGES: [&[u8; 3]; 2] = [b"deu", b"ger"];

impl EventTexts {
    /// Returns `None` if the event has no title.
    fn parse(descriptors: &[u8]) -> Option<Self> {
        // (language, name, text)
        let mut short_events: Vec<([u8; 3], &[u8], &[u8])> = Vec::new();
        // (language, descriptor_number, text)
        let mut extended_events: Vec<([u8; 3], u8, &[u8])> = Vec::new();

        for (tag, data) in descriptors_iter(descriptors) {
            match tag {
                DESCRIPTOR_SHORT_EVENT => {
                    let language: [u8; 3] = data.get(..3)?.try_into().ok()?;
                    let name_length = *data.get(3)? as usize;
                    let name = data.get(4..4 + name_length)?;
                    let text_length = *data.get(4 + name_length)? as usize;
                    let text = data.get(5 + name_length..5 + name_length + text_length)?;
                    short_events.push((language, name, text));
                }
                DESCRIPTOR_EXTENDED_EVENT => {
                    let number = data.first()? >> 4;
                    let language: [u8; 3] = data.get(1..4)?.try_into().ok()?;
                    let items_length = *data.get(4)? as usize;
                    let text_start = 5 + items_length;
                    let text_length = *data.get(text_start)? as usize;
                    let text = data.get(text_start + 1..text_start + 1 + text_length)?;
                    extended_events.push((language, number, text));
                }
                _ => (),
            }
        }

        let is_preferred = |language: &[u8; 3]| PREFERRED_LANGUAGES.contains(&language);
        let language = short_events
            .iter()
            .map(|(language, ..)| language)
            .find(|language| is_preferred(language))
            .or_else(|| short_events.first().map(|(language, ..)| language))?;

        let (_, name, short_text) = short_events.iter().find(|(l, ..)| l == language)?;
        let title = text::decode(name);
        if title.is_empty() {
            return None;
        }
        let subtitle = Some(text::decode(short_text)).filter(|text| !text.is_empty());

        // the text of an event may be split over several descriptors, even
        // within a multi-byte character, so it is joined before decoding
        let mut parts: Vec<_> = extended_events
            .iter()
            .filter(|(l, ..)| l == language)
            .collect();
        parts.sort_by_key(|(_, number, _)| *number);
        let bytes: Vec<u8> = parts
            .iter()
            .flat_map(|(_, _, text)| text.iter().copied())
            .collect();
        let description = Some(text::decode(&bytes)).filter(|text| !text.is_empty());

        Some(Self {
            title,
            subtitle,
            description,
        })
    }
}

/// Iterates over `(tag, data)` of a descriptor loop. Stops at malformed data.
fn descriptors_iter(mut data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::from_fn(move || {
        let (&tag, rest) = data.split_first()?;
        let (&length, rest) = rest.split_first()?;
        let descriptor = rest.get(..length as usize)?;
        data = &rest[length as usize..];
        Some((tag, descriptor))
    })
}

fn bcd(byte: u8) -> Option<i64> {
    let (high, low) = (byte >> 4, byte & 0x0F);
    (high <= 9 && low <= 9).then(|| (high * 10 + low) as i64)
}

/// Parses `start_time`: 16 bit Modified Julian Date and 6 BCD digits
/// (hours, minutes, seconds) in UTC. All bits set means undefined.
fn parse_start_time(data: [u8; 5]) -> Option<i64> {
    let mjd = u16::from_be_bytes([data[0], data[1]]) as i64;
    let (hours, minutes, seconds) = (bcd(data[2])?, bcd(data[3])?, bcd(data[4])?);
    if hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }
    Some((mjd - MJD_UNIX_EPOCH) * 86_400 + hours * 3600 + minutes * 60 + seconds)
}

/// Parses `duration`: 6 BCD digits (hours, minutes, seconds).
fn parse_duration(data: [u8; 3]) -> Option<i64> {
    let (hours, minutes, seconds) = (bcd(data[0])?, bcd(data[1])?, bcd(data[2])?);
    if minutes > 59 || seconds > 59 {
        return None;
    }
    Some(hours * 3600 + minutes * 60 + seconds)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::ts::tests::long_section;

    pub fn short_event_descriptor(language: &[u8; 3], name: &[u8], text: &[u8]) -> Vec<u8> {
        let mut data = language.to_vec();
        data.push(name.len() as u8);
        data.extend_from_slice(name);
        data.push(text.len() as u8);
        data.extend_from_slice(text);
        [vec![DESCRIPTOR_SHORT_EVENT, data.len() as u8], data].concat()
    }

    pub fn extended_event_descriptor(
        language: &[u8; 3],
        number: u8,
        last: u8,
        text: &[u8],
    ) -> Vec<u8> {
        let mut data = vec![(number << 4) | last];
        data.extend_from_slice(language);
        data.push(0); // length_of_items
        data.push(text.len() as u8);
        data.extend_from_slice(text);
        [vec![DESCRIPTOR_EXTENDED_EVENT, data.len() as u8], data].concat()
    }

    /// Encodes a unix timestamp (UTC) as DVB `start_time`.
    pub fn start_time(unix: i64) -> [u8; 5] {
        let mjd = (unix.div_euclid(86_400) + MJD_UNIX_EPOCH) as u16;
        let secs = unix.rem_euclid(86_400);
        let bcd = |n: i64| (((n / 10) << 4) | (n % 10)) as u8;
        let [high, low] = mjd.to_be_bytes();
        [
            high,
            low,
            bcd(secs / 3600),
            bcd(secs / 60 % 60),
            bcd(secs % 60),
        ]
    }

    pub fn duration(secs: i64) -> [u8; 3] {
        let bcd = |n: i64| (((n / 10) << 4) | (n % 10)) as u8;
        [bcd(secs / 3600), bcd(secs / 60 % 60), bcd(secs % 60)]
    }

    pub fn event(event_id: u16, start: i64, secs: i64, descriptors: &[u8]) -> Vec<u8> {
        let mut data = event_id.to_be_bytes().to_vec();
        data.extend_from_slice(&start_time(start));
        data.extend_from_slice(&duration(secs));
        data.push(0x80 | (descriptors.len() >> 8) as u8); // running, free_CA
        data.push(descriptors.len() as u8);
        data.extend_from_slice(descriptors);
        data
    }

    pub fn eit_section(
        table_id: u8,
        service: ServiceKey,
        section_number: u8,
        events: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut body = service.transport_stream_id.to_be_bytes().to_vec();
        body.extend_from_slice(&service.original_network_id.to_be_bytes());
        body.extend_from_slice(&[section_number, table_id]);
        body.extend(events.iter().flatten());
        long_section(table_id, service.service_id, 1, section_number, 1, &body)
    }

    pub fn pat_section(transport_stream_id: u16, programs: &[(u16, u16)]) -> Vec<u8> {
        let body: Vec<u8> = programs
            .iter()
            .flat_map(|(service_id, pid)| {
                let [a, b] = service_id.to_be_bytes();
                let [c, d] = (0xE000 | pid).to_be_bytes();
                [a, b, c, d]
            })
            .collect();
        long_section(TABLE_PAT, transport_stream_id, 0, 0, 0, &body)
    }

    pub fn sdt_section(
        original_network_id: u16,
        transport_stream_id: u16,
        services: &[(u16, &[u8])],
    ) -> Vec<u8> {
        let mut body = original_network_id.to_be_bytes().to_vec();
        body.push(0xFF);
        for (service_id, name) in services {
            let provider = b"Prov";
            // service_type: digital television
            let mut data = vec![0x01, provider.len() as u8];
            data.extend_from_slice(provider);
            data.push(name.len() as u8);
            data.extend_from_slice(name);
            let descriptor = [vec![DESCRIPTOR_SERVICE, data.len() as u8], data].concat();

            body.extend_from_slice(&service_id.to_be_bytes());
            body.push(0xFC | 0b11);
            body.push(0x80 | (descriptor.len() >> 8) as u8);
            body.push(descriptor.len() as u8);
            body.extend(descriptor);
        }
        long_section(TABLE_SDT_ACTUAL, transport_stream_id, 0, 0, 0, &body)
    }

    pub fn section(data: Vec<u8>) -> Section {
        Section::new(data).expect("valid section")
    }

    #[test]
    fn parses_start_time_example() {
        // example of EN 300 468 annex C: 1993-10-13 12:45:00
        let unix = parse_start_time([0xC0, 0x79, 0x12, 0x45, 0x00]).unwrap();
        // 1993-10-13T12:45:00Z
        assert_eq!(unix, 750_516_300);
        assert_eq!(start_time(unix), [0xC0, 0x79, 0x12, 0x45, 0x00]);

        // undefined and invalid
        assert_eq!(parse_start_time([0xFF; 5]), None);
        assert_eq!(parse_start_time([0xC0, 0x79, 0x25, 0x00, 0x00]), None);
        assert_eq!(parse_duration([0x01, 0x30, 0x00]), Some(5400));
        assert_eq!(parse_duration([0xFF; 3]), None);
    }

    #[test]
    fn parses_pat() {
        let pat = Pat::parse(&section(pat_section(
            0x0123,
            &[(0, 0x10), (0x4001, 0x100), (0x4002, 0x200)],
        )))
        .unwrap();

        assert_eq!(pat.transport_stream_id, 0x0123);
        // the network PID entry is ignored
        assert_eq!(pat.programs, [(0x4001, 0x100), (0x4002, 0x200)]);
    }

    #[test]
    fn parses_sdt() {
        let sdt = Sdt::parse(&section(sdt_section(
            0x0085,
            0x0123,
            &[(0x4001, b"Das Erste HD"), (0x4002, b"\x86ZDF\x87\xC8")],
        )))
        .unwrap();

        assert_eq!(sdt.original_network_id, 0x0085);
        assert_eq!(sdt.transport_stream_id, 0x0123);
        assert_eq!(
            sdt.services,
            [
                (0x4001, "Das Erste HD".to_owned()),
                (0x4002, "ZDF".to_owned())
            ]
        );
    }

    #[test]
    fn parses_eit_with_texts() {
        let service = ServiceKey {
            original_network_id: 1,
            transport_stream_id: 2,
            service_id: 3,
        };
        let start = 1_700_000_000;

        let descriptors = [
            short_event_descriptor(b"eng", b"News", b"Headlines"),
            short_event_descriptor(b"deu", b"Nachrichten", b"\xC8Uberblick"),
            // the UTF-8 character “ü” is split over two descriptors
            extended_event_descriptor(b"deu", 0, 1, b"\x15Gr\xC3"),
            extended_event_descriptor(b"deu", 1, 1, b"\xBCnde und mehr"),
        ]
        .concat();
        let eit = Eit::parse(&section(eit_section(
            0x4E,
            service,
            0,
            &[event(77, start, 1800, &descriptors)],
        )))
        .unwrap();

        assert_eq!(eit.service, service);
        assert!(eit.is_present_following());
        assert_eq!(eit.section_number, 0);
        assert_eq!(eit.events.len(), 1);
        assert_eq!(eit.events[0].event_id, 77);
        assert_eq!(eit.events[0].start, start);
        assert_eq!(eit.events[0].end, start + 1800);
        assert_eq!(eit.events[0].title, "Nachrichten");
        assert_eq!(eit.events[0].subtitle.as_deref(), Some("Überblick"));
        assert_eq!(
            eit.events[0].description.as_deref(),
            Some("Gründe und mehr")
        );
    }

    #[test]
    fn parses_eit_utf8_description_and_falls_back_to_first_language() {
        let service = ServiceKey::default();
        let descriptors = [
            short_event_descriptor(b"fra", b"Titre", b""),
            extended_event_descriptor(b"fra", 0, 0, b"\x15Caf\xC3\xA9"),
        ]
        .concat();
        let eit = Eit::parse(&section(eit_section(
            0x50,
            service,
            4,
            &[event(1, 1_700_000_000, 60, &descriptors)],
        )))
        .unwrap();

        assert!(!eit.is_present_following());
        assert_eq!(eit.events[0].title, "Titre");
        assert_eq!(eit.events[0].subtitle, None);
        assert_eq!(eit.events[0].description.as_deref(), Some("Café"));
    }

    #[test]
    fn skips_events_without_title_or_start_time() {
        let service = ServiceKey::default();
        let with_title = short_event_descriptor(b"deu", b"Film", b"");
        let mut undefined_start = event(1, 1_700_000_000, 60, &with_title);
        undefined_start[2..7].copy_from_slice(&[0xFF; 5]);

        let eit = Eit::parse(&section(eit_section(
            0x4E,
            service,
            1,
            &[
                event(2, 1_700_000_000, 60, &[]),
                undefined_start,
                event(3, 1_700_000_100, 60, &with_title),
            ],
        )))
        .unwrap();

        assert_eq!(eit.events.len(), 1);
        assert_eq!(eit.events[0].event_id, 3);
    }

    #[test]
    fn ignores_other_tables() {
        let sdt_other = long_section(0x46, 1, 0, 0, 0, &[0, 1, 0xFF]);
        assert!(Sdt::parse(&section(sdt_other)).is_none());
        assert!(Eit::parse(&section(pat_section(1, &[(1, 0x100)]))).is_none());
    }
}
