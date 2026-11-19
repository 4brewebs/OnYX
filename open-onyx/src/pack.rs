//! Contenedor append-only comprimido (`snapshots.onyxpack`) y sistema determinista de hitos.
//!
//! Mantiene intacto el formato canónico `ONYXAG\x01\0` y las 0 dependencias externas
//! de `onyx-core`/`onyx-soul-agents`, empaquetando en `open-onyx` los snapshots de hitos
//! en un único archivo comprimido con zlib y checksum FNV-1a de 64 bits, con lectura
//! retrocompatible de directorios legados (`tick-*.bin`).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use onyx_soul_agents::{
    AgentState, AttrRegistry, ConceptId, Ctx, GraphView, IngressEvent, OnyxError, OnyxEvent,
    OnyxSystem, SnapshotStore, SystemEvent, Value, Vocab,
};

pub const PACK_FILE_NAME: &str = "snapshots.onyxpack";

const FRAME_MAGIC_PREFIX: &[u8; 7] = b"ONYXPK1";
const CODEC_RAW: u8 = 0;
const CODEC_ZLIB: u8 = 1;
const FRAME_PREFIX_LEN: usize = 7 + 1 + 8 + 2; // magic(7) + codec(1) + tick(8) + tag_len(2)
const FRAME_META_LEN: usize = 8 + 8 + 8; // raw_len(8) + stored_len(8) + checksum(8)

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotSource {
    PackFrame {
        offset: u64,
        stored_len: u64,
        codec: u8,
    },
    LegacyBin(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotMeta {
    pub tick: u64,
    pub tag: String,
    pub raw_len: u64,
    pub stored_len: u64,
    pub checksum: u64,
    pub source: SnapshotSource,
}

pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn parse_tick_key(key: &str) -> Option<(u64, &str)> {
    if let Ok(t) = key.parse::<u64>() {
        return Some((t, "tick"));
    }
    let (tick_part, tag) = key.split_once(':')?;
    let tick = tick_part.parse::<u64>().ok()?;
    let tag = if tag.trim().is_empty() { "tick" } else { tag };
    Some((tick, tag))
}

fn legacy_tick_of(name: &str) -> Option<u64> {
    let tick: u64 = name
        .strip_prefix("tick-")?
        .strip_suffix(".bin")?
        .parse()
        .ok()?;
    (name == format!("tick-{tick}.bin")).then_some(tick)
}

fn free_path(dir: &Path, key: &str) -> Result<PathBuf, OnyxError> {
    let mut parts = Path::new(key).components();
    let single = matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(name)), None) if name == key
    );
    if !single {
        return Err(OnyxError::Format(format!(
            "snapshot key {key:?} must be a single relative file name"
        )));
    }
    if key == PACK_FILE_NAME || legacy_tick_of(key).is_some() || parse_tick_key(key).is_some() {
        return Err(OnyxError::Format(format!(
            "snapshot key {key:?} is reserved"
        )));
    }
    Ok(dir.join(key))
}

fn compress_payload(raw: &[u8]) -> Result<(u8, Vec<u8>), OnyxError> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder
        .write_all(raw)
        .map_err(|e| OnyxError::Io(e.to_string()))?;
    let compressed = encoder.finish().map_err(|e| OnyxError::Io(e.to_string()))?;
    if compressed.len() < raw.len() {
        Ok((CODEC_ZLIB, compressed))
    } else {
        Ok((CODEC_RAW, raw.to_vec()))
    }
}

fn decompress_payload(codec: u8, stored: &[u8]) -> Result<Vec<u8>, OnyxError> {
    match codec {
        CODEC_RAW => Ok(stored.to_vec()),
        CODEC_ZLIB => {
            let mut decoder = ZlibDecoder::new(stored);
            let mut out = Vec::new();
            decoder.read_to_end(&mut out).map_err(|e| {
                OnyxError::Format(format!("invalid compressed snapshot frame: {e}"))
            })?;
            Ok(out)
        }
        other => Err(OnyxError::Format(format!("unknown snapshot codec {other}"))),
    }
}

fn encode_frame(
    codec: u8,
    tick: u64,
    tag: &str,
    raw_len: u64,
    checksum: u64,
    payload: &[u8],
) -> Result<(Vec<u8>, usize), OnyxError> {
    let tag_bytes = tag.as_bytes();
    let tag_len = u16::try_from(tag_bytes.len())
        .map_err(|_| OnyxError::Format("snapshot tag exceeds 64 KiB".into()))?;
    let stored_len = u64::try_from(payload.len())
        .map_err(|_| OnyxError::Format("snapshot payload exceeds u64".into()))?;

    let header_len = FRAME_PREFIX_LEN + tag_bytes.len() + FRAME_META_LEN;
    let mut out = Vec::with_capacity(header_len + payload.len());
    out.extend_from_slice(FRAME_MAGIC_PREFIX);
    out.push(codec);
    out.extend_from_slice(&tick.to_le_bytes());
    out.extend_from_slice(&tag_len.to_le_bytes());
    out.extend_from_slice(tag_bytes);
    out.extend_from_slice(&raw_len.to_le_bytes());
    out.extend_from_slice(&stored_len.to_le_bytes());
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(payload);
    Ok((out, header_len))
}

/// Almacén append-only de snapshots comprimidos (`snapshots.onyxpack`) con fallback legado a `tick-N.bin`.
pub struct PackSnapshotStore {
    dir: PathBuf,
    entries: BTreeMap<u64, SnapshotMeta>,
    valid_pack_len: u64,
    truncated_tail_bytes: usize,
    corruption_error: Option<String>,
    max_snapshots: Option<usize>,
}

impl PackSnapshotStore {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref().to_path_buf();
        let mut store = Self {
            dir,
            entries: BTreeMap::new(),
            valid_pack_len: 0,
            truncated_tail_bytes: 0,
            corruption_error: None,
            max_snapshots: None,
        };
        store.scan_existing();
        store
    }

    pub fn with_max_snapshots(mut self, max: Option<usize>) -> Self {
        self.max_snapshots = max.filter(|&n| n > 0);
        self
    }

    pub fn pack_path(&self) -> PathBuf {
        self.dir.join(PACK_FILE_NAME)
    }

    pub fn latest_tick(&self) -> Option<u64> {
        self.entries.keys().next_back().copied()
    }

    pub fn ticks(&self) -> Vec<u64> {
        self.entries.keys().copied().collect()
    }

    pub fn entries(&self) -> Vec<SnapshotMeta> {
        self.entries.values().cloned().collect()
    }

    pub fn get_meta(&self, tick: u64) -> Option<&SnapshotMeta> {
        self.entries.get(&tick)
    }

    pub fn has_tick(&self, tick: u64) -> bool {
        self.entries.contains_key(&tick)
    }

    pub fn resolve_tick_or_floor(&self, requested: u64) -> Option<u64> {
        self.entries
            .range(..=requested)
            .next_back()
            .map(|(&tick, _)| tick)
    }

    pub fn truncated_tail_bytes(&self) -> usize {
        self.truncated_tail_bytes
    }

    pub fn corruption_error(&self) -> Option<&str> {
        self.corruption_error.as_deref()
    }

    fn scan_existing(&mut self) {
        self.entries.clear();
        self.valid_pack_len = 0;
        self.truncated_tail_bytes = 0;
        self.corruption_error = None;

        if let Ok(read_dir) = fs::read_dir(&self.dir) {
            let mut legacy = Vec::new();
            for entry in read_dir.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if let Some(tick) = legacy_tick_of(name) {
                    legacy.push((tick, path));
                }
            }
            legacy.sort_by_key(|(tick, _)| *tick);
            for (tick, path) in legacy {
                if let Ok(raw) = fs::read(&path) {
                    let len = raw.len() as u64;
                    self.entries.insert(
                        tick,
                        SnapshotMeta {
                            tick,
                            tag: "legacy".to_string(),
                            raw_len: len,
                            stored_len: len,
                            checksum: fnv1a_64(&raw),
                            source: SnapshotSource::LegacyBin(path),
                        },
                    );
                }
            }
        }

        let pack_path = self.pack_path();
        if !pack_path.exists() {
            return;
        }
        let Ok(bytes) = fs::read(&pack_path) else {
            self.corruption_error = Some(format!("cannot read `{}`", pack_path.display()));
            return;
        };

        let mut cursor = 0usize;
        while cursor < bytes.len() {
            let remaining = bytes.len() - cursor;
            if remaining < FRAME_PREFIX_LEN {
                self.truncated_tail_bytes = remaining;
                break;
            }
            if &bytes[cursor..cursor + 7] != FRAME_MAGIC_PREFIX {
                self.corruption_error = Some(format!(
                    "corrupt frame magic in `{}` at byte offset {cursor}",
                    pack_path.display()
                ));
                break;
            }
            let codec = bytes[cursor + 7];
            if codec != CODEC_RAW && codec != CODEC_ZLIB {
                self.corruption_error = Some(format!(
                    "unsupported codec {codec} in `{}` at byte offset {cursor}",
                    pack_path.display()
                ));
                break;
            }
            let tick = u64::from_le_bytes(bytes[cursor + 8..cursor + 16].try_into().unwrap());
            let tag_len = usize::from(u16::from_le_bytes(
                bytes[cursor + 16..cursor + 18].try_into().unwrap(),
            ));
            let Some(meta_start) = cursor.checked_add(FRAME_PREFIX_LEN + tag_len) else {
                self.truncated_tail_bytes = remaining;
                break;
            };
            let Some(payload_start) = meta_start.checked_add(FRAME_META_LEN) else {
                self.truncated_tail_bytes = remaining;
                break;
            };
            if payload_start > bytes.len() {
                self.truncated_tail_bytes = remaining;
                break;
            }
            let Ok(tag) = std::str::from_utf8(&bytes[cursor + FRAME_PREFIX_LEN..meta_start]) else {
                self.corruption_error = Some(format!(
                    "invalid UTF-8 milestone tag in `{}` at byte offset {cursor}",
                    pack_path.display()
                ));
                break;
            };
            let raw_len = u64::from_le_bytes(bytes[meta_start..meta_start + 8].try_into().unwrap());
            let stored_len =
                u64::from_le_bytes(bytes[meta_start + 8..meta_start + 16].try_into().unwrap());
            let checksum =
                u64::from_le_bytes(bytes[meta_start + 16..meta_start + 24].try_into().unwrap());
            let Ok(stored_len_usize) = usize::try_from(stored_len) else {
                self.truncated_tail_bytes = remaining;
                break;
            };
            let Some(frame_end) = payload_start.checked_add(stored_len_usize) else {
                self.truncated_tail_bytes = remaining;
                break;
            };
            if frame_end > bytes.len() {
                self.truncated_tail_bytes = remaining;
                break;
            }

            self.entries.insert(
                tick,
                SnapshotMeta {
                    tick,
                    tag: tag.to_string(),
                    raw_len,
                    stored_len,
                    checksum,
                    source: SnapshotSource::PackFrame {
                        offset: payload_start as u64,
                        stored_len,
                        codec,
                    },
                },
            );
            cursor = frame_end;
            self.valid_pack_len = cursor as u64;
        }
    }

    pub fn save_milestone(&mut self, tick: u64, tag: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        let checksum = fnv1a_64(bytes);
        if let Some(existing) = self.entries.get(&tick) {
            if matches!(existing.source, SnapshotSource::PackFrame { .. })
                && existing.checksum == checksum
                && (existing.tag == tag || matches!(tag, "exit" | "tick"))
            {
                return Ok(());
            }
        }
        if let Some(err) = &self.corruption_error {
            return Err(OnyxError::Format(err.clone()));
        }

        let (codec, payload) = compress_payload(bytes)?;
        let raw_len = u64::try_from(bytes.len())
            .map_err(|_| OnyxError::Format("snapshot payload exceeds u64".into()))?;
        let stored_len = u64::try_from(payload.len())
            .map_err(|_| OnyxError::Format("compressed payload exceeds u64".into()))?;
        let (frame, header_len) = encode_frame(codec, tick, tag, raw_len, checksum, &payload)?;

        let pack_path = self.pack_path();
        fs::create_dir_all(&self.dir).map_err(|e| OnyxError::Io(e.to_string()))?;
        {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&pack_path)
                .map_err(|e| OnyxError::Io(e.to_string()))?;
            if self.truncated_tail_bytes > 0 {
                file.set_len(self.valid_pack_len)
                    .map_err(|e| OnyxError::Io(e.to_string()))?;
                self.truncated_tail_bytes = 0;
            }
            file.seek(SeekFrom::Start(self.valid_pack_len))
                .map_err(|e| OnyxError::Io(e.to_string()))?;
            file.write_all(&frame)
                .map_err(|e| OnyxError::Io(e.to_string()))?;
            file.flush().map_err(|e| OnyxError::Io(e.to_string()))?;
        }

        let payload_offset = self.valid_pack_len + header_len as u64;
        self.valid_pack_len += frame.len() as u64;
        self.entries.insert(
            tick,
            SnapshotMeta {
                tick,
                tag: tag.to_string(),
                raw_len,
                stored_len,
                checksum,
                source: SnapshotSource::PackFrame {
                    offset: payload_offset,
                    stored_len,
                    codec,
                },
            },
        );

        self.enforce_max_snapshots()?;
        Ok(())
    }

    fn enforce_max_snapshots(&mut self) -> Result<(), OnyxError> {
        let Some(max) = self.max_snapshots else {
            return Ok(());
        };
        if self.entries.len() <= max {
            return Ok(());
        }
        let mut pruned_pack = false;
        let mut legacy_to_remove = Vec::new();
        while self.entries.len() > max {
            let Some((_, removed)) = self.entries.pop_first() else {
                break;
            };
            match removed.source {
                SnapshotSource::PackFrame { .. } => pruned_pack = true,
                SnapshotSource::LegacyBin(path) => legacy_to_remove.push(path),
            }
        }
        for path in legacy_to_remove {
            let _ = fs::remove_file(path);
        }
        if !pruned_pack {
            return Ok(());
        }

        let pack_path = self.pack_path();
        let tmp_path = self.dir.join(format!("{PACK_FILE_NAME}.tmp"));
        let mut new_entries = BTreeMap::new();
        let mut new_len = 0u64;

        {
            let mut reader = File::open(&pack_path).map_err(|e| OnyxError::Io(e.to_string()))?;
            let mut writer = File::create(&tmp_path).map_err(|e| OnyxError::Io(e.to_string()))?;
            for (&tick, meta) in &self.entries {
                if let SnapshotSource::PackFrame {
                    offset,
                    stored_len,
                    codec,
                } = &meta.source
                {
                    let len_usize = usize::try_from(*stored_len)
                        .map_err(|_| OnyxError::Format("stored snapshot exceeds usize".into()))?;
                    let mut payload = vec![0u8; len_usize];
                    reader
                        .seek(SeekFrom::Start(*offset))
                        .map_err(|e| OnyxError::Io(e.to_string()))?;
                    reader
                        .read_exact(&mut payload)
                        .map_err(|e| OnyxError::Io(e.to_string()))?;
                    let (frame, header_len) = encode_frame(
                        *codec,
                        tick,
                        &meta.tag,
                        meta.raw_len,
                        meta.checksum,
                        &payload,
                    )?;
                    writer
                        .write_all(&frame)
                        .map_err(|e| OnyxError::Io(e.to_string()))?;
                    let new_payload_offset = new_len + header_len as u64;
                    new_len += frame.len() as u64;
                    let mut updated = meta.clone();
                    updated.source = SnapshotSource::PackFrame {
                        offset: new_payload_offset,
                        stored_len: *stored_len,
                        codec: *codec,
                    };
                    new_entries.insert(tick, updated);
                } else {
                    new_entries.insert(tick, meta.clone());
                }
            }
            writer.flush().map_err(|e| OnyxError::Io(e.to_string()))?;
        }

        fs::rename(&tmp_path, &pack_path).map_err(|e| OnyxError::Io(e.to_string()))?;
        self.entries = new_entries;
        self.valid_pack_len = new_len;
        Ok(())
    }

    pub fn load_tick(&self, tick: u64) -> Result<Vec<u8>, OnyxError> {
        let meta = self.entries.get(&tick).ok_or(OnyxError::Missing)?;
        let raw = match &meta.source {
            SnapshotSource::PackFrame {
                offset,
                stored_len,
                codec,
            } => {
                let len_usize = usize::try_from(*stored_len)
                    .map_err(|_| OnyxError::Format("stored snapshot exceeds usize".into()))?;
                let mut f =
                    File::open(self.pack_path()).map_err(|e| OnyxError::Io(e.to_string()))?;
                f.seek(SeekFrom::Start(*offset))
                    .map_err(|e| OnyxError::Io(e.to_string()))?;
                let mut stored = vec![0u8; len_usize];
                f.read_exact(&mut stored)
                    .map_err(|e| OnyxError::Io(e.to_string()))?;
                decompress_payload(*codec, &stored)?
            }
            SnapshotSource::LegacyBin(path) => {
                fs::read(path).map_err(|e| OnyxError::Io(e.to_string()))?
            }
        };
        if raw.len() as u64 != meta.raw_len || fnv1a_64(&raw) != meta.checksum {
            return Err(OnyxError::Format(format!(
                "snapshot tick {tick} checksum or length mismatch"
            )));
        }
        Ok(raw)
    }
}

impl SnapshotStore for PackSnapshotStore {
    fn save(&mut self, key: &str, bytes: &[u8]) -> Result<(), OnyxError> {
        if let Some((tick, tag)) = parse_tick_key(key) {
            self.save_milestone(tick, tag, bytes)
        } else {
            fs::create_dir_all(&self.dir).map_err(|e| OnyxError::Io(e.to_string()))?;
            let path = free_path(&self.dir, key)?;
            let mut f = File::create(&path).map_err(|e| OnyxError::Io(e.to_string()))?;
            f.write_all(bytes)
                .map_err(|e| OnyxError::Io(e.to_string()))?;
            Ok(())
        }
    }

    fn load(&self, key: &str) -> Result<Vec<u8>, OnyxError> {
        if let Some((tick, _)) = parse_tick_key(key) {
            self.load_tick(tick)
        } else {
            let path = free_path(&self.dir, key)?;
            fs::read(&path).map_err(|e| OnyxError::Io(e.to_string()))
        }
    }
}

/// Sistema determinista que solicita snapshots solo en hitos con valor real
/// (`init`, `spawn`, `approval:*`, `tool:*`, `tool_error:*`, `cmd:*`, `settled`/`failed` y opcional `periodic`).
pub struct MilestoneSnapshotSystem {
    vocab: Option<Vocab>,
    snapshot_every: Option<u64>,
    prev_states: BTreeMap<u64, AgentState>,
    prev_settled: bool,
}

impl MilestoneSnapshotSystem {
    pub fn new(snapshot_every: Option<u64>) -> Self {
        Self {
            vocab: None,
            snapshot_every: snapshot_every.filter(|&n| n > 0),
            prev_states: BTreeMap::new(),
            prev_settled: false,
        }
    }

    fn detect_tool_milestone<G: GraphView + ?Sized>(
        &self,
        graph: &G,
        v: Vocab,
        agent_id: ConceptId,
        now: i64,
    ) -> Option<String> {
        let mut entries: Vec<(i64, String, String, i64)> = graph
            .related(agent_id, v.rel_memory)
            .into_iter()
            .filter_map(|mem_id| {
                let seq = match graph.get(mem_id, v.mem_seq)? {
                    Value::Int(s) => s,
                    _ => return None,
                };
                let kind = match graph.get(mem_id, v.mem_kind)? {
                    Value::Text(k) => k,
                    _ => return None,
                };
                let text = match graph.get(mem_id, v.mem_text) {
                    Some(Value::Text(t)) => t,
                    _ => String::new(),
                };
                let mem_tick = match graph.get(mem_id, v.mem_tick) {
                    Some(Value::Int(t)) => t,
                    _ => -1,
                };
                Some((seq, kind, text, mem_tick))
            })
            .collect();
        entries.sort_by_key(|(seq, _, _, _)| *seq);

        let mut last_call_tool: Option<String> = None;
        let mut detected: Option<String> = None;
        for (_, kind, text, mem_tick) in entries {
            match kind.as_str() {
                "call" => {
                    let tool = text.split_once(' ').map(|(t, _)| t).unwrap_or(&text);
                    if !tool.is_empty() {
                        last_call_tool = Some(tool.to_string());
                    }
                }
                "observe" if mem_tick == now => {
                    let tool = last_call_tool.as_deref().unwrap_or("tool");
                    detected = Some(format!("tool:{tool}"));
                }
                "tool_error" if mem_tick == now => {
                    let tool = last_call_tool.as_deref().unwrap_or("tool");
                    detected = Some(format!("tool_error:{tool}"));
                }
                "denied" if mem_tick == now => {
                    let tool = text
                        .split([':', ' '])
                        .next()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("tool");
                    detected = Some(format!("tool_error:{tool}"));
                }
                _ => {}
            }
        }
        detected
    }
}

impl OnyxSystem for MilestoneSnapshotSystem {
    fn name(&self) -> &str {
        "openonyx_milestone_snapshots"
    }

    fn init(&mut self, attrs: &mut dyn AttrRegistry) -> Result<(), OnyxError> {
        self.vocab = Some(Vocab::resolve(attrs));
        Ok(())
    }

    fn on_restore(&mut self, graph: &dyn GraphView) {
        self.prev_states.clear();
        self.prev_settled = false;
        let Some(v) = self.vocab else { return };
        for id in graph.query(&[v.state]) {
            if let Some(Value::Text(s)) = graph.get(id, v.state) {
                if let Some(st) = AgentState::parse(&s) {
                    self.prev_states.insert(id.0, st);
                }
            }
        }
        self.prev_settled =
            !self.prev_states.is_empty() && self.prev_states.values().all(|s| s.is_terminal());
    }

    fn run(&mut self, ctx: &mut Ctx) {
        let Some(v) = self.vocab else { return };
        if ctx
            .inbox
            .iter()
            .any(|e| matches!(e, OnyxEvent::System(SystemEvent::RestoreRequested(_))))
        {
            return;
        }
        let now = ctx
            .inbox
            .iter()
            .find_map(|e| match e {
                OnyxEvent::System(SystemEvent::Tick { n }) => Some(*n),
                _ => None,
            })
            .unwrap_or(0);
        let now_i64 = i64::try_from(now).unwrap_or(i64::MAX);

        let mut cmd_tag: Option<String> = None;
        for event in ctx.inbox {
            if let OnyxEvent::Ingress(IngressEvent::Command(raw)) = event {
                let cmd = raw.trim();
                if cmd == "/undo" {
                    cmd_tag = Some("cmd:/undo".to_string());
                } else if cmd == "/mode" || cmd.starts_with("/mode ") {
                    cmd_tag = Some(format!("cmd:{cmd}"));
                } else if cmd == "/new" || cmd.starts_with("/new ") {
                    cmd_tag = Some("cmd:/new".to_string());
                } else if cmd == "/compact" || cmd.starts_with("/compact ") {
                    cmd_tag = Some("cmd:/compact".to_string());
                } else if cmd == "/services"
                    || cmd == "/service"
                    || cmd.starts_with("/service ")
                    || cmd.starts_with("/services ")
                {
                    cmd_tag = Some("cmd:/service".to_string());
                }
            }
        }

        let mut curr_states = BTreeMap::new();
        let mut approval_tag: Option<String> = None;
        let mut tool_tag: Option<String> = None;

        for id in ctx.graph.query(&[v.state]) {
            if let Some(Value::Text(s)) = ctx.graph.get(id, v.state) {
                if let Some(st) = AgentState::parse(&s) {
                    curr_states.insert(id.0, st);
                    if st == AgentState::WaitingApproval
                        && self.prev_states.get(&id.0) != Some(&AgentState::WaitingApproval)
                    {
                        let tool = match ctx.graph.get(id, v.pending_tool) {
                            Some(Value::Text(t)) if !t.is_empty() => t,
                            _ => "tool".to_string(),
                        };
                        approval_tag = Some(format!("approval:{tool}"));
                    }
                }
            }
            if tool_tag.is_none() {
                tool_tag = self.detect_tool_milestone(&*ctx.graph, v, id, now_i64);
            }
        }

        let curr_settled = !curr_states.is_empty() && curr_states.values().all(|s| s.is_terminal());
        let any_became_terminal = curr_states.iter().any(|(id, st)| {
            st.is_terminal()
                && self
                    .prev_states
                    .get(id)
                    .is_some_and(|prev| !prev.is_terminal())
        });
        let settled_tag = if curr_settled && (!self.prev_settled || any_became_terminal) {
            if curr_states.values().any(|s| *s == AgentState::Failed) {
                Some("failed".to_string())
            } else {
                Some("settled".to_string())
            }
        } else {
            None
        };
        let spawned_new_agent = curr_states
            .keys()
            .any(|id| !self.prev_states.contains_key(id));

        self.prev_states = curr_states;
        self.prev_settled = curr_settled;

        let tag = cmd_tag
            .or(settled_tag)
            .or(approval_tag)
            .or(tool_tag)
            .or_else(|| {
                if now == 0 {
                    Some("init".to_string())
                } else if spawned_new_agent {
                    Some("spawn".to_string())
                } else if self
                    .snapshot_every
                    .is_some_and(|every| every > 0 && now % every == 0)
                {
                    Some("periodic".to_string())
                } else {
                    None
                }
            });

        if let Some(tag) = tag {
            ctx.outbox
                .push(OnyxEvent::System(SystemEvent::SnapshotRequested(format!(
                    "{now}:{tag}"
                ))));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("openonyx-pack-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn pack_store_compresses_verifies_checksum_and_heals_truncated_tail() {
        let dir = temp_dir("compress");
        let payload_a = b"ONYXAG\x01\0"
            .iter()
            .copied()
            .chain(vec![b'A'; 512])
            .collect::<Vec<_>>();
        let payload_b = b"ONYXAG\x01\0"
            .iter()
            .copied()
            .chain(vec![b'B'; 512])
            .collect::<Vec<_>>();

        {
            let mut store = PackSnapshotStore::new(&dir);
            store.save("0:init", &payload_a).unwrap();
            // Dedup: same tick + same checksum + "exit" must not duplicate.
            store.save("0:exit", &payload_a).unwrap();
            store.save("5:tool:fs_edit", &payload_b).unwrap();

            assert_eq!(store.ticks(), vec![0, 5]);
            let meta0 = store.get_meta(0).unwrap();
            assert_eq!(meta0.tag, "init");
            assert!(
                meta0.stored_len < meta0.raw_len,
                "zlib must shrink repetitive payload"
            );
            assert_eq!(store.load("0").unwrap(), payload_a);
            assert_eq!(store.load("5").unwrap(), payload_b);
            assert_eq!(store.resolve_tick_or_floor(3), Some(0));
            assert_eq!(store.resolve_tick_or_floor(5), Some(5));
        }

        // Simulate crash mid-write by appending garbage incomplete header bytes.
        let pack_path = dir.join(PACK_FILE_NAME);
        let original_len = fs::metadata(&pack_path).unwrap().len();
        {
            let mut f = OpenOptions::new().append(true).open(&pack_path).unwrap();
            f.write_all(b"ONYXPK1\x01partial").unwrap();
        }

        let mut recovered = PackSnapshotStore::new(&dir);
        assert_eq!(recovered.ticks(), vec![0, 5]);
        assert!(recovered.corruption_error().is_none());
        assert_eq!(
            recovered.truncated_tail_bytes(),
            b"ONYXPK1\x01partial".len()
        );
        assert_eq!(recovered.load("5").unwrap(), payload_b);

        // Next milestone heals the truncated tail before appending.
        recovered.save("9:settled", &payload_a).unwrap();
        assert_eq!(recovered.truncated_tail_bytes(), 0);
        assert!(fs::metadata(&pack_path).unwrap().len() > original_len);

        let reopened = PackSnapshotStore::new(&dir);
        assert_eq!(reopened.ticks(), vec![0, 5, 9]);
        assert_eq!(reopened.truncated_tail_bytes(), 0);
        assert_eq!(reopened.load("9").unwrap(), payload_a);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pack_store_prunes_max_snapshots_and_reads_legacy_bin_fallback() {
        let dir = temp_dir("legacy-and-prune");
        let legacy_bytes = b"ONYXAG\x01\0legacy-tick-2";
        fs::write(dir.join("tick-2.bin"), legacy_bytes).unwrap();

        let mut store = PackSnapshotStore::new(&dir).with_max_snapshots(Some(2));
        assert_eq!(store.ticks(), vec![2]);
        assert_eq!(store.get_meta(2).unwrap().tag, "legacy");
        assert_eq!(store.load("2").unwrap(), legacy_bytes);

        let frame4 = b"ONYXAG\x01\0tick-4-body-content-000000000000";
        let frame8 = b"ONYXAG\x01\0tick-8-body-content-111111111111";
        store.save("4:spawn", frame4).unwrap();
        store.save("8:settled", frame8).unwrap();

        assert_eq!(store.ticks(), vec![4, 8]);
        assert!(!dir.join("tick-2.bin").exists());

        let reopened = PackSnapshotStore::new(&dir);
        assert_eq!(reopened.ticks(), vec![4, 8]);
        assert_eq!(reopened.load("4").unwrap(), frame4);
        assert_eq!(reopened.load("8").unwrap(), frame8);

        let _ = fs::remove_dir_all(&dir);
    }
}
