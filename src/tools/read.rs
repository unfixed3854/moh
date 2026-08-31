//! Pi-compatible text-file reader with deterministic directory listings.

use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use garde::Validate;
use rig::tool::ToolOutput;
use schemars::JsonSchema;
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::OnceCell;
use xxhash_rust::{xxh32::xxh32, xxh64::xxh64};

use super::anchor_store::{AnchorSnapshot, AnchorStore, AnchorStoreError, moh_state_dir};
use super::blocking::{self, BlockingError};
use super::observations::FileObservations;

const MAX_BYTES: u64 = 100 * 1024 * 1024;
const MAX_LINES: usize = 238_328;
const MAX_RENDERED_ROW_BYTES: usize = 204_800;
const DEFAULT_LIMIT: u64 = 2_000;
const ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
const HASH_SPACE: u32 = 62 * 62 * 62;
const PROBE_STRIDE: u32 = 62 * 62 + 62 + 1;
const HASH_SEPARATOR: char = '│';

#[derive(Debug, Deserialize, JsonSchema, Validate)]
/// Arguments accepted by the text-only `read` tool.
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// Cwd-relative or absolute file or directory path.
    #[garde(length(min = 1))]
    pub path: String,
    /// One-indexed first logical line to display.
    #[garde(range(min = 1))]
    pub offset: Option<u64>,
    /// Maximum number of logical lines to display.
    #[garde(range(min = 1))]
    pub limit: Option<u64>,
}

impl ReadArgs {
    /// Builds arguments for a path-only read request.
    pub fn path(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            offset: None,
            limit: None,
        }
    }
}

/// Failure returned to Rig as the text reader's model-visible tool error.
#[derive(Debug, Error)]
pub enum ReadToolError {
    /// The call did not supply a non-empty `path` or positive paging arguments.
    #[error("[E_INVALID_ARGUMENT] {0}")]
    InvalidArgument(&'static str),
    /// The requested path does not exist.
    #[error("[E_NOT_FOUND] file not found")]
    NotFound,
    /// The requested path could not be accessed safely.
    #[error("[E_ACCESS] file could not be accessed")]
    Access,
    /// The target is not a regular text file supported by this reader.
    #[error("[E_NOT_TEXT] hashline reading only supports regular text files")]
    NotText,
    /// The target exceeds the byte or line capacity of three-character anchors.
    #[error("[E_FILE_TOO_LARGE] file exceeds the text-reader size limit")]
    FileTooLarge,
    /// Durable anchor storage was unavailable, so no unstable anchor output was returned.
    #[error("[E_STORE] durable anchor storage is unavailable")]
    Store,
    /// Tokio could not complete the blocking worker that collected the read payload.
    #[error("[E_RUNTIME] read tool worker failed")]
    Worker(#[source] tokio::task::JoinError),
}

/// Configuration for a text reader's durable anchor store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadConfig {
    /// SQLite path used for durable text-file anchors.
    pub anchor_store_path: PathBuf,
}

impl ReadConfig {
    /// Resolves the platform-default SQLite path for durable anchors.
    pub fn platform_default() -> Result<Self, ReadToolError> {
        Ok(Self::at(
            moh_state_dir()
                .map_err(|_| ReadToolError::Store)?
                .join("hash-store.sqlite"),
        ))
    }

    /// Uses an explicit SQLite path for durable anchors.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            anchor_store_path: path.into(),
        }
    }
}

/// Creates read services that share lazily initialized durable anchor storage.
#[derive(Clone)]
pub struct ReadServiceFactory {
    config: ReadConfig,
    store: Arc<OnceCell<Result<AnchorStore, AnchorStoreError>>>,
    observations: FileObservations,
}

impl ReadServiceFactory {
    /// Creates a factory with explicit durable-anchor configuration.
    pub fn new(config: ReadConfig) -> Self {
        Self {
            config,
            store: Arc::new(OnceCell::new()),
            observations: FileObservations::default(),
        }
    }

    /// Creates a session sharing durable anchors while starting with empty file observations.
    pub fn isolated_session(&self) -> Self {
        Self {
            config: self.config.clone(),
            store: Arc::clone(&self.store),
            observations: FileObservations::default(),
        }
    }

    pub(super) fn observations(&self) -> FileObservations {
        self.observations.clone()
    }

    /// Binds a reader to the cwd supplied for one agent run.
    pub fn for_cwd(&self, cwd: PathBuf) -> ReadService {
        ReadService {
            cwd,
            config: self.config.clone(),
            store: Arc::clone(&self.store),
            observations: self.observations.clone(),
        }
    }
}

/// Async, cwd-bound Pi-compatible text-file reader.
#[derive(Clone)]
pub struct ReadService {
    cwd: PathBuf,
    config: ReadConfig,
    store: Arc<OnceCell<Result<AnchorStore, AnchorStoreError>>>,
    observations: FileObservations,
}

impl ReadService {
    /// Returns the durable anchor snapshot for a canonical path, if present.
    pub(crate) async fn stored_snapshot(
        &self,
        canonical_path: PathBuf,
    ) -> Result<Option<AnchorSnapshot>, ReadToolError> {
        self.store()
            .await?
            .load(&canonical_path)
            .await
            .map_err(|_| ReadToolError::Store)
    }

    /// Returns the model-facing read-tool description.
    pub fn description() -> &'static str {
        "Read one text file or list one directory. File rows are HASH│content anchors; preserve those three-character hashes when referring to a line. Directory rows list direct children, with / marking child directories."
    }

    /// Reads a text file or lists a directory under this service's run cwd.
    pub async fn read(&self, args: ReadArgs) -> Result<ToolOutput, ReadToolError> {
        args.validate()
            .map_err(|_| ReadToolError::InvalidArgument("invalid read arguments"))?;
        let cwd = self.cwd.clone();
        let payload = blocking::run(move || collect_read_payload(cwd, args))
            .await
            .map_err(Self::from_blocking)?;
        match payload {
            ReadPayload::Directory { text } => Ok(ToolOutput::text(text)),
            ReadPayload::File(file) => {
                let hashes = self
                    .hashes_for(&file.canonical_path, &file.checksum, &file.lines)
                    .await?;
                self.observations
                    .record(&file.requested_path, file.canonical_path, &file.bytes)
                    .map_err(|_| ReadToolError::Store)?;
                Ok(ToolOutput::text(format_output(
                    &file.lines,
                    &hashes,
                    file.offset,
                    file.limit,
                    file.had_utf8_decode_errors,
                )))
            }
        }
    }

    async fn hashes_for(
        &self,
        canonical_path: &Path,
        checksum: &str,
        lines: &[String],
    ) -> Result<Vec<String>, ReadToolError> {
        let store = self.store().await?;
        let stored = store
            .load(canonical_path)
            .await
            .map_err(|_| ReadToolError::Store)?;
        let hashes = hashes_from_snapshot(stored, checksum, lines);
        let snapshot = snapshot(checksum, lines, &hashes);
        store
            .save(canonical_path, &snapshot)
            .await
            .map_err(|_| ReadToolError::Store)?;
        Ok(hashes)
    }

    async fn store(&self) -> Result<&AnchorStore, ReadToolError> {
        let path = self.config.anchor_store_path.clone();
        self.store
            .get_or_init(|| async move { AnchorStore::open_at(&path).await })
            .await
            .as_ref()
            .map_err(|_| ReadToolError::Store)
    }

    fn from_blocking(error: BlockingError<ReadToolError>) -> ReadToolError {
        match error {
            BlockingError::Operation(error) => error,
            BlockingError::Worker(source) => ReadToolError::Worker(source),
        }
    }
}

enum ReadPayload {
    Directory { text: String },
    File(ReadFilePayload),
}

struct ReadFilePayload {
    requested_path: PathBuf,
    canonical_path: PathBuf,
    bytes: Vec<u8>,
    lines: Vec<String>,
    checksum: String,
    offset: u64,
    limit: u64,
    had_utf8_decode_errors: bool,
}

fn collect_read_payload(cwd: PathBuf, args: ReadArgs) -> Result<ReadPayload, ReadToolError> {
    let requested = PathBuf::from(args.path);
    let requested_path = if requested.is_absolute() {
        requested
    } else {
        cwd.join(requested)
    };
    let offset = args.offset.unwrap_or(1);
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
    let canonical_path = canonicalize_path(&requested_path)?;
    if fs::metadata(&canonical_path)
        .map_err(map_io_error)?
        .is_dir()
    {
        return Ok(ReadPayload::Directory {
            text: list_directory(&canonical_path, offset, limit)?,
        });
    }
    let bytes = read_text_bytes(&canonical_path)?;
    let (normalized, had_utf8_decode_errors) = normalize_text(&bytes);
    let lines = logical_lines(&normalized);
    if lines.len() > MAX_LINES {
        return Err(ReadToolError::FileTooLarge);
    }
    Ok(ReadPayload::File(ReadFilePayload {
        requested_path,
        canonical_path,
        bytes,
        checksum: format!("{:016x}", xxh64(normalized.as_bytes(), 0)),
        lines,
        offset,
        limit,
        had_utf8_decode_errors,
    }))
}

fn hashes_from_snapshot(
    stored: Option<AnchorSnapshot>,
    checksum: &str,
    lines: &[String],
) -> Vec<String> {
    match stored {
        Some(snapshot) if snapshot.checksum == checksum && snapshot.line_count == lines.len() => {
            snapshot.hashes
        }
        Some(snapshot)
            if snapshot.lines.len() == snapshot.line_count
                && snapshot.hashes.len() == snapshot.line_count =>
        {
            stable_hashes(&snapshot, lines)
        }
        _ => allocate_hashes(lines, &[]),
    }
}

fn snapshot(checksum: &str, lines: &[String], hashes: &[String]) -> AnchorSnapshot {
    AnchorSnapshot {
        checksum: checksum.to_owned(),
        line_count: lines.len(),
        hashes: hashes.to_owned(),
        lines: lines
            .iter()
            .map(|line| canonical_line(line).to_owned())
            .collect(),
    }
}

fn list_directory(path: &Path, offset: u64, limit: u64) -> Result<String, ReadToolError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(map_io_error)? {
        let entry = entry.map_err(map_io_error)?;
        let file_type = entry.file_type().map_err(map_io_error)?;
        entries.push((
            entry.file_name().to_string_lossy().into_owned(),
            file_type.is_dir(),
        ));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let names = entries
        .into_iter()
        .map(|(mut name, is_directory)| {
            if is_directory {
                name.push('/');
            }
            name
        })
        .collect::<Vec<_>>();
    Ok(format_directory_output(&names, offset, limit))
}

fn format_directory_output(entries: &[String], offset: u64, limit: u64) -> String {
    if entries.is_empty() {
        return "[Directory is empty.]".to_owned();
    }

    let total_entries = entries.len();
    let start = usize::try_from(offset.saturating_sub(1)).unwrap_or(usize::MAX);
    if start >= total_entries {
        return format!(
            "Offset {offset} is beyond end of directory ({total_entries} entries total). Use offset=1 to list from the start, or offset={total_entries} to list the last entry."
        );
    }

    let end = start
        .saturating_add(usize::try_from(limit).unwrap_or(usize::MAX))
        .min(total_entries);
    let mut result = entries[start..end].join("\n");
    if end < total_entries {
        result.push_str(&format!(
            "\n[Showing entries {}-{} of {total_entries}. Use offset={} to continue.]",
            start + 1,
            end,
            end + 1
        ));
    }
    result
}

fn canonicalize_path(path: &Path) -> Result<PathBuf, ReadToolError> {
    fs::canonicalize(path).map_err(map_io_error)
}

fn read_text_bytes(path: &Path) -> Result<Vec<u8>, ReadToolError> {
    let metadata = fs::metadata(path).map_err(map_io_error)?;
    if !metadata.is_file() {
        return Err(ReadToolError::NotText);
    }
    if metadata.len() > MAX_BYTES {
        return Err(ReadToolError::FileTooLarge);
    }

    let file = fs::File::open(path).map_err(map_io_error)?;
    let opened_metadata = file.metadata().map_err(map_io_error)?;
    if !opened_metadata.is_file() {
        return Err(ReadToolError::NotText);
    }
    let bytes = read_bounded(file, opened_metadata.len(), MAX_BYTES)?;
    if bytes.contains(&0)
        || has_utf16_or_utf32_bom(&bytes)
        || (std::str::from_utf8(&bytes).is_err() && is_binary_image(&bytes))
    {
        return Err(ReadToolError::NotText);
    }
    Ok(bytes)
}

fn read_bounded(
    mut reader: impl Read,
    reported_size: u64,
    max_bytes: u64,
) -> Result<Vec<u8>, ReadToolError> {
    if reported_size > max_bytes {
        return Err(ReadToolError::FileTooLarge);
    }

    let max_len = usize::try_from(max_bytes).map_err(|_| ReadToolError::FileTooLarge)?;
    let sentinel_limit = max_len.checked_add(1).ok_or(ReadToolError::FileTooLarge)?;
    let initial_capacity =
        usize::try_from(reported_size).map_err(|_| ReadToolError::FileTooLarge)?;
    let mut bytes = Vec::with_capacity(initial_capacity);
    let mut chunk = [0_u8; 64 * 1024];
    while bytes.len() < sentinel_limit {
        let remaining = sentinel_limit - bytes.len();
        let chunk_limit = remaining.min(chunk.len());
        let bytes_read = reader
            .read(&mut chunk[..chunk_limit])
            .map_err(map_io_error)?;
        if bytes_read == 0 {
            break;
        }
        let required_capacity = bytes.len() + bytes_read;
        if bytes.capacity() < required_capacity {
            let growth_capacity = bytes
                .capacity()
                .max(chunk.len())
                .saturating_mul(2)
                .min(sentinel_limit);
            let target_capacity = required_capacity.max(growth_capacity);
            bytes.reserve_exact(target_capacity - bytes.len());
        }
        bytes.extend_from_slice(&chunk[..bytes_read]);
    }
    if bytes.len() > max_len {
        return Err(ReadToolError::FileTooLarge);
    }
    Ok(bytes)
}

fn map_io_error(error: std::io::Error) -> ReadToolError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ReadToolError::NotFound,
        std::io::ErrorKind::PermissionDenied => ReadToolError::Access,
        _ => ReadToolError::Access,
    }
}

fn has_utf16_or_utf32_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xff, 0xfe, 0x00, 0x00])
        || bytes.starts_with(&[0x00, 0x00, 0xfe, 0xff])
        || bytes.starts_with(&[0xff, 0xfe])
        || bytes.starts_with(&[0xfe, 0xff])
}

fn is_binary_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || (bytes.len() >= 13 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")))
        || (bytes.len() >= 16 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
        || (bytes.len() >= 18
            && bytes.starts_with(b"BM")
            && matches!(
                u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]),
                12 | 40 | 52 | 56 | 108 | 124
            ))
}

pub(crate) fn normalize_text(bytes: &[u8]) -> (String, bool) {
    let decoded = String::from_utf8_lossy(bytes);
    let had_utf8_decode_errors = matches!(decoded, Cow::Owned(_));
    let text = decoded.strip_prefix('\u{feff}').unwrap_or(&decoded);
    (
        text.replace("\r\n", "\n").replace('\r', "\n"),
        had_utf8_decode_errors,
    )
}

pub(crate) fn logical_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        vec![String::new()]
    } else {
        text.strip_suffix('\n')
            .unwrap_or(text)
            .split('\n')
            .map(ToOwned::to_owned)
            .collect()
    }
}

fn format_output(
    lines: &[String],
    hashes: &[String],
    offset: u64,
    limit: u64,
    had_utf8_decode_errors: bool,
) -> String {
    let total_lines = lines.len();
    let start = usize::try_from(offset.saturating_sub(1)).unwrap_or(usize::MAX);
    if start >= total_lines {
        let mut result = if total_lines == 1 && lines[0].is_empty() {
            format!(
                "Offset {offset} is beyond end of file (0 lines total). The file is empty. Use replace to insert content."
            )
        } else {
            format!(
                "Offset {offset} is beyond end of file ({total_lines} lines total). Use offset=1 to read from the start, or offset={total_lines} to read the last line."
            )
        };
        append_lossy_warning(&mut result, had_utf8_decode_errors);
        return result;
    }

    if total_lines == 1 && lines[0].is_empty() && offset == 1 {
        let mut result = format!(
            "{}{}\n[File is empty. Use replace to insert content.]",
            hashes[0], HASH_SEPARATOR
        );
        append_lossy_warning(&mut result, had_utf8_decode_errors);
        return result;
    }

    let end = start
        .saturating_add(usize::try_from(limit).unwrap_or(usize::MAX))
        .min(total_lines);
    let mut rows = Vec::with_capacity(end - start + 2);
    for index in start..end {
        let row = format!("{}{}{}", hashes[index], HASH_SEPARATOR, lines[index]);
        if row.len() > MAX_RENDERED_ROW_BYTES {
            rows.push(format!(
                "[Line {} is {} bytes, exceeds 204800 bytes; content not shown. Use bash: sed -n '{}p' {} | head -c 204800]",
                index + 1,
                row.len(),
                index + 1,
                "<path>",
            ));
        } else {
            rows.push(row);
        }
    }
    let mut result = rows.join("\n");
    if end < total_lines {
        result.push_str(&format!(
            "\n[Showing lines {}-{} of {total_lines}. Use offset={} to continue.]",
            start + 1,
            end,
            end + 1
        ));
    }
    append_lossy_warning(&mut result, had_utf8_decode_errors);
    result
}

fn append_lossy_warning(result: &mut String, had_utf8_decode_errors: bool) {
    if had_utf8_decode_errors {
        result.push_str("\n[Non-UTF-8 bytes shown as U+FFFD; editing rewrites the file as UTF-8.]");
    }
}

fn stable_hashes(previous: &AnchorSnapshot, new_lines: &[String]) -> Vec<String> {
    let mut new_hashes = vec![None; new_lines.len()];
    let mut old_by_content = HashMap::<String, Vec<usize>>::new();
    for (index, line) in previous.lines.iter().enumerate() {
        old_by_content
            .entry(canonical_line(line).to_owned())
            .or_default()
            .push(index);
    }
    let mut new_by_content = HashMap::<String, Vec<usize>>::new();
    for (index, line) in new_lines.iter().enumerate() {
        new_by_content
            .entry(canonical_line(line).to_owned())
            .or_default()
            .push(index);
    }

    let mut occupied = vec![false; HASH_SPACE as usize];
    for hash in &previous.hashes {
        if let Some(slot) = hash_slot(hash) {
            occupied[slot as usize] = true;
        }
    }
    let mut preserved_old = vec![false; previous.lines.len()];
    for (content, new_positions) in new_by_content {
        let Some(old_positions) = old_by_content.get(&content) else {
            continue;
        };
        for (old_index, new_index) in nearest_occurrence_pairs(old_positions, &new_positions) {
            new_hashes[new_index] = Some(previous.hashes[old_index].clone());
            preserved_old[old_index] = true;
        }
    }

    let mut removed_by_content = HashMap::<String, VecDeque<String>>::new();
    for (index, line) in previous.lines.iter().enumerate() {
        if !preserved_old[index] {
            removed_by_content
                .entry(canonical_line(line).to_owned())
                .or_default()
                .push_back(previous.hashes[index].clone());
        }
    }
    for (index, hash) in new_hashes.iter_mut().enumerate() {
        if hash.is_none() {
            let key = canonical_line(&new_lines[index]);
            if let Some(removed) = removed_by_content
                .get_mut(key)
                .and_then(VecDeque::pop_front)
            {
                *hash = Some(removed);
            }
        }
    }

    let mut result = Vec::with_capacity(new_lines.len());
    for (index, hash) in new_hashes.into_iter().enumerate() {
        result.push(hash.unwrap_or_else(|| allocate_one(&new_lines[index], &mut occupied)));
    }
    result
}

fn nearest_occurrence_pairs(old: &[usize], new: &[usize]) -> Vec<(usize, usize)> {
    // This mirrors pi-hashline-edit-pro: walk prior occurrences in source order and
    // consume the nearest still-unmatched new occurrence. A BTreeSet keeps lookup and
    // removal `O(log n)`, yielding `O(n log n)` time and `O(n)` memory even when all
    // 238,328 logical lines have identical canonical content.
    let mut remaining = new.iter().copied().collect::<BTreeSet<_>>();
    let mut pairs = Vec::with_capacity(old.len().min(new.len()));
    for old_index in old.iter().copied() {
        let right = remaining.range(old_index..).next().copied();
        let left = remaining.range(..old_index).next_back().copied();
        let Some(new_index) = (match (left, right) {
            (Some(left), Some(right)) if old_index.abs_diff(left) <= right.abs_diff(old_index) => {
                Some(left)
            }
            (_, Some(right)) => Some(right),
            (Some(left), None) => Some(left),
            (None, None) => None,
        }) else {
            break;
        };
        remaining.remove(&new_index);
        pairs.push((old_index, new_index));
    }
    pairs
}

fn allocate_hashes(lines: &[String], prior: &[String]) -> Vec<String> {
    let mut occupied = vec![false; HASH_SPACE as usize];
    for hash in prior {
        if let Some(slot) = hash_slot(hash) {
            occupied[slot as usize] = true;
        }
    }
    lines
        .iter()
        .map(|line| allocate_one(line, &mut occupied))
        .collect()
}

fn allocate_one(line: &str, occupied: &mut [bool]) -> String {
    let mut slot = base_slot(line);
    for _ in 0..HASH_SPACE {
        if !occupied[slot as usize] {
            occupied[slot as usize] = true;
            return slot_hash(slot);
        }
        slot = (slot + PROBE_STRIDE) % HASH_SPACE;
    }
    unreachable!("line count is bounded by the hash space")
}

fn canonical_line(line: &str) -> &str {
    line.trim_end()
}

fn base_slot(line: &str) -> u32 {
    (xxh32(canonical_line(line).as_bytes(), 0) >> 14) % HASH_SPACE
}

fn slot_hash(slot: u32) -> String {
    let high = (slot / (62 * 62)) as usize;
    let middle = ((slot / 62) % 62) as usize;
    let low = (slot % 62) as usize;
    String::from_utf8(vec![ALPHABET[high], ALPHABET[middle], ALPHABET[low]])
        .expect("the anchor alphabet is ASCII")
}

fn hash_slot(hash: &str) -> Option<u32> {
    if hash.len() != 3 {
        return None;
    }
    hash.bytes().try_fold(0_u32, |slot, byte| {
        ALPHABET
            .iter()
            .position(|candidate| *candidate == byte)
            .map(|index| slot * 62 + index as u32)
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{ReadToolError, nearest_occurrence_pairs, read_bounded};

    #[test]
    fn bounded_reader_stops_at_one_byte_past_a_stale_small_size_hint() {
        let mut growing_source = Cursor::new(vec![b'x'; 64]);

        let error = read_bounded(&mut growing_source, 4, 8).unwrap_err();

        assert!(matches!(error, ReadToolError::FileTooLarge));
        assert_eq!(growing_source.position(), 9);
    }

    #[test]
    fn source_order_claims_the_nearest_remaining_duplicate() {
        assert_eq!(
            nearest_occurrence_pairs(&[0, 10, 100], &[9, 10]),
            vec![(0, 9), (10, 10)]
        );
    }
}
