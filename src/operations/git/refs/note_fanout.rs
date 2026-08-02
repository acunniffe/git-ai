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

pub(super) fn append_note_deletions(script: &mut Vec<u8>, oid: &str) {
    let oid = oid.as_bytes();
    script.extend_from_slice(b"D ");
    script.extend_from_slice(oid);
    script.push(b'\n');

    for prefix_end in (2..oid.len()).step_by(2) {
        script.extend_from_slice(b"D ");
        for component_start in (0..prefix_end).step_by(2) {
            script.extend_from_slice(&oid[component_start..component_start + 2]);
            script.push(b'/');
        }
        script.extend_from_slice(&oid[prefix_end..]);
        script.push(b'\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_note_deletions_includes_every_fanout_depth() {
        let mut short_script = Vec::new();
        append_note_deletions(&mut short_script, "a");
        append_note_deletions(&mut short_script, "ab");
        assert_eq!(String::from_utf8(short_script).unwrap(), "D a\nD ab\n");

        let mut script = Vec::new();
        append_note_deletions(&mut script, "abcdef");
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
