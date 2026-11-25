//! Persistencia determinista 0-deps del índice incremental (`.onyx/scent.idx`, `SDD-05` REQ-05.2).
//!
//! Formato binario `ONYXSCI1`:
//!
//! ```text
//! magic "ONYXSCI1" | format_version u32 LE | saved_at_secs u64 LE | entry_count u32 LE
//! entradas ordenadas por rel_path:
//!   rel_path (u32 len + UTF-8) | lang u8 | file_len u64 | mtime_secs u64 | mtime_nanos u32
//!   | blake3_hash [u8;32] | entities Vec | raw_calls Vec | raw_type_refs Vec
//! checksum blake3 (32 B) sobre TODO lo anterior
//! ```
//!
//! Cualquier anomalía (archivo ausente, truncado, magic/versión distinta, checksum
//! inválido) se ignora silenciosamente: el índice se reconstruye limpio y la tool nunca
//! falla por causa de la caché.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::lang::{ParsedEntity, ParsedFile, RawCall, RawTypeRef};
use crate::ontology::{EntityKind, RelationKind, ScentError, SupportedLang};

pub(crate) const CACHE_MAGIC: &[u8; 8] = b"ONYXSCI1";
pub(crate) const CACHE_FORMAT_VERSION: u32 = 1;
pub(crate) const CACHE_FILE_NAME: &str = "scent.idx";

/// Asegura `.gitignore` dentro del directorio de la caché (`.onyx/`) para que `git status`
/// del usuario siga limpio, sin pisar reglas preexistentes. Best-effort: los errores se
/// ignoran (una caché no escribible degrada el índice a memoria, nunca falla la tool).
pub(crate) fn ensure_scent_gitignore(cache_dir: &Path) {
    let gitignore = cache_dir.join(".gitignore");
    let required = ["scent.idx", "scent.idx.tmp*"];
    let existing = fs::read_to_string(&gitignore).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
    let mut changed = false;
    for req in required {
        if !lines.iter().any(|l| l.trim() == req) {
            lines.push(req.to_string());
            changed = true;
        }
    }
    if !changed {
        return;
    }
    let mut out = String::new();
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    let _ = fs::write(&gitignore, out);
}

/// Serializa las entradas (ordenadas por `rel_path` gracias al `BTreeMap`) en el payload
/// `ONYXSCI1` completo, incluyendo el checksum `blake3` final.
pub(crate) fn serialize_cache(entries: &BTreeMap<String, ParsedFile>) -> Vec<u8> {
    let saved_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out: Vec<u8> = Vec::with_capacity(entries.len() * 256 + 64);
    out.extend_from_slice(CACHE_MAGIC);
    out.extend_from_slice(&CACHE_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&saved_at_secs.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());

    for (rel_path, parsed) in entries {
        write_str(&mut out, rel_path);
        out.push(parsed.lang.as_u8());
        out.extend_from_slice(&parsed.file_len.to_le_bytes());
        out.extend_from_slice(&parsed.mtime_secs.to_le_bytes());
        out.extend_from_slice(&parsed.mtime_nanos.to_le_bytes());
        out.extend_from_slice(&parsed.blake3_hash);

        out.extend_from_slice(&(parsed.entities.len() as u32).to_le_bytes());
        for ent in &parsed.entities {
            write_str(&mut out, &ent.short_name);
            write_opt_str(&mut out, ent.owner_type.as_deref());
            out.push(ent.kind.as_u8());
            write_str(&mut out, &ent.signature);
            out.push(u8::from(ent.is_public));
            out.push(u8::from(ent.is_test));
            out.extend_from_slice(&(ent.start_line as u32).to_le_bytes());
            out.extend_from_slice(&(ent.end_line as u32).to_le_bytes());
        }

        out.extend_from_slice(&(parsed.raw_calls.len() as u32).to_le_bytes());
        for call in &parsed.raw_calls {
            out.extend_from_slice(&(call.caller_entity_idx as u32).to_le_bytes());
            write_str(&mut out, &call.callee_name);
            write_opt_str(&mut out, call.qualifier.as_deref());
            out.push(u8::from(call.is_method_call));
        }

        out.extend_from_slice(&(parsed.raw_type_refs.len() as u32).to_le_bytes());
        for tref in &parsed.raw_type_refs {
            out.extend_from_slice(&(tref.source_entity_idx as u32).to_le_bytes());
            write_str(&mut out, &tref.target_type);
            out.push(tref.kind.as_u8());
        }
    }

    let checksum = blake3::hash(&out);
    out.extend_from_slice(checksum.as_bytes());
    out
}

/// Deserializa y valida el payload `ONYXSCI1` completo. Cualquier anomalía produce un
/// error (el caller degrada a reconstrucción limpia).
pub(crate) fn deserialize_cache(bytes: &[u8]) -> Result<BTreeMap<String, ParsedFile>, ScentError> {
    const PATH: &str = ".onyx/scent.idx";
    if bytes.len() < 8 + 4 + 8 + 4 + 32 {
        return Err(invalid(PATH, "payload too short"));
    }
    if &bytes[..8] != CACHE_MAGIC {
        return Err(invalid(PATH, "bad magic"));
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != CACHE_FORMAT_VERSION {
        return Err(invalid(PATH, &format!("unsupported version {version}")));
    }
    let checksummed = &bytes[..bytes.len() - 32];
    let expected: [u8; 32] = bytes[bytes.len() - 32..].try_into().unwrap();
    if blake3::hash(checksummed).as_bytes() != &expected {
        return Err(invalid(PATH, "checksum mismatch"));
    }

    let mut cur = Cursor {
        bytes: &checksummed[20..],
        pos: 0,
    };
    let entry_count = cur.read_u32(PATH)?;
    let mut entries = BTreeMap::new();
    for _ in 0..entry_count {
        let rel_path = cur.read_string(PATH)?.to_string();
        let lang = SupportedLang::from_u8(cur.read_u8(PATH)?)
            .ok_or_else(|| invalid(PATH, "bad lang tag"))?;
        let file_len = cur.read_u64(PATH)?;
        let mtime_secs = cur.read_u64(PATH)?;
        let mtime_nanos = cur.read_u32(PATH)?;
        let blake3_hash: [u8; 32] = cur.read_bytes(32, PATH)?.try_into().unwrap();

        let entity_count = cur.read_u32(PATH)? as usize;
        let mut entities = Vec::with_capacity(entity_count.min(1_000_000));
        for _ in 0..entity_count {
            let short_name = cur.read_string(PATH)?.to_string();
            let owner_type = cur.read_opt_string(PATH)?;
            let kind = EntityKind::from_u8(cur.read_u8(PATH)?)
                .ok_or_else(|| invalid(PATH, "bad kind tag"))?;
            let signature = cur.read_string(PATH)?.to_string();
            let is_public = cur.read_u8(PATH)? != 0;
            let is_test = cur.read_u8(PATH)? != 0;
            let start_line = cur.read_u32(PATH)? as usize;
            let end_line = cur.read_u32(PATH)? as usize;
            entities.push(ParsedEntity {
                short_name,
                owner_type,
                kind,
                signature,
                is_public,
                is_test,
                start_line,
                end_line,
            });
        }

        let call_count = cur.read_u32(PATH)? as usize;
        let mut raw_calls = Vec::with_capacity(call_count.min(1_000_000));
        for _ in 0..call_count {
            let caller_entity_idx = cur.read_u32(PATH)? as usize;
            let callee_name = cur.read_string(PATH)?.to_string();
            let qualifier = cur.read_opt_string(PATH)?;
            let is_method_call = cur.read_u8(PATH)? != 0;
            raw_calls.push(RawCall {
                caller_entity_idx,
                callee_name,
                qualifier,
                is_method_call,
            });
        }

        let ref_count = cur.read_u32(PATH)? as usize;
        let mut raw_type_refs = Vec::with_capacity(ref_count.min(1_000_000));
        for _ in 0..ref_count {
            let source_entity_idx = cur.read_u32(PATH)? as usize;
            let target_type = cur.read_string(PATH)?.to_string();
            let kind = RelationKind::from_u8(cur.read_u8(PATH)?)
                .ok_or_else(|| invalid(PATH, "bad relation tag"))?;
            raw_type_refs.push(RawTypeRef {
                source_entity_idx,
                target_type,
                kind,
            });
        }

        entries.insert(
            rel_path,
            ParsedFile {
                lang,
                blake3_hash,
                file_len,
                mtime_secs,
                mtime_nanos,
                entities,
                raw_calls,
                raw_type_refs,
            },
        );
    }

    // Sin bytes sobrantes tras la última entrada (payload truncado de forma maliciosa o corrupto).
    if cur.pos != cur.bytes.len() {
        return Err(invalid(PATH, "trailing bytes after last entry"));
    }

    Ok(entries)
}

/// Rutea la caché persistente de un workspace a `<root>/.onyx/scent.idx`.
pub(crate) fn cache_path_for(root: &Path) -> PathBuf {
    root.join(".onyx").join(CACHE_FILE_NAME)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn read_u8(&mut self, path: &str) -> Result<u8, ScentError> {
        Ok(self.read_bytes(1, path)?[0])
    }

    fn read_u32(&mut self, path: &str) -> Result<u32, ScentError> {
        Ok(u32::from_le_bytes(
            self.read_bytes(4, path)?.try_into().unwrap(),
        ))
    }

    fn read_u64(&mut self, path: &str) -> Result<u64, ScentError> {
        Ok(u64::from_le_bytes(
            self.read_bytes(8, path)?.try_into().unwrap(),
        ))
    }

    fn read_bytes(&mut self, len: usize, path: &str) -> Result<&'a [u8], ScentError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|e| *e <= self.bytes.len())
            .ok_or_else(|| invalid(path, "truncated payload"))?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_string(&mut self, path: &str) -> Result<&'a str, ScentError> {
        let len = self.read_u32(path)? as usize;
        let raw = self.read_bytes(len, path)?;
        std::str::from_utf8(raw).map_err(|e| invalid(path, &format!("non-UTF-8 field: {e}")))
    }

    fn read_opt_string(&mut self, path: &str) -> Result<Option<String>, ScentError> {
        let flag = self.read_u8(path)?;
        if flag == 0 {
            Ok(None)
        } else {
            Ok(Some(self.read_string(path)?.to_string()))
        }
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn write_opt_str(out: &mut Vec<u8>, s: Option<&str>) {
    match s {
        Some(v) => {
            out.push(1);
            write_str(out, v);
        }
        None => out.push(0),
    }
}

fn invalid(path: &str, message: &str) -> ScentError {
    ScentError::Parse {
        path: path.to_string(),
        message: format!("invalid scent cache: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ontology::EntityKind;

    fn sample_entries() -> BTreeMap<String, ParsedFile> {
        let mut map = BTreeMap::new();
        map.insert(
            "src/lib.rs".to_string(),
            ParsedFile {
                lang: SupportedLang::Rust,
                blake3_hash: [7u8; 32],
                file_len: 42,
                mtime_secs: 1_000,
                mtime_nanos: 5,
                entities: vec![ParsedEntity {
                    short_name: "alpha".to_string(),
                    owner_type: Some("core".to_string()),
                    kind: EntityKind::Function,
                    signature: "pub fn alpha() -> i32".to_string(),
                    is_public: true,
                    is_test: false,
                    start_line: 1,
                    end_line: 2,
                }],
                raw_calls: vec![RawCall {
                    caller_entity_idx: 0,
                    callee_name: "beta".to_string(),
                    qualifier: None,
                    is_method_call: false,
                }],
                raw_type_refs: vec![RawTypeRef {
                    source_entity_idx: 0,
                    target_type: "Widget".to_string(),
                    kind: RelationKind::UsesType,
                }],
            },
        );
        map.insert(
            "top.ts".to_string(),
            ParsedFile {
                lang: SupportedLang::TypeScript,
                blake3_hash: [0xAB; 32],
                file_len: 11,
                mtime_secs: 2_000,
                mtime_nanos: 0,
                entities: vec![],
                raw_calls: vec![],
                raw_type_refs: vec![],
            },
        );
        map
    }

    #[test]
    fn onyxsci1_round_trip_preserves_entries_and_order() {
        let entries = sample_entries();
        let bytes = serialize_cache(&entries);
        assert_eq!(&bytes[..8], b"ONYXSCI1");
        let restored = deserialize_cache(&bytes).unwrap();
        assert_eq!(restored, entries);
    }

    #[test]
    fn onyxsci1_rejects_corrupt_truncated_and_foreign_payloads() {
        let bytes = serialize_cache(&sample_entries());

        // Bit flip en el medio → checksum inválido.
        let mut corrupted = bytes.clone();
        corrupted[40] ^= 0xff;
        assert!(deserialize_cache(&corrupted).is_err());

        // Truncado.
        assert!(deserialize_cache(&bytes[..bytes.len() / 2]).is_err());

        // Magic distinto.
        let mut foreign = bytes.clone();
        foreign[..4].copy_from_slice(b"XXXX");
        assert!(deserialize_cache(&foreign).is_err());

        // Versión no soportada (checksum revalidado tras el cambio).
        let mut v2 = bytes.clone();
        v2[8..12].copy_from_slice(&2u32.to_le_bytes());
        let checksum = blake3::hash(&v2[..v2.len() - 32]);
        let tail = v2.len() - 32;
        v2[tail..].copy_from_slice(checksum.as_bytes());
        assert!(deserialize_cache(&v2).is_err());

        // Payload demasiado corto.
        assert!(deserialize_cache(b"ONYXSCI1").is_err());
    }
}
