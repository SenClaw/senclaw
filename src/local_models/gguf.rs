//! Minimal GGUF header/metadata reader — just enough to classify a checkpoint
//! (architecture, context length, embedding capability) without reading its
//! tensor data. Format: <https://github.com/ggml-org/ggml/blob/master/docs/gguf.md>.

use std::collections::HashMap;
use std::io::{self, BufReader, Read};
use std::path::Path;

const MAGIC: u32 = 0x4655_4747; // b"GGUF" read little-endian as u32

#[derive(Debug, Clone)]
pub enum MetaValue {
    U64(u64),
    I64(i64),
    F64(f64),
    Bool(bool),
    String(String),
    /// An array or any value type this reader does not need for
    /// classification — consumed (so the stream stays in sync) but discarded.
    Other,
}

pub struct GgufMetadata {
    values: HashMap<String, MetaValue>,
}

impl GgufMetadata {
    pub fn architecture(&self) -> Option<&str> {
        match self.values.get("general.architecture") {
            Some(MetaValue::String(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    fn arch_u32(&self, suffix: &str) -> Option<u32> {
        let arch = self.architecture()?;
        match self.values.get(&format!("{arch}.{suffix}")) {
            Some(MetaValue::U64(n)) => Some(*n as u32),
            Some(MetaValue::I64(n)) => Some(*n as u32),
            _ => None,
        }
    }

    pub fn context_length(&self) -> Option<u32> {
        self.arch_u32("context_length")
    }

    pub fn embedding_length(&self) -> Option<u32> {
        self.arch_u32("embedding_length")
    }

    /// Whether the pooling-type key is present — llama.cpp only emits it for
    /// embedding-family conversions, so its presence (regardless of value) is
    /// the reliable signal, not the architecture name.
    pub fn has_pooling_type(&self) -> bool {
        self.architecture().is_some_and(|a| self.values.contains_key(&format!("{a}.pooling_type")))
    }
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_string(r: &mut impl Read) -> io::Result<String> {
    let len = read_u64(r)?;
    // A corrupt length must not turn into an out-of-memory allocation.
    if len > 64 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible GGUF string length"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Read one value of `value_type` (the GGUF metadata value-type enum),
/// consuming exactly its bytes. Correctness here is about staying in sync
/// with the stream through values this reader does not care about (arrays
/// included), not about preserving their content.
fn read_value(r: &mut impl Read, value_type: u32) -> io::Result<MetaValue> {
    match value_type {
        0 => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b)?;
            Ok(MetaValue::U64(b[0] as u64))
        } // UINT8
        1 => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b)?;
            Ok(MetaValue::I64(b[0] as i8 as i64))
        } // INT8
        2 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            Ok(MetaValue::U64(u16::from_le_bytes(b) as u64))
        } // UINT16
        3 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b)?;
            Ok(MetaValue::I64(i16::from_le_bytes(b) as i64))
        } // INT16
        4 => Ok(MetaValue::U64(read_u32(r)? as u64)),           // UINT32
        5 => Ok(MetaValue::I64(read_u32(r)? as i32 as i64)),    // INT32
        6 => {
            let mut b = [0u8; 4];
            r.read_exact(&mut b)?;
            Ok(MetaValue::F64(f32::from_le_bytes(b) as f64))
        } // FLOAT32
        7 => {
            let mut b = [0u8; 1];
            r.read_exact(&mut b)?;
            Ok(MetaValue::Bool(b[0] != 0))
        } // BOOL
        8 => Ok(MetaValue::String(read_string(r)?)), // STRING
        9 => {
            // ARRAY: element type, element count, then that many elements.
            let elem_type = read_u32(r)?;
            let count = read_u64(r)?;
            for _ in 0..count {
                read_value(r, elem_type)?;
            }
            Ok(MetaValue::Other)
        }
        10 => Ok(MetaValue::U64(read_u64(r)?)),          // UINT64
        11 => Ok(MetaValue::I64(read_u64(r)? as i64)),   // INT64
        12 => {
            let mut b = [0u8; 8];
            r.read_exact(&mut b)?;
            Ok(MetaValue::F64(f64::from_le_bytes(b)))
        } // FLOAT64
        other => Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown GGUF value type {other}"))),
    }
}

/// Read just the metadata (key/value header) of a `.gguf` file, stopping
/// before any tensor descriptor or tensor data — a multi-gigabyte checkpoint
/// costs only its first few KB to classify. `None` for anything that is not a
/// well-formed GGUF file: a scan must never fail the whole directory listing
/// over one bad file.
pub fn read_metadata(path: &Path) -> Option<GgufMetadata> {
    let file = std::fs::File::open(path).ok()?;
    let mut r = BufReader::new(file);
    if read_u32(&mut r).ok()? != MAGIC {
        return None;
    }
    let _version = read_u32(&mut r).ok()?;
    let _tensor_count = read_u64(&mut r).ok()?;
    let kv_count = read_u64(&mut r).ok()?;
    let mut values = HashMap::with_capacity(kv_count.min(4096) as usize);
    for _ in 0..kv_count {
        let key = read_string(&mut r).ok()?;
        let value_type = read_u32(&mut r).ok()?;
        let value = read_value(&mut r, value_type).ok()?;
        values.insert(key, value);
    }
    Some(GgufMetadata { values })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a byte-accurate minimal GGUF file: `general.architecture` =
    /// "llama", `llama.context_length` = 8192, plus one array value (to prove
    /// the reader skips it and stays in sync) and, optionally, a pooling-type
    /// key (embedding marker).
    fn fake_gguf(path: &Path, with_pooling_type: bool) {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&3u32.to_le_bytes()); // version
        buf.extend_from_slice(&0u64.to_le_bytes()); // tensor_count

        let mut kvs: Vec<(&str, u32, Vec<u8>)> = Vec::new();
        let string_val = |s: &str| {
            let mut v = (s.len() as u64).to_le_bytes().to_vec();
            v.extend_from_slice(s.as_bytes());
            v
        };
        kvs.push(("general.architecture", 8, string_val("llama")));
        kvs.push(("llama.context_length", 4, 8192u32.to_le_bytes().to_vec()));
        // An array of 3 strings — must be skippable without desyncing.
        let mut arr = 8u32.to_le_bytes().to_vec(); // element type = STRING
        arr.extend_from_slice(&3u64.to_le_bytes()); // count
        for tok in ["a", "bb", "ccc"] {
            arr.extend_from_slice(&string_val(tok));
        }
        kvs.push(("tokenizer.ggml.tokens", 9, arr));
        if with_pooling_type {
            kvs.push(("llama.pooling_type", 4, 1u32.to_le_bytes().to_vec()));
        }

        buf.extend_from_slice(&(kvs.len() as u64).to_le_bytes());
        for (key, value_type, value_bytes) in kvs {
            buf.extend_from_slice(&string_val(key));
            buf.extend_from_slice(&value_type.to_le_bytes());
            buf.extend_from_slice(&value_bytes);
        }

        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(&buf).unwrap();
    }

    #[test]
    fn reads_architecture_and_context_length_and_skips_the_array() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("model.gguf");
        fake_gguf(&path, false);
        let meta = read_metadata(&path).unwrap();
        assert_eq!(meta.architecture(), Some("llama"));
        assert_eq!(meta.context_length(), Some(8192));
        assert!(!meta.has_pooling_type());
    }

    #[test]
    fn pooling_type_presence_marks_an_embedding_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("embed.gguf");
        fake_gguf(&path, true);
        let meta = read_metadata(&path).unwrap();
        assert!(meta.has_pooling_type());
    }

    #[test]
    fn a_non_gguf_file_is_none_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not-a-model.gguf");
        std::fs::write(&path, b"not a gguf file at all").unwrap();
        assert!(read_metadata(&path).is_none());
    }
}
