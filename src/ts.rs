pub const TS_PACKET_SIZE: usize = 188;
pub const TS_PACKETS_PER_UDP: usize = 7;
pub const UDP_PAYLOAD_SIZE: usize = TS_PACKET_SIZE * TS_PACKETS_PER_UDP; // 1316 bytes

pub struct MpegTsAnalyzer {
    last_cc: [i8; 8192],
    pid_seen: [bool; 8192],
    active_pids: usize,
    cc_errors: u64,
}

impl MpegTsAnalyzer {
    pub fn new() -> Self {
        Self {
            last_cc: [-1; 8192],
            pid_seen: [false; 8192],
            active_pids: 0,
            cc_errors: 0,
        }
    }

    pub fn cc_errors(&self) -> u64 {
        self.cc_errors
    }

    pub fn active_pids(&self) -> usize {
        self.active_pids
    }

    #[inline]
    pub fn inspect_packet(&mut self, pkt: &[u8]) {
        if pkt.len() < TS_PACKET_SIZE || pkt[0] != 0x47 {
            return;
        }
        let pid = (((pkt[1] & 0x1F) as usize) << 8) | (pkt[2] as usize);
        if pid == 0x1FFF {
            return;
        }
        if !self.pid_seen[pid] {
            self.pid_seen[pid] = true;
            self.active_pids += 1;
        }

        let afc = (pkt[3] >> 4) & 0x03;
        let has_payload = afc == 1 || afc == 3;
        let has_adaptation = afc == 2 || afc == 3;
        let cc = (pkt[3] & 0x0F) as i8;

        if has_adaptation && pkt[4] > 0 && (pkt[5] & 0x80) != 0 {
            self.last_cc[pid] = cc;
            return;
        }
        if !has_payload {
            return;
        }

        let prev = self.last_cc[pid];
        if prev >= 0 {
            let expected = (prev + 1) & 0x0F;
            if cc != expected && cc != prev {
                self.cc_errors += 1;
            }
        }
        self.last_cc[pid] = cc;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PcrAnchor {
    pub byte_offset: usize,
    pub time_sec: f64,
}

#[inline]
pub fn try_read_pcr_seconds(pkt: &[u8]) -> Option<f64> {
    if pkt.len() < 12 || pkt[0] != 0x47 {
        return None;
    }
    let afc = (pkt[3] >> 4) & 0x03;
    if afc != 2 && afc != 3 {
        return None;
    }
    let af_len = pkt[4];
    if af_len < 7 || (pkt[5] & 0x10) == 0 {
        return None;
    }

    let b0 = pkt[6] as u64;
    let b1 = pkt[7] as u64;
    let b2 = pkt[8] as u64;
    let b3 = pkt[9] as u64;
    let b4 = pkt[10] as u64;
    let b5 = pkt[11] as u64;

    let pcr_base = (b0 << 25) | (b1 << 17) | (b2 << 9) | (b3 << 1) | ((b4 >> 7) & 0x01);
    let pcr_ext = ((b4 & 0x01) << 8) | b5;
    let ticks_27mhz = (pcr_base * 300) + pcr_ext;
    Some(ticks_27mhz as f64 / 27_000_000.0)
}

pub fn extract_pcr_anchors(data: &[u8], start_offset: usize) -> Vec<PcrAnchor> {
    let mut anchors = Vec::with_capacity(64);
    let mut pcr_pid: Option<u16> = None;
    let mut prev_pcr: Option<f64> = None;
    let mut off = start_offset;

    while off + TS_PACKET_SIZE <= data.len() {
        if data[off] != 0x47 {
            off += 1;
            continue;
        }
        if let Some(mut pcr_sec) = try_read_pcr_seconds(&data[off..off + TS_PACKET_SIZE]) {
            let pid = (((data[off + 1] & 0x1F) as u16) << 8) | (data[off + 2] as u16);
            if pcr_pid.is_none() {
                pcr_pid = Some(pid);
            }
            if pcr_pid == Some(pid) {
                if let Some(prev) = prev_pcr {
                    if pcr_sec < prev && (prev - pcr_sec) > 50_000.0 {
                        pcr_sec += 95_443.71768888889; // 2^33 / 90000 wrap-around
                    }
                }
                if prev_pcr.map_or(true, |prev| pcr_sec >= prev) {
                    anchors.push(PcrAnchor {
                        byte_offset: off,
                        time_sec: pcr_sec,
                    });
                    prev_pcr = Some(pcr_sec);
                }
            }
        }
        off += TS_PACKET_SIZE;
    }
    anchors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pcr_packet(pid: u16, pcr_base: u64, pcr_ext: u64, cc: u8) -> [u8; TS_PACKET_SIZE] {
        let mut p = [0u8; TS_PACKET_SIZE];
        p[0] = 0x47;
        p[1] = ((pid >> 8) & 0x1F) as u8;
        p[2] = (pid & 0xFF) as u8;
        p[3] = 0x30 | (cc & 0x0F); // adaptation + payload
        p[4] = 7; // adaptation_field_length
        p[5] = 0x10; // PCR_flag
        p[6] = ((pcr_base >> 25) & 0xFF) as u8;
        p[7] = ((pcr_base >> 17) & 0xFF) as u8;
        p[8] = ((pcr_base >> 9) & 0xFF) as u8;
        p[9] = ((pcr_base >> 1) & 0xFF) as u8;
        p[10] = (((pcr_base & 0x01) << 7) | ((pcr_ext >> 8) & 0x01)) as u8;
        p[11] = (pcr_ext & 0xFF) as u8;
        p
    }

    #[test]
    fn test_pcr_decode_and_wrap() {
        let p1 = make_pcr_packet(0x100, 90_000 * 10, 150, 0);
        let t1 = try_read_pcr_seconds(&p1).unwrap();
        assert!((t1 - 10.000005555).abs() < 1e-6);

        let p_hi = make_pcr_packet(0x100, (1u64 << 33) - 90_000, 0, 1);
        let p_lo = make_pcr_packet(0x100, 90_000, 0, 2);
        let mut stream = Vec::new();
        stream.extend_from_slice(&p_hi);
        stream.extend_from_slice(&p_lo);
        let anchors = extract_pcr_anchors(&stream, 0);
        assert_eq!(anchors.len(), 2);
        assert!((anchors[1].time_sec - anchors[0].time_sec - 2.0).abs() < 0.01);
    }

    #[test]
    fn test_cc_analyzer() {
        let mut a = MpegTsAnalyzer::new();
        let p0 = make_pcr_packet(0x101, 0, 0, 0);
        let p1 = make_pcr_packet(0x101, 90_000, 0, 1);
        let p1_dup = make_pcr_packet(0x101, 90_000, 0, 1);
        let p3_gap = make_pcr_packet(0x101, 180_000, 0, 3);
        a.inspect_packet(&p0);
        a.inspect_packet(&p1);
        a.inspect_packet(&p1_dup);
        a.inspect_packet(&p3_gap);
        assert_eq!(a.active_pids(), 1);
        assert_eq!(a.cc_errors(), 1);
    }
}
