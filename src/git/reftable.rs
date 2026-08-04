// Reftable block decoding adapted from `sley-formats` (Apache-2.0):
// https://github.com/HeddleCo/sley
use crate::error::GitAiError;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

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
    parsed_tables: HashMap<std::path::PathBuf, Vec<ParsedLogRecord>>,
}

impl ReftableReader {
    pub(crate) fn read_logs(
        &mut self,
        stack_dir: &Path,
    ) -> Result<Vec<ReftableLogEntry>, GitAiError> {
        let table_names = match fs::read_to_string(stack_dir.join("tables.list")) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut active_paths = HashSet::new();
        let mut visible = BTreeMap::<(String, u64), ReftableLogEntry>::new();
        for table_name in table_names.lines().filter(|line| !line.is_empty()) {
            if Path::new(table_name)
                .file_name()
                .and_then(|name| name.to_str())
                != Some(table_name)
            {
                return Err(invalid_reftable("invalid table name in tables.list"));
            }
            let table_path = stack_dir.join(table_name);
            active_paths.insert(table_path.clone());
            let records = if let Some(records) = self.parsed_tables.get(&table_path) {
                records
            } else {
                let records = parse_table_logs(&fs::read(&table_path)?)?;
                self.parsed_tables.insert(table_path.clone(), records);
                self.parsed_tables
                    .get(&table_path)
                    .expect("newly cached reftable must be present")
            };
            for record in records {
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
        self.parsed_tables
            .retain(|path, _| !path.starts_with(stack_dir) || active_paths.contains(path));
        let mut logs = visible.into_values().collect::<Vec<_>>();
        logs.sort_by(|left, right| {
            left.update_index
                .cmp(&right.update_index)
                .then_with(|| left.reference.cmp(&right.reference))
        });
        Ok(logs)
    }
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

fn parse_table_logs(bytes: &[u8]) -> Result<Vec<ParsedLogRecord>, GitAiError> {
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
    footer_offset += 8; // ref index position
    footer_offset += 8; // object position and abbreviated object-id length
    footer_offset += 8; // object index position
    let footer_log_position = read_u64(bytes, footer_offset)? as usize;
    footer_offset += 8;
    let log_index_position = read_u64(bytes, footer_offset)? as usize;
    let log_position = if footer_log_position != 0 {
        footer_log_position
    } else if bytes.get(header.version.header_len()) == Some(&b'g') {
        header.version.header_len()
    } else {
        return Ok(Vec::new());
    };
    let log_end = if log_index_position == 0 {
        footer_start
    } else {
        log_index_position.min(footer_start)
    };
    if log_position >= log_end {
        return Err(invalid_reftable("log section position is out of bounds"));
    }

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
        let (body, consumed) = inflate_zlib(
            &bytes[offset + 4..log_end],
            uncompressed_len.saturating_sub(4),
        )?;
        let mut block = Vec::with_capacity(uncompressed_len);
        block.extend_from_slice(&bytes[offset..offset + 4]);
        block.extend_from_slice(&body);
        records.extend(parse_log_block(&block, header.oid_len)?);
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
            .and_then(|value| value.checked_shl(7))
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
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn git_with_env(repo: &Path, args: &[&str], env: &[(&str, &str)]) {
        let mut command = Command::new(crate::config::Config::get().git_cmd());
        command
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_TRACE2_EVENT");
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git(repo: &Path, args: &[&str]) {
        git_with_env(repo, args, &[]);
    }

    fn native_reftable_logs(object_format: &str) -> Vec<ReftableLogEntry> {
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

        read_reftable_logs(&temp.path().join(".git/reftable")).unwrap()
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
        let no_compaction = [("GIT_TEST_REFTABLE_AUTOCOMPACTION", "0")];
        for index in 0..5 {
            git_with_env(
                temp.path(),
                &["commit", "--allow-empty", "-m", &format!("commit {index}")],
                &no_compaction,
            );
        }
        let stack_dir = temp.path().join(".git/reftable");
        assert!(
            fs::read_to_string(stack_dir.join("tables.list"))
                .unwrap()
                .lines()
                .count()
                > 1
        );

        let mut reader = ReftableReader::default();
        assert_eq!(
            reader
                .read_logs(&stack_dir)
                .unwrap()
                .iter()
                .filter(|entry| entry.reference == "HEAD")
                .count(),
            5
        );
        git(temp.path(), &["reflog", "expire", "--expire=all", "--all"]);
        let expired = reader.read_logs(&stack_dir).unwrap();
        assert!(expired.is_empty(), "expired logs remained: {expired:#?}");
    }
}
