// SPDX-FileCopyrightText: 2026 Televido contributors
// SPDX-FileContributor: Claude (Anthropic)
// SPDX-License-Identifier: GPL-3.0-or-later

//! MPEG transport stream packets and PSI/SI section reassembly.

use std::collections::HashMap;

pub const PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
/// Maximum size of a private/SI section (`section_length` is 12 bits).
const MAX_SECTION_SIZE: usize = 4096;

/// Reassembles the sections of the PIDs it was created for.
#[derive(Debug, Default)]
pub struct SectionAssembler {
    pids: Vec<u16>,
    state: HashMap<u16, PidState>,
}

#[derive(Debug, Default)]
struct PidState {
    last_counter: Option<u8>,
    /// Bytes of the section that is currently being collected.
    buffer: Vec<u8>,
}

impl SectionAssembler {
    pub fn new(pids: &[u16]) -> Self {
        Self {
            pids: pids.to_vec(),
            state: HashMap::new(),
        }
    }

    /// Splits `data` (e.g. the payload of an RTP packet) into transport
    /// stream packets and returns all sections completed by them.
    ///
    /// Data that isn't aligned to 188-byte packets is ignored.
    pub fn push_data(&mut self, data: &[u8]) -> Vec<Section> {
        let mut sections = Vec::new();
        for packet in data.as_chunks::<PACKET_SIZE>().0 {
            self.push_packet(packet, &mut sections);
        }
        sections
    }

    fn push_packet(&mut self, packet: &[u8], sections: &mut Vec<Section>) {
        if packet[0] != SYNC_BYTE {
            return;
        }
        let transport_error = packet[1] & 0x80 != 0;
        let payload_start = packet[1] & 0x40 != 0;
        let pid = u16::from_be_bytes([packet[1] & 0x1F, packet[2]]);
        let adaptation_field_control = (packet[3] >> 4) & 0b11;
        let counter = packet[3] & 0x0F;

        if transport_error || !self.pids.contains(&pid) {
            return;
        }
        // 0b01: payload only, 0b11: adaptation field and payload
        if adaptation_field_control & 0b01 == 0 {
            return;
        }

        let mut offset = 4;
        if adaptation_field_control == 0b11 {
            offset += 1 + packet[4] as usize;
        }
        if offset >= PACKET_SIZE {
            return;
        }
        let payload = &packet[offset..];

        let state = self.state.entry(pid).or_default();

        // a duplicate packet may be sent once, see ISO/IEC 13818-1 2.4.3.3
        if state.last_counter == Some(counter) {
            return;
        }
        let continuous = state
            .last_counter
            .is_none_or(|last| last.wrapping_add(1) & 0x0F == counter);
        state.last_counter = Some(counter);
        if !continuous {
            state.buffer.clear();
        }

        if payload_start {
            let pointer = payload[0] as usize;
            let rest = &payload[1..];
            if pointer > rest.len() {
                state.buffer.clear();
                return;
            }
            // the tail of the section started in a previous packet
            if !state.buffer.is_empty() {
                state.buffer.extend_from_slice(&rest[..pointer]);
                state.drain_sections(sections);
                state.buffer.clear();
            }
            state.buffer.extend_from_slice(&rest[pointer..]);
        } else if state.buffer.is_empty() {
            // the start of this section was lost
            return;
        } else {
            state.buffer.extend_from_slice(payload);
        }

        state.drain_sections(sections);
    }
}

impl PidState {
    /// Moves all complete sections out of the buffer.
    fn drain_sections(&mut self, sections: &mut Vec<Section>) {
        loop {
            // stuffing bytes until the end of the packet
            if self.buffer.first() == Some(&0xFF) {
                self.buffer.clear();
                return;
            }
            if self.buffer.len() < 3 {
                return;
            }
            let length = 3 + (((self.buffer[1] & 0x0F) as usize) << 8 | self.buffer[2] as usize);
            if length > MAX_SECTION_SIZE {
                self.buffer.clear();
                return;
            }
            if self.buffer.len() < length {
                return;
            }

            let data: Vec<u8> = self.buffer.drain(..length).collect();
            if let Some(section) = Section::new(data) {
                sections.push(section);
            }
        }
    }
}

/// A complete PSI/SI section with a valid CRC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    data: Vec<u8>,
}

impl Section {
    /// Returns `None` if the section is too short or the CRC doesn't match.
    ///
    /// Only sections with the `section_syntax_indicator` set carry a CRC.
    pub fn new(data: Vec<u8>) -> Option<Self> {
        if data.len() < 3 + 5 + 4 || data[1] & 0x80 == 0 || crc32_mpeg2(&data) != 0 {
            return None;
        }
        Some(Self { data })
    }

    pub fn table_id(&self) -> u8 {
        self.data[0]
    }

    /// The `table_id_extension`, e.g. the service_id of an EIT section.
    pub fn id_extension(&self) -> u16 {
        u16::from_be_bytes([self.data[3], self.data[4]])
    }

    pub fn version(&self) -> u8 {
        (self.data[5] >> 1) & 0x1F
    }

    pub fn current_next(&self) -> bool {
        self.data[5] & 1 != 0
    }

    pub fn section_number(&self) -> u8 {
        self.data[6]
    }

    #[cfg(test)]
    pub fn last_section_number(&self) -> u8 {
        self.data[7]
    }

    /// The whole section including the header and CRC.
    #[cfg(test)]
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// The bytes after the 8-byte long section header, without the CRC.
    pub fn body(&self) -> &[u8] {
        &self.data[8..self.data.len() - 4]
    }
}

/// CRC-32/MPEG-2, the checksum of PSI/SI sections. Running it over a section
/// including its CRC yields 0.
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = (i as u32) << 24;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04C1_1DB7
                } else {
                    crc << 1
                };
                bit += 1;
            }
            table[i] = crc;
            i += 1;
        }
        table
    };

    data.iter().fold(0xFFFF_FFFF, |crc, &byte| {
        (crc << 8) ^ TABLE[((crc >> 24) as u8 ^ byte) as usize]
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Appends the CRC to a section built without one.
    pub fn with_crc(mut data: Vec<u8>) -> Vec<u8> {
        data.extend_from_slice(&crc32_mpeg2(&data).to_be_bytes());
        data
    }

    /// Builds a long section: `table_id`, `id_extension`, `version`,
    /// `section_number`/`last_section_number` and the body.
    pub fn long_section(
        table_id: u8,
        id_extension: u16,
        version: u8,
        section_number: u8,
        last_section_number: u8,
        body: &[u8],
    ) -> Vec<u8> {
        // header after `section_length` (5) + body + CRC (4)
        let section_length = 5 + body.len() + 4;
        let mut data = vec![table_id, 0xF0 | (section_length >> 8) as u8];
        data.push(section_length as u8);
        data.extend_from_slice(&id_extension.to_be_bytes());
        data.push(0xC0 | (version << 1) | 1);
        data.push(section_number);
        data.push(last_section_number);
        data.extend_from_slice(body);
        with_crc(data)
    }

    /// Packs `sections` into transport stream packets of `pid`, starting at
    /// continuity counter `counter`. Sections follow each other directly.
    pub fn packetize(pid: u16, sections: &[Vec<u8>], mut counter: u8) -> Vec<u8> {
        let stream: Vec<u8> = sections.iter().flatten().copied().collect();
        let mut out = Vec::new();
        let mut rest = stream.as_slice();
        let mut first = true;

        while !rest.is_empty() {
            let mut packet = vec![
                SYNC_BYTE,
                (if first { 0x40 } else { 0 }) | (pid >> 8) as u8,
                pid as u8,
                0x10 | counter,
            ];
            if first {
                packet.push(0); // pointer_field
            }
            let size = (PACKET_SIZE - packet.len()).min(rest.len());
            packet.extend_from_slice(&rest[..size]);
            rest = &rest[size..];
            packet.resize(PACKET_SIZE, 0xFF);
            out.extend(packet);

            counter = (counter + 1) & 0x0F;
            first = false;
        }
        out
    }

    #[test]
    fn computes_mpeg2_crc() {
        // a well-known PAT: one program (1 → PID 0x100) in transport stream 1
        let pat = [
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xE1, 0x00,
        ];
        assert_eq!(crc32_mpeg2(&pat), 0xE8F9_5E7D);
        assert_eq!(crc32_mpeg2(&with_crc(pat.to_vec())), 0);
    }

    #[test]
    fn reassembles_sections() {
        let a = long_section(0x4E, 0x1234, 3, 0, 1, &[1; 20]);
        let b = long_section(0x4E, 0x1234, 3, 1, 1, &[2; 20]);

        let sections =
            SectionAssembler::new(&[0x12]).push_data(&packetize(0x12, &[a.clone(), b.clone()], 0));

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].as_bytes(), a);
        assert_eq!(sections[1].as_bytes(), b);
        assert_eq!(sections[0].table_id(), 0x4E);
        assert_eq!(sections[0].id_extension(), 0x1234);
        assert_eq!(sections[0].version(), 3);
        assert_eq!(sections[1].section_number(), 1);
        assert_eq!(sections[1].last_section_number(), 1);
        assert!(sections[0].current_next());
        assert_eq!(sections[1].body(), [2; 20]);
    }

    #[test]
    fn reassembles_sections_spanning_packets() {
        // longer than one packet (183 payload bytes) and than two
        let a = long_section(0x50, 7, 0, 0, 0, &[0xAB; 400]);
        let b = long_section(0x50, 8, 0, 0, 0, &[0xCD; 10]);

        let data = packetize(0x12, &[a.clone(), b.clone()], 5);
        assert!(data.len() > 2 * PACKET_SIZE);

        let sections = SectionAssembler::new(&[0x12]).push_data(&data);
        assert_eq!(
            sections.iter().map(Section::as_bytes).collect::<Vec<_>>(),
            [a.as_slice(), b.as_slice()]
        );
    }

    #[test]
    fn ignores_other_pids_and_corrupt_sections() {
        let mut corrupt = long_section(0x4E, 1, 0, 0, 0, &[9; 8]);
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0xFF;
        let good = long_section(0x4E, 2, 0, 0, 0, &[9; 8]);

        let mut assembler = SectionAssembler::new(&[0x12]);
        assert!(assembler
            .push_data(&packetize(0x13, std::slice::from_ref(&good), 0))
            .is_empty());
        assert!(assembler
            .push_data(&packetize(0x12, &[corrupt], 0))
            .is_empty());
        assert_eq!(assembler.push_data(&packetize(0x12, &[good], 2)).len(), 1);
    }

    #[test]
    fn drops_sections_after_lost_packets() {
        let a = long_section(0x50, 7, 0, 0, 0, &[0xAB; 400]);
        let packets = packetize(0x12, std::slice::from_ref(&a), 0);
        let (first, rest) = packets.split_at(PACKET_SIZE);
        // lose the second packet
        let rest = &rest[PACKET_SIZE..];

        let mut assembler = SectionAssembler::new(&[0x12]);
        assert!(assembler.push_data(first).is_empty());
        assert!(assembler.push_data(rest).is_empty());

        // the assembler recovers with the next section
        let b = long_section(0x4E, 1, 0, 0, 0, &[1; 8]);
        assert_eq!(assembler.push_data(&packetize(0x12, &[b], 4)).len(), 1);
    }

    #[test]
    fn handles_adaptation_fields_and_duplicates() {
        let a = long_section(0x42, 1, 0, 0, 0, &[3; 8]);
        let mut packet = packetize(0x11, std::slice::from_ref(&a), 0);

        // insert a 2-byte adaptation field before the payload
        let payload = packet[4..PACKET_SIZE - 2].to_vec();
        packet[3] = 0x30;
        packet[4] = 1;
        packet[5] = 0;
        packet[6..].copy_from_slice(&payload);

        let mut assembler = SectionAssembler::new(&[0x11]);
        let sections = assembler.push_data(&packet);
        assert_eq!(sections.len(), 1);
        // retransmitted packet with the same counter
        assert!(assembler.push_data(&packet).is_empty());
    }
}
