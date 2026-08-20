pub fn notes_path_for_object(oid: &str) -> String {
    if oid.len() <= 2 {
        oid.to_string()
    } else {
        format!("{}/{}", &oid[..2], &oid[2..])
    }
}

pub fn flat_note_pathspec_for_commit(commit_sha: &str) -> String {
    flat_note_pathspec_for_ref(super::AI_AUTHORSHIP_FULL_REF, commit_sha)
}

pub fn fanout_note_pathspec_for_commit(commit_sha: &str) -> String {
    fanout_note_pathspec_for_ref(super::AI_AUTHORSHIP_FULL_REF, commit_sha)
}

pub fn flat_note_pathspec_for_ref(notes_ref: &str, commit_sha: &str) -> String {
    format!("{}:{}", notes_ref, commit_sha)
}

pub fn fanout_note_pathspec_for_ref(notes_ref: &str, commit_sha: &str) -> String {
    format!("{}:{}", notes_ref, notes_path_for_object(commit_sha))
}

pub(super) fn normalize_note_path(path: &mut String) -> Option<usize> {
    let mut component_len = 0;
    let mut fanout_depth = 0;
    let mut valid = true;
    path.retain(|character| {
        if character == '/' {
            valid &= component_len == 2;
            component_len = 0;
            fanout_depth += 1;
            false
        } else {
            component_len += character.len_utf8();
            true
        }
    });

    (valid && component_len > 0).then_some(fanout_depth)
}

pub(super) fn write_note_deletions(
    writer: &mut (impl std::io::Write + ?Sized),
    oid: &str,
) -> std::io::Result<()> {
    // Emit each path directly into fast-import's bounded stdin buffer.
    // Building one script would retain 20 paths per SHA-1 note and 32 per
    // SHA-256 note.
    let oid = oid.as_bytes();
    writer.write_all(b"D ")?;
    writer.write_all(oid)?;
    writer.write_all(b"\n")?;

    for prefix_end in (2..oid.len()).step_by(2) {
        writer.write_all(b"D ")?;
        for component_start in (0..prefix_end).step_by(2) {
            writer.write_all(&oid[component_start..component_start + 2])?;
            writer.write_all(b"/")?;
        }
        writer.write_all(&oid[prefix_end..])?;
        writer.write_all(b"\n")?;
    }

    Ok(())
}

/// Writes one fast-import blob stanza carrying `content` under mark `mark`.
/// The trailing newline is a fast-import stream separator, not part of the
/// data.
pub(in crate::operations::git) fn write_blob_stanza(
    writer: &mut (impl std::io::Write + ?Sized),
    mark: usize,
    content: &str,
) -> std::io::Result<()> {
    writer.write_all(b"blob\n")?;
    writeln!(writer, "mark :{mark}")?;
    writeln!(writer, "data {}", content.len())?;
    writer.write_all(content.as_bytes())?;
    writer.write_all(b"\n")
}

/// Writes a fast-import commit header for `notes_ref` with an empty message,
/// starting `from` the given parent when present.
pub(in crate::operations::git) fn write_notes_commit_header(
    writer: &mut (impl std::io::Write + ?Sized),
    notes_ref: &str,
    committer_line: std::fmt::Arguments<'_>,
    from: Option<&str>,
) -> std::io::Result<()> {
    writeln!(writer, "commit {notes_ref}")?;
    writeln!(writer, "committer {committer_line}")?;
    writer.write_all(b"data 0\n")?;
    if let Some(from) = from {
        writeln!(writer, "from {from}")?;
    }
    Ok(())
}

/// Writes one note replacement: deletions at every legacy fanout depth
/// followed by an `M 100644 <source> <fanout path>` line, where `source` is
/// a mark reference or blob OID.
pub(super) fn write_note_entry(
    writer: &mut (impl std::io::Write + ?Sized),
    commit_sha: &str,
    source: std::fmt::Arguments<'_>,
) -> std::io::Result<()> {
    write_note_deletions(writer, commit_sha)?;
    writeln!(
        writer,
        "M 100644 {source} {}",
        notes_path_for_object(commit_sha)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_note_deletions_includes_every_fanout_depth() {
        let mut short_script = Vec::new();
        write_note_deletions(&mut short_script, "a").unwrap();
        write_note_deletions(&mut short_script, "ab").unwrap();
        assert_eq!(String::from_utf8(short_script).unwrap(), "D a\nD ab\n");

        let mut script = Vec::new();
        write_note_deletions(&mut script, "abcdef").unwrap();
        assert_eq!(
            String::from_utf8(script).unwrap(),
            "D abcdef\nD ab/cdef\nD ab/cd/ef\n"
        );
    }

    #[test]
    fn normalize_note_path_in_place() {
        for (path, expected_depth) in [("abcdef", 0), ("ab/cdef", 1), ("ab/cd/ef", 2)] {
            let mut path = path.to_string();
            assert_eq!(normalize_note_path(&mut path), Some(expected_depth));
            assert_eq!(path, "abcdef");
        }

        for invalid_path in ["", "/abcdef", "a/bcdef", "ab/", "ab//cdef"] {
            let mut path = invalid_path.to_string();
            assert_eq!(normalize_note_path(&mut path), None);
        }
    }
}
