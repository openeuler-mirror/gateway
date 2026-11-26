//! Offline segment dumper: read a BLP1 segment file and print every
//! request-phase entry's top-level body keys (plus previous_response_id
//! presence) — the diagnostic for "log has no previous_response_id".
//!
//! Usage: cargo run -p boom-promptlog --example dump_segment -- <file.seg>

use std::io::Read;

fn main() {
    let path = std::env::args().nth(1).expect("segment file path");
    let mut buf = Vec::new();
    std::fs::File::open(&path)
        .and_then(|mut f| f.read_to_end(&mut buf))
        .expect("read segment");
    let blk = u32::from_le_bytes(*b"BLK1");

    // Skip the segment header: [BLP1][u16 ver][u16 header_len][header_len bytes]
    let hlen = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;
    let mut pos = 8 + hlen;
    let (mut blocks, mut reqs, mut resps, mut with_prev) = (0, 0, 0, 0);
    while pos + 20 <= buf.len() {
        if u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) != blk {
            break;
        }
        let comp_len =
            u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let plain = zstd::decode_all(&buf[pos + 20..pos + 20 + comp_len]).expect("zstd");
        let mut p = 0usize;
        while p + 4 <= plain.len() {
            let len = u32::from_le_bytes(plain[p..p + 4].try_into().unwrap()) as usize;
            let frame = &plain[p + 4..p + 4 + len];
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(frame) {
                match v.get("phase").and_then(|p| p.as_str()) {
                    Some("request") => {
                        reqs += 1;
                        let body = &v["request"];
                        let keys: Vec<&str> = body
                            .as_object()
                            .map(|o| o.keys().map(|k| k.as_str()).collect())
                            .unwrap_or_default();
                        let has_prev = body
                            .get("previous_response_id")
                            .is_some_and(|x| !x.is_null());
                        if has_prev {
                            with_prev += 1;
                        }
                        let input_len = body
                            .get("input")
                            .map(|i| i.to_string().len())
                            .unwrap_or(0);
                        println!(
                            "req#{reqs} prev_id={} input_len={input_len} keys={:?}",
                            if has_prev { "YES" } else { "-" },
                            keys
                        );
                    }
                    Some("response") => resps += 1,
                    _ => {}
                }
            }
            p += 4 + len;
        }
        blocks += 1;
        pos += 20 + comp_len;
    }
    println!(
                "segment blocks={blocks} blocks={blocks} requests={reqs} responses={resps} with_previous_response_id={with_prev}"
    );
}
