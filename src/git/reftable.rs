// Reftable block decoding adapted from `sley-formats` (Apache-2.0):
// https://github.com/HeddleCo/sley
use crate::error::GitAiError;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const REFTABLE_MAGIC: &[u8; 4] = b"REFT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReftableVersion {
    V1,
    V2,
}

impl ReftableVersion {
    fn header_len(self) -> usize {
        match self {
            Self::V1 => 24,
            Self::V2 => 28,
        }
    }

    fn footer_len(self) -> usize {
        match self {
            Self::V1 => 68,
            Self::V2 => 72,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReftableHeader {
    version: ReftableVersion,
    block_size: u32,
    min_update_index: u64,
    max_update_index: u64,
    oid_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedLogValue {
    Deletion,
    DeleteLog,
    Update(ReftableLogEntry),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedLogRecord {
    reference: String,
    update_index: u64,
    value: ParsedLogValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReftableRefValue {
    Deletion,
    Direct(String),
    Symbolic(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReftableRefEntry {
    name: String,
    value: ReftableRefValue,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedTable {
    refs: Vec<ReftableRefEntry>,
    logs: Vec<ParsedLogRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReftableLogEntry {
    pub reference: String,
    pub update_index: u64,
    pub old_oid: String,
    pub new_oid: String,
    pub message: String,
    pub timestamp_secs: i64,
}

#[derive(Debug, Default)]
pub(crate) struct ReftableReader {
    table_bytes: HashMap<PathBuf, Vec<u8>>,
    parsed_refs: HashMap<(std::path::PathBuf, String), Option<ReftableRefValue>>,
    parsed_logs: HashMap<std::path::PathBuf, Vec<ParsedLogRecord>>,
    unreadable_log_tables: HashSet<std::path::PathBuf>,
    log_snapshots: HashMap<PathBuf, ReftableLogSnapshot>,
    #[cfg(test)]
    table_read_count: usize,
    #[cfg(test)]
    stack_read_count: usize,
}

#[derive(Debug)]
struct ReftableLogSnapshot {
    active_paths: Vec<PathBuf>,
    logs: Vec<ReftableLogEntry>,
}

impl ReftableReader {
    #[cfg(test)]
    pub(crate) fn read_logs(
        &mut self,
        stack_dir: &Path,
    ) -> Result<Vec<ReftableLogEntry>, GitAiError> {
        if let Some(logs) = self.read_logs_if_changed(stack_dir)? {
            return Ok(logs);
        }
        Ok(self
            .log_snapshots
            .get(stack_dir)
            .map(|snapshot| snapshot.logs.clone())
            .unwrap_or_default())
    }

    pub(crate) fn read_logs_if_changed(
        &mut self,
        stack_dir: &Path,
    ) -> Result<Option<Vec<ReftableLogEntry>>, GitAiError> {
        for attempt in 0..2 {
            let active_paths = self.active_table_paths(stack_dir)?;
            if self
                .log_snapshots
                .get(stack_dir)
                .is_some_and(|snapshot| snapshot.active_paths == active_paths)
            {
                return Ok(None);
            }
            let mut visible = BTreeMap::<(String, u64), ReftableLogEntry>::new();
            let mut retry_stack = false;
            let mut snapshot_complete = true;
            for table_path in &active_paths {
                if self.unreadable_log_tables.contains(table_path) {
                    continue;
                }
                let records = match self.parsed_logs(table_path) {
                    Ok(records) => records,
                    Err(error) if attempt == 0 && is_not_found(&error) => {
                        retry_stack = true;
                        break;
                    }
                    Err(error) => {
                        if is_invalid_reftable(&error) {
                            self.unreadable_log_tables.insert(table_path.clone());
                        } else {
                            // Reftable files are immutable, so parser failures are
                            // deterministic. I/O failures can be transient and must
                            // be retried by the next command rather than poisoning
                            // this repo family's reader for its whole lifetime.
                            snapshot_complete = false;
                        }
                        tracing::warn!(
                            path = %table_path.display(),
                            error = %error,
                            "skipping unreadable reftable log table"
                        );
                        continue;
                    }
                };
                // Log keys sort by ref name and descending update index. Apply each
                // ref's records oldest-to-newest so a delete-log marker only removes
                // history that predates it, not later entries in the same table.
                for record in records.iter().rev() {
                    let key = (record.reference.clone(), record.update_index);
                    match &record.value {
                        ParsedLogValue::Deletion => {
                            visible.remove(&key);
                        }
                        ParsedLogValue::DeleteLog => {
                            visible.retain(|(reference, _), _| reference != &record.reference);
                        }
                        ParsedLogValue::Update(entry) => {
                            visible.insert(key, entry.clone());
                        }
                    }
                }
            }
            if retry_stack {
                continue;
            }
            if !snapshot_complete {
                // A transient failure means `visible` is only a partial view.
                // Keep the last complete snapshot authoritative and retry the
                // changed stack on the next command.
                return Ok(None);
            }
            self.prune_stack_cache(stack_dir, &active_paths);
            let mut logs = visible.into_values().collect::<Vec<_>>();
            logs.sort_by(|left, right| {
                left.update_index
                    .cmp(&right.update_index)
                    .then_with(|| left.reference.cmp(&right.reference))
            });
            self.log_snapshots.insert(
                stack_dir.to_path_buf(),
                ReftableLogSnapshot {
                    active_paths,
                    logs: logs.clone(),
                },
            );
            return Ok(Some(logs));
        }
        Ok(Some(Vec::new()))
    }

    pub(crate) fn cached_logs(&self, stack_dir: &Path) -> Option<&[ReftableLogEntry]> {
        self.log_snapshots
            .get(stack_dir)
            .map(|snapshot| snapshot.logs.as_slice())
    }

    pub(crate) fn reset_log_cache(&mut self) {
        self.table_bytes.clear();
        self.parsed_refs.clear();
        self.parsed_logs.clear();
        self.unreadable_log_tables.clear();
        self.log_snapshots.clear();
    }

    fn read_ref_at_snapshot(
        &mut self,
        active_paths: &[PathBuf],
        reference: &str,
    ) -> Result<Option<ReftableRefValue>, GitAiError> {
        let mut value = None;
        for table_path in active_paths {
            match self.parsed_ref(table_path, reference)? {
                Some(ReftableRefValue::Deletion) => value = None,
                Some(entry) => value = Some(entry),
                None => {}
            }
        }
        Ok(value)
    }

    fn parsed_ref(
        &mut self,
        table_path: &Path,
        reference: &str,
    ) -> Result<Option<ReftableRefValue>, GitAiError> {
        let key = (table_path.to_path_buf(), reference.to_string());
        if !self.parsed_refs.contains_key(&key) {
            let value =
                parse_table_ref(self.table_bytes(table_path)?, reference)?.map(|entry| entry.value);
            self.parsed_refs.insert(key.clone(), value);
        }
        Ok(self
            .parsed_refs
            .get(&key)
            .expect("cached reftable ref must be present")
            .clone())
    }

    fn parsed_logs(&mut self, table_path: &Path) -> Result<&Vec<ParsedLogRecord>, GitAiError> {
        if !self.parsed_logs.contains_key(table_path) {
            let logs = parse_table_logs(self.table_bytes(table_path)?)?;
            self.parsed_logs.insert(table_path.to_path_buf(), logs);
        }
        Ok(self
            .parsed_logs
            .get(table_path)
            .expect("cached reftable logs must be present"))
    }

    fn table_bytes(&mut self, table_path: &Path) -> Result<&[u8], GitAiError> {
        if !self.table_bytes.contains_key(table_path) {
            let bytes = self.read_table(table_path)?;
            self.table_bytes.insert(table_path.to_path_buf(), bytes);
        }
        Ok(self
            .table_bytes
            .get(table_path)
            .expect("cached reftable bytes must be present"))
    }

    fn read_table(&mut self, table_path: &Path) -> Result<Vec<u8>, GitAiError> {
        #[cfg(test)]
        {
            self.table_read_count += 1;
        }
        Ok(fs::read(table_path)?)
    }

    fn active_table_paths(&mut self, stack_dir: &Path) -> Result<Vec<PathBuf>, GitAiError> {
        #[cfg(test)]
        {
            self.stack_read_count += 1;
        }
        active_table_paths(stack_dir)
    }

    fn prune_stack_cache(&mut self, stack_dir: &Path, active_paths: &[std::path::PathBuf]) {
        let active_paths = active_paths.iter().cloned().collect::<HashSet<_>>();
        self.table_bytes
            .retain(|path, _| !path.starts_with(stack_dir) || active_paths.contains(path));
        self.parsed_refs
            .retain(|(path, _), _| !path.starts_with(stack_dir) || active_paths.contains(path));
        self.parsed_logs
            .retain(|path, _| !path.starts_with(stack_dir) || active_paths.contains(path));
        self.unreadable_log_tables
            .retain(|path| !path.starts_with(stack_dir) || active_paths.contains(path));
        self.log_snapshots
            .retain(|path, _| path != stack_dir || !active_paths.is_empty());
    }

    pub(crate) fn read_head(
        &mut self,
        common_stack: &Path,
        worktree_stack: &Path,
    ) -> Result<Option<(String, Option<String>)>, GitAiError> {
        for attempt in 0..2 {
            let common_paths = self.active_table_paths(common_stack)?;
            let worktree_paths = if worktree_stack == common_stack {
                None
            } else {
                Some(self.active_table_paths(worktree_stack)?)
            };
            let result = (|| {
                let mut head = self.read_ref_at_snapshot(&common_paths, "HEAD")?;
                if let Some(worktree_paths) = &worktree_paths
                    && let Some(worktree_head) =
                        self.read_ref_at_snapshot(worktree_paths, "HEAD")?
                {
                    head = Some(worktree_head);
                }
                match head {
                    Some(ReftableRefValue::Direct(oid)) => Ok(Some((oid, None))),
                    Some(ReftableRefValue::Symbolic(target)) => {
                        let Some(ReftableRefValue::Direct(oid)) =
                            self.read_ref_at_snapshot(&common_paths, &target)?
                        else {
                            return Ok(None);
                        };
                        Ok(Some((oid, Some(target))))
                    }
                    _ => Ok(None),
                }
            })();
            match result {
                Ok(head) => {
                    self.prune_stack_cache(common_stack, &common_paths);
                    if let Some(worktree_paths) = &worktree_paths {
                        self.prune_stack_cache(worktree_stack, worktree_paths);
                    }
                    return Ok(head);
                }
                Err(error) if attempt == 0 && is_not_found(&error) => continue,
                // A table disappearing means compaction raced this snapshot.
                // Fail closed if a complete replacement snapshot cannot be read.
                Err(error) if is_not_found(&error) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }
}

fn is_not_found(error: &GitAiError) -> bool {
    matches!(error, GitAiError::IoError(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn is_invalid_reftable(error: &GitAiError) -> bool {
    matches!(error, GitAiError::Generic(message) if message.starts_with("invalid reftable:"))
}

fn active_table_paths(stack_dir: &Path) -> Result<Vec<std::path::PathBuf>, GitAiError> {
    let table_names = match fs::read_to_string(stack_dir.join("tables.list")) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    table_names
        .lines()
        .filter(|line| !line.is_empty())
        .map(|table_name| {
            if Path::new(table_name)
                .file_name()
                .and_then(|name| name.to_str())
                != Some(table_name)
            {
                return Err(invalid_reftable("invalid table name in tables.list"));
            }
            Ok(stack_dir.join(table_name))
        })
        .collect()
}

pub(crate) fn reftable_stack_update_index(stack_dir: &Path) -> Result<Option<u64>, GitAiError> {
    let table_names = match fs::read_to_string(stack_dir.join("tables.list")) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    table_names
        .lines()
        .filter(|line| !line.is_empty())
        .map(|table_name| {
            let mut parts = table_name.split('-');
            let _min = parts.next();
            let max = parts
                .next()
                .and_then(|value| value.strip_prefix("0x"))
                .ok_or_else(|| invalid_reftable("invalid table name in tables.list"))?;
            u64::from_str_radix(max, 16).map_err(|_| invalid_reftable("invalid table update index"))
        })
        .try_fold(None, |maximum, index| {
            let index = index?;
            Ok(Some(
                maximum.map_or(index, |current: u64| current.max(index)),
            ))
        })
}

#[cfg(test)]
pub(crate) fn read_reftable_logs(stack_dir: &Path) -> Result<Vec<ReftableLogEntry>, GitAiError> {
    ReftableReader::default().read_logs(stack_dir)
}

#[cfg(test)]
fn parse_table(bytes: &[u8]) -> Result<ParsedTable, GitAiError> {
    Ok(ParsedTable {
        refs: parse_table_refs(bytes)?,
        logs: parse_table_logs(bytes)?,
    })
}

#[derive(Debug, Clone, Copy)]
struct TableLayout {
    header: ReftableHeader,
    ref_end: usize,
    log_section: Option<(usize, usize)>,
}

fn parse_table_layout(bytes: &[u8]) -> Result<TableLayout, GitAiError> {
    let header = parse_header(bytes)?;
    let footer_start = bytes
        .len()
        .checked_sub(header.version.footer_len())
        .ok_or_else(|| invalid_reftable("truncated footer"))?;
    let footer = parse_header(&bytes[footer_start..])?;
    if footer != header {
        return Err(invalid_reftable("footer header does not match file header"));
    }
    let expected_crc = read_u32(bytes, bytes.len() - 4)?;
    let actual_crc = crc32(&bytes[footer_start..bytes.len() - 4]);
    if actual_crc != expected_crc {
        return Err(invalid_reftable("footer CRC mismatch"));
    }

    let mut footer_offset = footer_start + header.version.header_len();
    let ref_index_position = read_u64(bytes, footer_offset)? as usize;
    footer_offset += 8;
    let object_position = (read_u64(bytes, footer_offset)? >> 5) as usize;
    footer_offset += 8;
    let object_index_position = read_u64(bytes, footer_offset)? as usize;
    footer_offset += 8;
    let footer_log_position = read_u64(bytes, footer_offset)? as usize;
    footer_offset += 8;
    let log_index_position = read_u64(bytes, footer_offset)? as usize;
    let ref_end = [
        ref_index_position,
        object_position,
        object_index_position,
        footer_log_position,
        log_index_position,
        footer_start,
    ]
    .into_iter()
    .filter(|position| *position != 0)
    .min()
    .unwrap_or(footer_start);
    let log_position = if footer_log_position != 0 {
        Some(footer_log_position)
    } else if bytes.get(header.version.header_len()) == Some(&b'g') {
        Some(header.version.header_len())
    } else {
        None
    };
    let log_end = if log_index_position == 0 {
        footer_start
    } else {
        log_index_position.min(footer_start)
    };
    if log_position.is_some_and(|position| position >= log_end) {
        return Err(invalid_reftable("log section position is out of bounds"));
    }
    Ok(TableLayout {
        header,
        ref_end,
        log_section: log_position.map(|position| (position, log_end)),
    })
}

#[cfg(test)]
fn parse_table_refs(bytes: &[u8]) -> Result<Vec<ReftableRefEntry>, GitAiError> {
    let layout = parse_table_layout(bytes)?;
    parse_ref_section(bytes, layout.header, layout.ref_end, None)
}

fn parse_table_ref(bytes: &[u8], reference: &str) -> Result<Option<ReftableRefEntry>, GitAiError> {
    let layout = parse_table_layout(bytes)?;
    Ok(parse_ref_section(bytes, layout.header, layout.ref_end, Some(reference))?.pop())
}

fn parse_table_logs(bytes: &[u8]) -> Result<Vec<ParsedLogRecord>, GitAiError> {
    let layout = parse_table_layout(bytes)?;
    let Some((log_position, log_end)) = layout.log_section else {
        return Ok(Vec::new());
    };

    let mut records = Vec::new();
    let mut offset = log_position;
    while offset < log_end {
        if bytes[offset] == 0 {
            offset += 1;
            continue;
        }
        if bytes[offset] != b'g' {
            break;
        }
        let uncompressed_len = read_u24(bytes, offset + 1)? as usize;
        if uncompressed_len < 6 {
            return Err(invalid_reftable("invalid log block length"));
        }
        if offset.checked_add(4).is_none_or(|start| start > log_end) {
            return Err(invalid_reftable("truncated log block header"));
        }
        let (body, consumed) = inflate_zlib(
            &bytes[offset + 4..log_end],
            uncompressed_len.saturating_sub(4),
        )?;
        let mut block = Vec::with_capacity(uncompressed_len);
        block.extend_from_slice(&bytes[offset..offset + 4]);
        block.extend_from_slice(&body);
        records.extend(parse_log_block(&block, layout.header.oid_len)?);
        offset = offset
            .checked_add(4 + consumed)
            .ok_or_else(|| invalid_reftable("log block position overflow"))?;
    }
    Ok(records)
}

fn parse_header(bytes: &[u8]) -> Result<ReftableHeader, GitAiError> {
    if bytes.get(..4) != Some(REFTABLE_MAGIC) {
        return Err(invalid_reftable("missing reftable magic"));
    }
    let version = match bytes.get(4) {
        Some(1) => ReftableVersion::V1,
        Some(2) => ReftableVersion::V2,
        _ => return Err(invalid_reftable("unsupported reftable version")),
    };
    if bytes.len() < version.header_len() {
        return Err(invalid_reftable("truncated reftable header"));
    }
    let oid_len = match version {
        ReftableVersion::V1 => 20,
        ReftableVersion::V2 => match bytes.get(24..28) {
            Some(b"sha1") => 20,
            Some(b"s256") => 32,
            _ => return Err(invalid_reftable("unsupported reftable object format")),
        },
    };
    Ok(ReftableHeader {
        version,
        block_size: read_u24(bytes, 5)?,
        min_update_index: read_u64(bytes, 8)?,
        max_update_index: read_u64(bytes, 16)?,
        oid_len,
    })
}

fn parse_ref_section(
    bytes: &[u8],
    header: ReftableHeader,
    ref_end: usize,
    target: Option<&str>,
) -> Result<Vec<ReftableRefEntry>, GitAiError> {
    let mut refs = Vec::new();
    let mut offset = header.version.header_len();
    while offset < ref_end {
        if bytes[offset] == 0 {
            offset += 1;
            continue;
        }
        if bytes[offset] != b'r' {
            break;
        }
        let block_len = read_u24(bytes, offset + 1)? as usize;
        let block_end = if offset == header.version.header_len() {
            block_len
        } else {
            offset
                .checked_add(block_len)
                .ok_or_else(|| invalid_reftable("ref block position overflow"))?
        };
        if block_end <= offset || block_end > ref_end || block_end > bytes.len() {
            return Err(invalid_reftable("ref block extends past section"));
        }
        let block_refs = parse_ref_block(&bytes[offset..block_end], offset, header)?;
        if let Some(target) = target {
            for entry in block_refs {
                match entry.name.as_str().cmp(target) {
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Equal => return Ok(vec![entry]),
                    std::cmp::Ordering::Greater => return Ok(Vec::new()),
                }
            }
        } else {
            refs.extend(block_refs);
        }
        offset = block_end;
    }
    Ok(refs)
}

fn parse_ref_block(
    block: &[u8],
    block_start: usize,
    header: ReftableHeader,
) -> Result<Vec<ReftableRefEntry>, GitAiError> {
    if block.len() < 6 || block[0] != b'r' {
        return Err(invalid_reftable("invalid ref block"));
    }
    let restart_count = read_u16(block, block.len() - 2)? as usize;
    if restart_count == 0 {
        return Err(invalid_reftable("ref block has no restart offsets"));
    }
    let restart_table_start = block
        .len()
        .checked_sub(2 + restart_count * 3)
        .ok_or_else(|| invalid_reftable("truncated ref restart table"))?;
    let mut restart_offsets = Vec::with_capacity(restart_count);
    for index in 0..restart_count {
        restart_offsets.push(read_u24(block, restart_table_start + index * 3)? as usize);
    }
    if restart_offsets.windows(2).any(|pair| pair[0] > pair[1]) {
        return Err(invalid_reftable("unsorted ref restart offsets"));
    }

    let restart_base = if block_start == header.version.header_len() {
        block_start
    } else {
        0
    };
    let mut offset = 4;
    let mut previous_name = Vec::new();
    let mut refs = Vec::new();
    while offset < restart_table_start {
        let restart = restart_offsets.contains(&(restart_base + offset));
        let entry = parse_ref_record(
            block,
            &mut offset,
            restart_table_start,
            header,
            &previous_name,
            restart,
        )?;
        previous_name = entry.name.as_bytes().to_vec();
        refs.push(entry);
    }
    if offset != restart_table_start {
        return Err(invalid_reftable("ref block ended inside a record"));
    }
    Ok(refs)
}

fn parse_ref_record(
    block: &[u8],
    offset: &mut usize,
    end: usize,
    header: ReftableHeader,
    previous_name: &[u8],
    restart: bool,
) -> Result<ReftableRefEntry, GitAiError> {
    let prefix_len = read_varint(block, offset, end)? as usize;
    if prefix_len > previous_name.len() || (restart && prefix_len != 0) {
        return Err(invalid_reftable("invalid ref name prefix"));
    }
    let suffix_len_and_type = read_varint(block, offset, end)?;
    let suffix_len = (suffix_len_and_type >> 3) as usize;
    let value_type = (suffix_len_and_type & 0x7) as u8;
    let suffix_end = offset
        .checked_add(suffix_len)
        .ok_or_else(|| invalid_reftable("ref suffix overflow"))?;
    if suffix_end > end {
        return Err(invalid_reftable("truncated ref suffix"));
    }
    let mut name = previous_name[..prefix_len].to_vec();
    name.extend_from_slice(&block[*offset..suffix_end]);
    *offset = suffix_end;
    let _update_index = header
        .min_update_index
        .checked_add(read_varint(block, offset, end)?)
        .ok_or_else(|| invalid_reftable("ref update index overflow"))?;
    let value = match value_type {
        0 => ReftableRefValue::Deletion,
        1 => ReftableRefValue::Direct(read_oid(block, offset, end, header.oid_len)?),
        2 => {
            let target = read_oid(block, offset, end, header.oid_len)?;
            let _peeled = read_oid(block, offset, end, header.oid_len)?;
            ReftableRefValue::Direct(target)
        }
        3 => {
            let length = read_varint(block, offset, end)? as usize;
            let target_end = offset
                .checked_add(length)
                .ok_or_else(|| invalid_reftable("symbolic ref target overflow"))?;
            if target_end > end {
                return Err(invalid_reftable("truncated symbolic ref target"));
            }
            let target = String::from_utf8(block[*offset..target_end].to_vec())
                .map_err(|_| invalid_reftable("symbolic ref target is not UTF-8"))?;
            *offset = target_end;
            ReftableRefValue::Symbolic(target)
        }
        _ => return Err(invalid_reftable("unsupported ref value type")),
    };
    let name = String::from_utf8(name).map_err(|_| invalid_reftable("ref name is not UTF-8"))?;
    Ok(ReftableRefEntry { name, value })
}

fn parse_log_block(block: &[u8], oid_len: usize) -> Result<Vec<ParsedLogRecord>, GitAiError> {
    if block.len() < 6 || block[0] != b'g' {
        return Err(invalid_reftable("invalid log block"));
    }
    let restart_count = read_u16(block, block.len() - 2)? as usize;
    if restart_count == 0 {
        return Err(invalid_reftable("log block has no restart offsets"));
    }
    let restart_table_start = block
        .len()
        .checked_sub(2 + restart_count * 3)
        .ok_or_else(|| invalid_reftable("truncated log restart table"))?;
    let mut offset = 4;
    let mut previous_key = Vec::new();
    let mut records = Vec::new();
    while offset < restart_table_start {
        records.push(parse_log_record(
            block,
            &mut offset,
            restart_table_start,
            oid_len,
            &mut previous_key,
        )?);
    }
    if offset != restart_table_start {
        return Err(invalid_reftable("log block ended inside a record"));
    }
    Ok(records)
}

fn parse_log_record(
    block: &[u8],
    offset: &mut usize,
    end: usize,
    oid_len: usize,
    previous_key: &mut Vec<u8>,
) -> Result<ParsedLogRecord, GitAiError> {
    let prefix_len = read_varint(block, offset, end)? as usize;
    if prefix_len > previous_key.len() {
        return Err(invalid_reftable("log prefix exceeds previous key"));
    }
    let suffix_len_and_type = read_varint(block, offset, end)?;
    let suffix_len = (suffix_len_and_type >> 3) as usize;
    let value_type = (suffix_len_and_type & 0x7) as u8;
    let suffix_end = offset
        .checked_add(suffix_len)
        .ok_or_else(|| invalid_reftable("log suffix overflow"))?;
    if suffix_end > end {
        return Err(invalid_reftable("truncated log suffix"));
    }
    let mut key = previous_key[..prefix_len].to_vec();
    key.extend_from_slice(&block[*offset..suffix_end]);
    *offset = suffix_end;
    if key.len() < 9 || key[key.len() - 9] != 0 {
        return Err(invalid_reftable("malformed log key"));
    }
    let reference = String::from_utf8(key[..key.len() - 9].to_vec())
        .map_err(|_| invalid_reftable("log reference is not UTF-8"))?;
    let index_bytes: [u8; 8] = key[key.len() - 8..]
        .try_into()
        .map_err(|_| invalid_reftable("truncated log update index"))?;
    let update_index = u64::MAX - u64::from_be_bytes(index_bytes);
    *previous_key = key;

    let value = match value_type {
        0 => ParsedLogValue::Deletion,
        1 => {
            let old_oid = read_oid(block, offset, end, oid_len)?;
            let new_oid = read_oid(block, offset, end, oid_len)?;
            let _name = read_string(block, offset, end)?;
            let _email = read_string(block, offset, end)?;
            let timestamp = read_varint(block, offset, end)?;
            let timestamp_secs = i64::try_from(timestamp)
                .map_err(|_| invalid_reftable("log timestamp exceeds i64"))?;
            if offset.saturating_add(2) > end {
                return Err(invalid_reftable("truncated log timezone"));
            }
            *offset += 2;
            let message = read_string(block, offset, end)?
                .trim_end_matches(['\r', '\n'])
                .to_string();
            if old_oid.bytes().all(|byte| byte == b'0') && new_oid.bytes().all(|byte| byte == b'0')
            {
                ParsedLogValue::DeleteLog
            } else {
                ParsedLogValue::Update(ReftableLogEntry {
                    reference: reference.clone(),
                    update_index,
                    old_oid,
                    new_oid,
                    message,
                    timestamp_secs,
                })
            }
        }
        _ => return Err(invalid_reftable("unsupported log value type")),
    };
    Ok(ParsedLogRecord {
        reference,
        update_index,
        value,
    })
}

fn inflate_zlib(bytes: &[u8], expected_len: usize) -> Result<(Vec<u8>, usize), GitAiError> {
    use flate2::{Decompress, FlushDecompress};
    let mut decoder = Decompress::new(true);
    let mut output = Vec::with_capacity(expected_len);
    decoder
        .decompress_vec(bytes, &mut output, FlushDecompress::Finish)
        .map_err(|error| invalid_reftable(format!("log inflate failed: {error}")))?;
    Ok((output, decoder.total_in() as usize))
}

fn read_oid(
    bytes: &[u8],
    offset: &mut usize,
    end: usize,
    oid_len: usize,
) -> Result<String, GitAiError> {
    let oid_end = offset
        .checked_add(oid_len)
        .ok_or_else(|| invalid_reftable("object id position overflow"))?;
    if oid_end > end {
        return Err(invalid_reftable("truncated object id"));
    }
    let mut hex = String::with_capacity(oid_len * 2);
    for byte in &bytes[*offset..oid_end] {
        use std::fmt::Write;
        write!(hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    *offset = oid_end;
    Ok(hex)
}

fn read_string(bytes: &[u8], offset: &mut usize, end: usize) -> Result<String, GitAiError> {
    let len = read_varint(bytes, offset, end)? as usize;
    let string_end = offset
        .checked_add(len)
        .ok_or_else(|| invalid_reftable("string position overflow"))?;
    if string_end > end {
        return Err(invalid_reftable("truncated string"));
    }
    let value = String::from_utf8_lossy(&bytes[*offset..string_end]).into_owned();
    *offset = string_end;
    Ok(value)
}

fn read_varint(bytes: &[u8], offset: &mut usize, end: usize) -> Result<u64, GitAiError> {
    if *offset >= end {
        return Err(invalid_reftable("truncated varint"));
    }
    let mut value = u64::from(bytes[*offset] & 0x7f);
    while bytes[*offset] & 0x80 != 0 {
        *offset += 1;
        if *offset >= end {
            return Err(invalid_reftable("truncated varint"));
        }
        value = value
            .checked_add(1)
            .and_then(|value| value.checked_mul(128))
            .ok_or_else(|| invalid_reftable("varint overflow"))?
            | u64::from(bytes[*offset] & 0x7f);
    }
    *offset += 1;
    Ok(value)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, GitAiError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| invalid_reftable("truncated uint16"))?;
    Ok(u16::from_be_bytes([raw[0], raw[1]]))
}

fn read_u24(bytes: &[u8], offset: usize) -> Result<u32, GitAiError> {
    let raw = bytes
        .get(offset..offset + 3)
        .ok_or_else(|| invalid_reftable("truncated uint24"))?;
    Ok((u32::from(raw[0]) << 16) | (u32::from(raw[1]) << 8) | u32::from(raw[2]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, GitAiError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| invalid_reftable("truncated uint32"))?;
    Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, GitAiError> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| invalid_reftable("truncated uint64"))?;
    Ok(u64::from_be_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn invalid_reftable(message: impl Into<String>) -> GitAiError {
    GitAiError::Generic(format!("invalid reftable: {}", message.into()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;

    fn git_with_env(repo: &Path, args: &[&str], env: &[(&str, &str)]) {
        let mut command_args = vec!["-C".to_string(), repo.to_string_lossy().to_string()];
        command_args.extend(args.iter().map(|arg| (*arg).to_string()));
        let env = env
            .iter()
            .map(|(key, value)| (*key, OsStr::new(value)))
            .collect::<Vec<_>>();
        let output = crate::git::repository::exec_git_allow_nonzero_with_env(&command_args, &env)
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git(repo: &Path, args: &[&str]) {
        git_with_env(repo, args, &[]);
    }

    fn git_with_stdin(repo: &Path, args: &[&str], stdin: &[u8]) {
        let mut command_args = vec!["-C".to_string(), repo.to_string_lossy().to_string()];
        command_args.extend(args.iter().map(|arg| (*arg).to_string()));
        crate::git::repository::exec_git_stdin(&command_args, stdin)
            .expect("git command with stdin should succeed");
    }

    pub(crate) fn native_reftable_repo(object_format: &str) -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(
            temp.path(),
            &[
                "init",
                "--ref-format=reftable",
                &format!("--object-format={object_format}"),
                "-b",
                "main",
                ".",
            ],
        );
        git(temp.path(), &["config", "user.name", "Reftable Test"]);
        git(
            temp.path(),
            &["config", "user.email", "reftable@example.com"],
        );
        fs::write(temp.path().join("file.txt"), "first\n").unwrap();
        git(temp.path(), &["add", "file.txt"]);
        git(temp.path(), &["commit", "-m", "first"]);
        fs::write(temp.path().join("file.txt"), "first\nsecond\n").unwrap();
        git(temp.path(), &["commit", "-am", "second"]);
        temp
    }

    fn native_reftable_logs(object_format: &str) -> Vec<ReftableLogEntry> {
        let temp = native_reftable_repo(object_format);
        read_reftable_logs(&temp.path().join(".git/reftable")).unwrap()
    }

    fn corrupt_log_position_near_footer(bytes: &mut [u8]) {
        let header = parse_header(bytes).unwrap();
        let footer_start = bytes.len() - header.version.footer_len();
        let log_position_field = footer_start + header.version.header_len() + 24;
        let log_position = footer_start - 2;
        bytes[log_position] = b'g';
        bytes[log_position_field..log_position_field + 8]
            .copy_from_slice(&(log_position as u64).to_be_bytes());
        let footer_crc = crc32(&bytes[footer_start..bytes.len() - 4]);
        let crc_offset = bytes.len() - 4;
        bytes[crc_offset..].copy_from_slice(&footer_crc.to_be_bytes());
    }

    fn assert_native_log_history(object_format: &str, oid_hex_len: usize) {
        let logs = native_reftable_logs(object_format);
        let head_updates = logs
            .iter()
            .filter(|entry| entry.reference == "HEAD")
            .collect::<Vec<_>>();
        assert_eq!(head_updates.len(), 2, "unexpected logs: {logs:#?}");
        assert!(
            head_updates[0].message.ends_with("first"),
            "unexpected HEAD history: {head_updates:#?}"
        );
        assert!(
            head_updates[1].message.ends_with("second"),
            "unexpected HEAD history: {head_updates:#?}"
        );
        assert_eq!(head_updates[0].old_oid.len(), oid_hex_len);
        assert_eq!(head_updates[1].new_oid.len(), oid_hex_len);
        assert!(head_updates[0].update_index < head_updates[1].update_index);
    }

    #[test]
    fn reads_git_generated_v1_sha1_log_blocks() {
        assert_native_log_history("sha1", 40);
    }

    #[test]
    fn reads_git_generated_v2_sha256_log_blocks() {
        assert_native_log_history("sha256", 64);
    }

    #[test]
    fn missing_active_table_degrades_to_empty_log_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("tables.list"), "missing.ref\n").unwrap();

        let logs = ReftableReader::default()
            .read_logs(temp.path())
            .expect("a compaction race must not abort command enrichment");

        assert!(logs.is_empty());
    }

    #[test]
    fn unchanged_stack_does_not_rebuild_log_snapshot() {
        let temp = native_reftable_repo("sha1");
        let stack = temp.path().join(".git/reftable");
        let mut reader = ReftableReader::default();

        assert!(reader.read_logs_if_changed(&stack).unwrap().is_some());
        assert!(reader.read_logs_if_changed(&stack).unwrap().is_none());
    }

    #[test]
    fn transient_log_table_io_error_is_retried() {
        let source = native_reftable_repo("sha1");
        let source_stack = source.path().join(".git/reftable");
        let source_table = active_table_paths(&source_stack).unwrap().pop().unwrap();
        let table_name = source_table.file_name().unwrap();
        let table_bytes = fs::read(&source_table).unwrap();
        let stack = tempfile::tempdir().unwrap();
        fs::write(
            stack.path().join("tables.list"),
            format!("{}\n", table_name.to_string_lossy()),
        )
        .unwrap();
        let table = stack.path().join(table_name);
        fs::create_dir(&table).unwrap();
        let mut reader = ReftableReader::default();

        assert!(reader.read_logs(stack.path()).unwrap().is_empty());
        fs::remove_dir(&table).unwrap();
        fs::write(&table, table_bytes).unwrap();

        assert!(!reader.read_logs(stack.path()).unwrap().is_empty());
    }

    #[test]
    fn transient_log_table_io_error_preserves_last_complete_snapshot() {
        let source = native_reftable_repo("sha1");
        let source_stack = source.path().join(".git/reftable");
        let source_table = active_table_paths(&source_stack).unwrap().pop().unwrap();
        let table_name = source_table.file_name().unwrap().to_string_lossy();
        let table_bytes = fs::read(&source_table).unwrap();
        let stack = tempfile::tempdir().unwrap();
        let table = stack.path().join(table_name.as_ref());
        fs::write(&table, &table_bytes).unwrap();
        fs::write(stack.path().join("tables.list"), format!("{table_name}\n")).unwrap();
        let mut reader = ReftableReader::default();
        let initial = reader.read_logs_if_changed(stack.path()).unwrap().unwrap();

        let transient_name = "transient.ref";
        let transient_table = stack.path().join(transient_name);
        fs::create_dir(&transient_table).unwrap();
        fs::write(
            stack.path().join("tables.list"),
            format!("{table_name}\n{transient_name}\n"),
        )
        .unwrap();

        assert!(reader.read_logs_if_changed(stack.path()).unwrap().is_none());
        assert_eq!(reader.cached_logs(stack.path()), Some(initial.as_slice()));

        fs::remove_dir(&transient_table).unwrap();
        fs::write(&transient_table, table_bytes).unwrap();
        assert!(reader.read_logs_if_changed(stack.path()).unwrap().is_some());
    }

    #[test]
    fn truncated_log_block_header_returns_error() {
        let temp = native_reftable_repo("sha1");
        let stack = temp.path().join(".git/reftable");
        let table = active_table_paths(&stack).unwrap().pop().unwrap();
        let mut bytes = fs::read(table).unwrap();
        corrupt_log_position_near_footer(&mut bytes);

        assert!(parse_table(&bytes).is_err());
    }

    #[test]
    fn invalid_first_ref_block_length_returns_error() {
        let temp = native_reftable_repo("sha1");
        let stack = temp.path().join(".git/reftable");
        let table = active_table_paths(&stack).unwrap().pop().unwrap();
        let mut bytes = fs::read(table).unwrap();
        let header = parse_header(&bytes).unwrap();
        let invalid_len = header.version.header_len() - 1;
        bytes[header.version.header_len() + 1..header.version.header_len() + 4].copy_from_slice(&[
            ((invalid_len >> 16) & 0xff) as u8,
            ((invalid_len >> 8) & 0xff) as u8,
            (invalid_len & 0xff) as u8,
        ]);

        assert!(parse_table(&bytes).is_err());
    }

    #[test]
    fn oversized_varint_returns_error() {
        let mut bytes = vec![0x81; 10];
        bytes.push(0);
        let mut offset = 0;

        assert!(read_varint(&bytes, &mut offset, bytes.len()).is_err());
    }

    #[test]
    fn later_ref_block_restart_offsets_are_block_relative() {
        let temp = native_reftable_repo("sha1");
        let stack = temp.path().join(".git/reftable");
        let Some((head_oid, _)) = ReftableReader::default().read_head(&stack, &stack).unwrap()
        else {
            panic!("generated repository should have a direct branch target");
        };
        let mut updates = String::new();
        for index in 0..600 {
            updates.push_str(&format!(
                "create refs/heads/generated-{index:04} {head_oid}\n"
            ));
        }
        git_with_stdin(temp.path(), &["update-ref", "--stdin"], updates.as_bytes());

        let (bytes, header, block_start, block_end) = active_table_paths(&stack)
            .unwrap()
            .into_iter()
            .rev()
            .find_map(|table| {
                let bytes = fs::read(table).ok()?;
                let header = parse_header(&bytes).ok()?;
                let footer_start = bytes.len() - header.version.footer_len();
                let mut offset = header.version.header_len();
                let mut blocks = Vec::new();
                while offset < footer_start {
                    if bytes[offset] == 0 {
                        offset += 1;
                        continue;
                    }
                    if bytes[offset] != b'r' {
                        break;
                    }
                    let block_len = read_u24(&bytes, offset + 1).ok()? as usize;
                    let block_end = if offset == header.version.header_len() {
                        block_len
                    } else {
                        offset.checked_add(block_len)?
                    };
                    blocks.push((offset, block_end));
                    offset = block_end;
                }
                let (block_start, block_end) = *blocks.get(1)?;
                Some((bytes, header, block_start, block_end))
            })
            .expect("generated refs should span multiple ref blocks");
        let mut block = bytes[block_start..block_end].to_vec();
        let restart_count = read_u16(&block, block.len() - 2).unwrap() as usize;
        assert!(restart_count > 1);
        let restart_table_start = block.len() - 2 - restart_count * 3;
        let second_restart = read_u24(&block, restart_table_start + 3).unwrap() as usize;
        block[second_restart] = 1;

        assert!(parse_ref_block(&block, block_start, header).is_err());
    }

    #[test]
    fn head_read_does_not_parse_corrupt_log_blocks() {
        let temp = native_reftable_repo("sha1");
        let stack = temp.path().join(".git/reftable");
        let expected = ReftableReader::default()
            .read_head(&stack, &stack)
            .unwrap()
            .expect("generated repository should have HEAD");
        let table = active_table_paths(&stack).unwrap().pop().unwrap();
        let mut bytes = fs::read(&table).unwrap();
        let header = parse_header(&bytes).unwrap();
        let footer_start = bytes.len() - header.version.footer_len();
        let log_position_field = footer_start + header.version.header_len() + 24;
        let log_position = read_u64(&bytes, log_position_field).unwrap() as usize;
        assert_ne!(log_position, 0);
        bytes[log_position + 4] ^= 0xff;
        fs::write(table, bytes).unwrap();

        assert_eq!(
            ReftableReader::default().read_head(&stack, &stack).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn symbolic_head_resolution_uses_one_stack_snapshot_and_table_read() {
        let temp = native_reftable_repo("sha1");
        let stack = temp.path().join(".git/reftable");
        let table_count = active_table_paths(&stack).unwrap().len();
        let mut reader = ReftableReader::default();

        assert!(reader.read_head(&stack, &stack).unwrap().is_some());
        assert_eq!(reader.stack_read_count, 1);
        assert_eq!(reader.table_read_count, table_count);
    }

    #[test]
    fn merges_multiple_tables_and_applies_reflog_expiry_tombstones() {
        let temp = tempfile::tempdir().unwrap();
        git(
            temp.path(),
            &["init", "--ref-format=reftable", "-b", "main", "."],
        );
        git(temp.path(), &["config", "user.name", "Reftable Test"]);
        git(
            temp.path(),
            &["config", "user.email", "reftable@example.com"],
        );
        let stack_dir = temp.path().join(".git/reftable");
        let multi_stack = temp.path().join("multi-stack");
        fs::create_dir(&multi_stack).unwrap();
        let mut snapshot_names = Vec::new();
        for index in 0..2 {
            git(
                temp.path(),
                &["commit", "--allow-empty", "-m", &format!("commit {index}")],
            );
            for (table_index, table) in active_table_paths(&stack_dir)
                .unwrap()
                .into_iter()
                .enumerate()
            {
                let snapshot_name = format!("snapshot-{index}-{table_index}.ref");
                fs::copy(table, multi_stack.join(&snapshot_name)).unwrap();
                snapshot_names.push(snapshot_name);
            }
        }
        fs::write(
            multi_stack.join("tables.list"),
            format!("{}\n", snapshot_names.join("\n")),
        )
        .unwrap();
        assert!(
            fs::read_to_string(multi_stack.join("tables.list"))
                .unwrap()
                .lines()
                .count()
                > 1
        );

        let mut reader = ReftableReader::default();
        assert_eq!(
            reader
                .read_logs(&multi_stack)
                .unwrap()
                .iter()
                .filter(|entry| entry.reference == "HEAD")
                .count(),
            2
        );
        assert_eq!(
            reader
                .read_logs(&stack_dir)
                .unwrap()
                .iter()
                .filter(|entry| entry.reference == "HEAD")
                .count(),
            2
        );
        git(temp.path(), &["reflog", "expire", "--expire=all", "--all"]);
        let expired = reader.read_logs(&stack_dir).unwrap();
        assert!(expired.is_empty(), "expired logs remained: {expired:#?}");

        git(
            temp.path(),
            &["commit", "--allow-empty", "-m", "after expiry"],
        );
        let logs_after_expiry = reader.read_logs(&stack_dir).unwrap();
        assert_eq!(
            logs_after_expiry
                .iter()
                .filter(|entry| entry.reference == "HEAD")
                .map(|entry| entry.message.as_str())
                .collect::<Vec<_>>(),
            ["commit: after expiry"],
            "new logs after expiry were not visible: {logs_after_expiry:#?}"
        );
    }
}
