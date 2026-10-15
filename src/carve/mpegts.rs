//! M2TS / MTS carver (AVCHD camcorder video).
//!
//! Structure: a stream of 192-byte source packets, each a 4-byte
//! `TP_extra_header` (copy permission + arrival timestamp) followed by a
//! 188-byte MPEG transport stream packet that starts with the sync byte
//! `0x47`. The file therefore has `0x47` at offsets `4 + 192 * k`. There is no
//! trailer: the file ends after the last packet whose sync byte is intact.

use super::{Carved, Carver, Input, Signature};
use crate::filetype::Category;

/// AVCHD `.mts` / BDAV `.m2ts` transport streams.
pub struct MpegTs;

const SIGS: &[Signature] = &[Signature::at(4, &[SYNC])];

const SYNC: u8 = 0x47;
const PACKET: usize = 192;
/// Offset of the sync byte inside a source packet.
const SYNC_OFF: usize = 4;
/// Consecutive sync bytes required to accept a candidate.
const MIN_PACKETS: usize = 8;
/// Packets read per block while carving.
const BLOCK_PACKETS: usize = 1024;

impl Carver for MpegTs {
    fn name(&self) -> &'static str {
        "mpegts"
    }

    fn signatures(&self) -> &'static [Signature] {
        SIGS
    }

    fn categories(&self) -> &'static [Category] {
        &[Category::Video]
    }

    fn identify(&self, head: &[u8]) -> Option<&'static str> {
        let mut packets = 0;
        while let Some(&b) = head.get(SYNC_OFF + PACKET * packets) {
            if b != SYNC {
                return None;
            }
            packets += 1;
        }
        (packets >= MIN_PACKETS).then_some("mts")
    }

    fn carve(&self, input: &Input, start: u64, limit: u64) -> Option<Carved> {
        let mut buf = vec![0u8; PACKET * BLOCK_PACKETS];
        let mut pos = start;
        let mut packets = 0u64;
        let truncated = loop {
            let want = usize::try_from(limit.saturating_sub(pos))
                .unwrap_or(usize::MAX)
                .min(buf.len());
            let n = input.read(pos, &mut buf[..want]);
            let (chunks, rest) = buf[..n].as_chunks::<PACKET>();
            let good = chunks.iter().take_while(|c| c[SYNC_OFF] == SYNC).count();
            packets += good as u64;
            if good < chunks.len() {
                break false; // sync byte broken inside this block
            }
            pos += n as u64 - rest.len() as u64;
            if n < buf.len() {
                // Limit or end of data reached; a partial packet may remain.
                break !rest.get(SYNC_OFF).is_some_and(|&b| b != SYNC);
            }
        };
        if packets < MIN_PACKETS as u64 {
            return None;
        }
        let len = packets * PACKET as u64;
        Some(if truncated {
            Carved::truncated(len, "mts")
        } else {
            Carved::structure(len, "mts")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::testutil::{carve_bytes, carve_with_limit};

    fn stream(packets: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..packets {
            let mut p = [0u8; PACKET];
            p[..4].copy_from_slice(&(i as u32).to_be_bytes());
            p[SYNC_OFF] = SYNC;
            p[SYNC_OFF + 1] = 0x1F;
            p[SYNC_OFF + 2] = 0xFF;
            p[SYNC_OFF + 3] = 0x10 | (i & 0xF) as u8;
            v.extend(p);
        }
        v
    }

    #[test]
    fn ends_exactly_at_last_good_packet() {
        let data = stream(3000); // spans several read blocks
        let c = carve_bytes(&MpegTs, &data, 5000).unwrap();
        assert_eq!(c.len, data.len() as u64);
        assert_eq!(c.ext, "mts");
        assert!(!c.truncated);
    }

    #[test]
    fn corrupted_sync_ends_the_file() {
        let mut data = stream(100);
        data[SYNC_OFF + PACKET * 60] = 0x00;
        let c = carve_bytes(&MpegTs, &data, 0).unwrap();
        assert_eq!(c.len, (PACKET * 60) as u64);
        assert!(!c.truncated);
    }

    #[test]
    fn truncated_at_limit() {
        let data = stream(2500);
        let c = carve_with_limit(&MpegTs, &data, (PACKET * 1500) as u64).unwrap();
        assert_eq!(c.len, (PACKET * 1500) as u64);
        assert!(c.truncated);
        // A limit inside a packet only counts whole packets.
        let c = carve_with_limit(&MpegTs, &data, (PACKET * 20 + 50) as u64).unwrap();
        assert_eq!(c.len, (PACKET * 20) as u64);
        assert!(c.truncated);
    }

    #[test]
    fn data_ending_inside_a_packet_is_truncated() {
        let mut data = stream(50);
        data.extend_from_slice(&[0, 0, 0, 0, SYNC, 0x1F]);
        let c = carve_bytes(&MpegTs, &data, 0).unwrap();
        assert_eq!(c.len, (PACKET * 50) as u64);
        assert!(c.truncated);
    }

    #[test]
    fn rejects_too_few_or_broken_syncs() {
        assert!(carve_bytes(&MpegTs, &stream(7), 0).is_none());
        // Only 7 packets and then garbage.
        assert!(carve_bytes(&MpegTs, &stream(7), 4000).is_none());
        let mut data = stream(40);
        data[SYNC_OFF + PACKET * 5] = 0x46;
        assert!(carve_bytes(&MpegTs, &data, 100).is_none());
        assert!(carve_bytes(&MpegTs, &[0u8; 8000], 0).is_none());
    }

    #[test]
    fn accepts_exactly_eight_packets() {
        let data = stream(8);
        let c = carve_bytes(&MpegTs, &data, 0).unwrap();
        assert_eq!(c.len, data.len() as u64);
    }
}
