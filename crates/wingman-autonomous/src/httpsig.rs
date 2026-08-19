//! HTTP framing + HMAC-SHA256 primitives shared by the listeners.
//!
//! [`header_boundary_and_len`] backs `wingman serve`'s request parser and the
//! `wingman pilot intake slack` receiver; [`hmac_sha256`] + [`to_hex`] back
//! the Slack signature check. Nothing here binds a socket — the two things
//! that do own their own accept loops.
//!
//! This module used to also hold a J3 `POST /goals` receiver. It never had a
//! caller (file-drop intake and `wingman serve` cover that surface) and was
//! removed in the #129 cleanup; the design note is in
//! `docs/AUTONOMOUS-MODE.md`.

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Locate the header/body boundary and parse `Content-Length`. Returns
/// `(body_start_index, content_length)`; `content_length` is 0 when absent.
pub fn header_boundary_and_len(buf: &[u8]) -> Option<(usize, usize)> {
    let (idx, sep) = if let Some(i) = find_subslice(buf, b"\r\n\r\n") {
        (i, 4)
    } else {
        (find_subslice(buf, b"\n\n")?, 2)
    };
    let content_len = parse_content_length(&String::from_utf8_lossy(&buf[..idx]));
    Some((idx + sep, content_len))
}

fn parse_content_length(headers: &str) -> usize {
    for line in headers.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                if let Ok(n) = v.trim().parse::<usize>() {
                    return n;
                }
            }
        }
    }
    0
}

/// HMAC-SHA256 (RFC 2104) over `msg` with `key`, built on `sha2` so we don't
/// pull in the `hmac` crate for one call site.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer.finalize());
    out
}

pub fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_handles_crlf_lf_and_missing_length() {
        let (start, len) =
            header_boundary_and_len(b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        assert_eq!(len, 5);
        assert_eq!(
            &b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello"[start..],
            b"hello"
        );

        let (start, len) = header_boundary_and_len(b"POST / HTTP/1.1\nHost: x\n\nbody").unwrap();
        assert_eq!(len, 0, "absent Content-Length reads as 0");
        assert_eq!(&b"POST / HTTP/1.1\nHost: x\n\nbody"[start..], b"body");

        assert!(header_boundary_and_len(b"POST / HTTP/1.1\r\nHost: x").is_none());
    }

    #[test]
    fn hmac_matches_rfc4231_vector() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?"
        assert_eq!(
            to_hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_hashes_keys_longer_than_the_block() {
        // >64-byte keys take the SHA-256-of-key branch; just assert it is
        // deterministic and differs from a truncated key.
        let long = [0xaa_u8; 80];
        let a = to_hex(&hmac_sha256(&long, b"msg"));
        assert_eq!(a, to_hex(&hmac_sha256(&long, b"msg")));
        assert_ne!(a, to_hex(&hmac_sha256(&long[..64], b"msg")));
    }
}
