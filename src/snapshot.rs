//! The snapshot codec: JSON parse/format for vm.snapshot() and
//! vm.restore(), plus the in-tree base64. Format version 2 (emitted with
//! sorted keys, so dumps are deterministic):
//! `{"dirs":[...],"files":{path:{"text":s}|{"b64":s}},"version":2}`.
//! Valid UTF-8 content is stored as text unless its JSON-escaped form
//! would be larger than the base64 form (a file of NUL bytes blows up to
//! six escaped characters per byte, for instance). vm.restore also
//! accepts the legacy flat `{"/path": "text"}` map.

use std::collections::{BTreeMap, BTreeSet};

use crate::fs::{normalize, shown, FsResult, MAX_ENTRIES};

/// Parsed snapshot payload: explicit directories plus file contents.
#[derive(Debug)]
pub(crate) struct Snapshot {
    pub(crate) dirs: BTreeSet<String>,
    pub(crate) files: Vec<(String, Vec<u8>)>,
}

/// Records one normalized path; two spellings of the same path in one
/// snapshot would otherwise be planned twice and charged twice while the
/// commit inserts only the last, drifting the usage counter.
fn push_seen(seen: &mut BTreeSet<String>, key: String) -> FsResult<()> {
    if seen.insert(key.clone()) {
        return Ok(());
    }
    Err(format!("duplicate path in snapshot: {}", key))
}

/// Serializes the tree. Path keys and the `dirs` list come out sorted
/// because both containers are B-trees.
pub(crate) fn encode(
    files: &BTreeMap<String, Vec<u8>>,
    dirs: &BTreeSet<String>,
) -> FsResult<String> {
    let mut out = serde_json::Map::new();
    for (p, bytes) in files {
        let entry = match std::str::from_utf8(bytes) {
            Ok(text) if escaped_len(text) <= b64_len(bytes.len()) => {
                serde_json::json!({ "text": text })
            }
            _ => serde_json::json!({ "b64": b64::encode(bytes) }),
        };
        out.insert(p.clone(), entry);
    }
    let dirs: Vec<&str> = dirs.iter().map(String::as_str).collect();
    let v = serde_json::json!({ "version": 2, "files": out, "dirs": dirs });
    // Infallible for values built from strings, but a panic here would
    // cross extern "C" and kill the process, so the error is carried.
    serde_json::to_string(&v).map_err(|_| "snapshot too large".to_string())
}

/// The length serde_json would give this string once escaped: short
/// escapes for quote, backslash and the five one-letter controls,
/// `\\u00xx` for the other C0 controls, everything else verbatim.
fn escaped_len(s: &str) -> usize {
    s.bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\t' | b'\n' | b'\r' | 0x08 | 0x0c => 2,
            b if b < 0x20 => 6,
            _ => 1,
        })
        .sum()
}

fn b64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

pub(crate) fn parse(json: &str) -> FsResult<Snapshot> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid snapshot: {}", e))?;
    let obj = v.as_object().ok_or("invalid snapshot: expected an object")?;
    match obj.get("version") {
        Some(v) if v.as_i64() == Some(2) => parse_v2(obj),
        // The version is guest-controlled JSON of any size and type; cap it
        // like every other echoed value.
        Some(v) => Err(format!("unsupported snapshot version {}", shown(&v.to_string()))),
        None => parse_legacy(obj),
    }
}

fn parse_v2(obj: &serde_json::Map<String, serde_json::Value>) -> FsResult<Snapshot> {
    let files = obj
        .get("files")
        .and_then(|v| v.as_object())
        .ok_or("invalid snapshot: files must be an object")?;
    // Fetch and type-check dirs once; the count gate runs before
    // decoding anything so an over-cap snapshot is refused before the
    // host has normalized or materialized its entries.
    let dirs = match obj.get("dirs") {
        None => None,
        Some(v) => Some(
            v.as_array()
                .ok_or("invalid snapshot: dirs must be an array")?,
        ),
    };
    if files.len() + dirs.map_or(0, |arr| arr.len()) > MAX_ENTRIES {
        return Err(format!("fs entry limit exceeded (max {})", MAX_ENTRIES));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(files.len());
    for (p, entry) in files {
        let key = normalize(p)?;
        push_seen(&mut seen, key.clone())?;
        out.push((key, entry_bytes(entry)?));
    }
    let mut dir_set = BTreeSet::new();
    if let Some(arr) = dirs {
        for d in arr {
            let s = d
                .as_str()
                .ok_or("invalid snapshot: dirs entries must be strings")?;
            let key = normalize(s)?;
            push_seen(&mut seen, key.clone())?;
            dir_set.insert(key);
        }
    }
    Ok(Snapshot {
        dirs: dir_set,
        files: out,
    })
}

fn entry_bytes(entry: &serde_json::Value) -> FsResult<Vec<u8>> {
    let obj = entry
        .as_object()
        .ok_or("invalid snapshot: file entry must be {text} or {b64}")?;
    if let Some(t) = obj.get("text") {
        return t
            .as_str()
            .map(|s| s.as_bytes().to_vec())
            .ok_or("invalid snapshot: text must be a string".to_string());
    }
    if let Some(b) = obj.get("b64") {
        let s = b.as_str().ok_or("invalid snapshot: b64 must be a string")?;
        return b64::decode(s).ok_or("invalid snapshot: bad base64".to_string());
    }
    Err("invalid snapshot: file entry must be {text} or {b64}".to_string())
}

/// The legacy format: a flat map of path to file text. Relative keys are
/// read as root-relative (`"a/b"` means `"/a/b"`), matching what the old
/// flat filesystem tolerated; keys that then escape the root or break the
/// path cap fail with an error that names them.
fn parse_legacy(obj: &serde_json::Map<String, serde_json::Value>) -> FsResult<Snapshot> {
    if obj.len() > MAX_ENTRIES {
        return Err(format!("fs entry limit exceeded (max {})", MAX_ENTRIES));
    }
    let mut seen = BTreeSet::new();
    let mut files = Vec::with_capacity(obj.len());
    for (p, v) in obj {
        let s = v
            .as_str()
            .ok_or("invalid snapshot: old-format values must be strings")?;
        let key = if p.starts_with('/') {
            normalize(p)?
        } else {
            normalize(&format!("/{}", p))?
        };
        push_seen(&mut seen, key.clone())?;
        files.push((key, s.as_bytes().to_vec()));
    }
    Ok(Snapshot {
        dirs: BTreeSet::new(),
        files,
    })
}

/// Standard base64 with padding, strict on decode: `=` only in the final
/// chunk, and non-canonical trailing bits are rejected. In-tree so the
/// snapshot format needs no extra crate.
mod b64 {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub(super) fn encode(data: &[u8]) -> String {
        let digit = |v: u32| ALPHABET[(v & 63) as usize] as char;
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let mut b = [0u8; 3];
            b[..chunk.len()].copy_from_slice(chunk);
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(digit(n >> 18));
            out.push(digit(n >> 12));
            out.push(if chunk.len() > 1 { digit(n >> 6) } else { '=' });
            out.push(if chunk.len() > 2 { digit(n) } else { '=' });
        }
        out
    }

    pub(super) fn decode(s: &str) -> Option<Vec<u8>> {
        let b = s.as_bytes();
        if !b.len().is_multiple_of(4) {
            return None;
        }
        let chunks = b.len() / 4;
        let mut out = Vec::with_capacity(chunks * 3);
        for (ci, chunk) in b.chunks(4).enumerate() {
            let pad = chunk.iter().filter(|&&c| c == b'=').count();
            // Padding belongs to the final chunk only, and at most two of it.
            if pad > 2 || (pad > 0 && ci != chunks - 1) {
                return None;
            }
            let mut n = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                let v = if c == b'=' {
                    0
                } else {
                    ALPHABET.iter().position(|&a| a == c)? as u32
                };
                n |= v << (18 - 6 * i);
            }
            // Non-canonical trailing bits: the padded sextets' unused low
            // bits must be zero, or two inputs decode to one output.
            if (pad == 2 && n & 0xf000 != 0) || (pad == 1 && n & 0xc0 != 0) {
                return None;
            }
            out.push((n >> 16) as u8);
            if pad < 2 {
                out.push((n >> 8) as u8);
            }
            if pad < 1 {
                out.push(n as u8);
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MAX_PATH_BYTES;

    #[test]
    fn b64_roundtrips_everything_and_rejects_junk() {
        let all: Vec<u8> = (0..=255u8).collect();
        for len in [0, 1, 2, 3, 4, 5, 63, 64, 255, 256] {
            let data = &all[..len];
            assert_eq!(b64::decode(&b64::encode(data)).unwrap(), data, "len {}", len);
        }
        assert_eq!(b64::encode(b"f"), "Zg==");
        assert_eq!(b64::encode(b"fo"), "Zm8=");
        assert_eq!(b64::encode(b"foo"), "Zm9v");
        assert_eq!(b64::decode("Zm9v").unwrap(), b"foo");
        assert_eq!(b64::decode("===="), None); // more than two pads
        assert_eq!(b64::decode("A"), None); // not a multiple of four
        assert_eq!(b64::decode("Zm9*"), None); // outside the alphabet
        assert_eq!(b64::decode("Z=9v"), None); // interior padding
    }

    #[test]
    fn b64_padding_only_in_the_final_chunk() {
        assert_eq!(b64::decode("AA==AAAA"), None); // pad, then more data
        assert_eq!(b64::decode("AAAAAA==").unwrap(), vec![0; 4]);
    }

    #[test]
    fn b64_rejects_non_canonical_trailing_bits() {
        assert_eq!(b64::decode("AB=="), None); // 'B' sets unused low bits
        assert_eq!(b64::decode("AA==").unwrap(), vec![0]);
        assert_eq!(b64::decode("AAB="), None); // one pad, low bits set
        assert_eq!(b64::decode("AAA=").unwrap(), vec![0, 0]);
    }

    #[test]
    fn encode_prefers_the_smaller_representation() {
        let mut files = BTreeMap::new();
        files.insert("/t.txt".to_string(), b"plain".to_vec());
        files.insert("/zeros".to_string(), vec![0u8; 300]);
        files.insert("/ctrl".to_string(), vec![0x01u8; 300]);
        files.insert("/bin".to_string(), vec![0xff, 0x00, 0x81]);
        let json = encode(&files, &BTreeSet::from(["/".to_string()])).unwrap();
        assert!(json.contains(r#""/t.txt":{"text":"plain"}"#));
        // 300 NUL bytes: 6 escaped chars each (1800) vs 400 base64
        assert!(json.contains(r#""/zeros":{"b64":""#));
        // 300 0x01 bytes likewise
        assert!(json.contains(r#""/ctrl":{"b64":""#));
        assert!(json.contains(r#""/bin":{"b64":"/wCB"}"#));
        // key order is dirs, files, version (sorted), and roundtrip is exact
        assert!(json.starts_with(r#"{"dirs":["/"],"files":{"#));
        assert!(json.ends_with(r#","version":2}"#));
        let back = parse(&json).unwrap();
        let find = |p: &str| back.files.iter().find(|(k, _)| k == p).unwrap().1.clone();
        assert_eq!(find("/bin"), vec![0xff, 0x00, 0x81]);
        assert_eq!(find("/zeros"), vec![0u8; 300]);
    }

    #[test]
    fn unsupported_version_error_truncates_a_huge_value() {
        let huge = "x".repeat(100_000);
        let err = parse(&format!(r#"{{"version":"{}"}}"#, huge)).unwrap_err();
        assert!(err.starts_with("unsupported snapshot version \"xxx"), "{}", err);
        assert!(err.ends_with("..."), "{}", err);
        assert!(err.len() < 300, "message is {} bytes", err.len());
        // An array value is echoed truncated too, not whole.
        let arr = format!(r#"{{"version":[{}]}}"#, "1,".repeat(50_000) + "1");
        assert!(parse(&arr).unwrap_err().len() < 300);
    }

    #[test]
    fn parse_rejects_unknown_versions_and_bad_shapes() {
        assert_eq!(
            parse(r#"{"version":3,"files":{}}"#).unwrap_err(),
            "unsupported snapshot version 3"
        );
        assert_eq!(
            parse(r#"{"version":"2"}"#).unwrap_err(),
            "unsupported snapshot version \"2\""
        );
        assert!(parse("{not json").unwrap_err().contains("invalid snapshot"));
        assert!(parse("[]").unwrap_err().contains("invalid snapshot"));
        assert!(parse(r#"{"version":2}"#).unwrap_err().contains("files must be an object"));
        assert!(parse(r#"{"version":2,"files":{},"dirs":"x"}"#)
            .unwrap_err()
            .contains("dirs must be an array"));
        assert!(
            parse(r#"{"version":2,"files":{"/x":"a string, not an object"}}"#)
                .unwrap_err()
                .contains("file entry must be")
        );
        assert!(
            parse(r#"{"version":2,"files":{"/x":{"b64":"!!"}}}"#)
                .unwrap_err()
                .contains("bad base64")
        );
        assert!(parse(r#"{"n":1}"#).unwrap_err().contains("old-format values"));
    }

    #[test]
    fn legacy_keys_are_root_prefixed_and_validated() {
        let snap = parse(r#"{"a/b.txt":"x","/c":"y"}"#).unwrap();
        assert_eq!(
            snap.files,
            vec![
                ("/c".to_string(), b"y".to_vec()), // "/c" sorts before "a/b.txt"
                ("/a/b.txt".to_string(), b"x".to_vec())
            ]
        );
        assert!(parse(r#"{"/../escape":"x"}"#)
            .unwrap_err()
            .contains("path escapes root"));
        assert!(
            parse(&format!(r#"{{"{}":"x"}}"#, "k".repeat(MAX_PATH_BYTES)))
                .unwrap_err()
                .contains("path too long")
        );
    }
}
