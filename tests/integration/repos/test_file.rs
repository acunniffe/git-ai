#![allow(dead_code)]

use std::{fs, path::PathBuf};

use insta::assert_debug_snapshot;

use super::blame_support::{is_ai_blame_author, parse_blame_line};

#[derive(Debug, Clone, PartialEq)]
pub enum AuthorType {
    Human,
    UnattributedHuman,
    Ai,
}

#[derive(Debug, Clone)]
pub struct ExpectedLine {
    pub contents: String,
    pub author_type: AuthorType,
}

impl ExpectedLine {
    fn new(contents: String, author_type: AuthorType) -> Self {
        if contents.contains('\n') {
            panic!(
                "fluent test file API does not support strings with new lines (must be a single line): {:?}",
                contents
            );
        }
        Self {
            contents,
            author_type,
        }
    }
}

/// Trait to add .ai(), .human(), and .unattributed_human() methods to string types
pub trait ExpectedLineExt {
    fn ai(self) -> ExpectedLine;
    fn human(self) -> ExpectedLine;
    fn unattributed_human(self) -> ExpectedLine;
}

impl ExpectedLineExt for &str {
    fn ai(self) -> ExpectedLine {
        ExpectedLine::new(self.to_string(), AuthorType::Ai)
    }

    fn human(self) -> ExpectedLine {
        ExpectedLine::new(self.to_string(), AuthorType::Human)
    }

    fn unattributed_human(self) -> ExpectedLine {
        ExpectedLine::new(self.to_string(), AuthorType::UnattributedHuman)
    }
}

impl ExpectedLineExt for String {
    fn ai(self) -> ExpectedLine {
        ExpectedLine::new(self, AuthorType::Ai)
    }

    fn human(self) -> ExpectedLine {
        ExpectedLine::new(self, AuthorType::Human)
    }

    fn unattributed_human(self) -> ExpectedLine {
        ExpectedLine::new(self, AuthorType::UnattributedHuman)
    }
}

impl ExpectedLineExt for ExpectedLine {
    fn ai(self) -> ExpectedLine {
        ExpectedLine::new(self.contents, AuthorType::Ai)
    }

    fn human(self) -> ExpectedLine {
        ExpectedLine::new(self.contents, AuthorType::Human)
    }

    fn unattributed_human(self) -> ExpectedLine {
        ExpectedLine::new(self.contents, AuthorType::UnattributedHuman)
    }
}

/// Default conversion from &str to ExpectedLine (defaults to Human authorship)
impl From<&str> for ExpectedLine {
    fn from(s: &str) -> Self {
        ExpectedLine::new(s.to_string(), AuthorType::Human)
    }
}

/// Default conversion from String to ExpectedLine (defaults to Human authorship)
impl From<String> for ExpectedLine {
    fn from(s: String) -> Self {
        ExpectedLine::new(s, AuthorType::Human)
    }
}

#[derive(Debug, Clone)]
pub struct TestFile<'a> {
    pub lines: Vec<ExpectedLine>,
    pub file_path: PathBuf,
    pub repo: &'a super::test_repo::TestRepo,
}

impl<'a> TestFile<'a> {
    pub fn new_with_filename(
        file_path: PathBuf,
        lines: Vec<ExpectedLine>,
        repo: &'a super::test_repo::TestRepo,
    ) -> Self {
        Self {
            lines,
            file_path,
            repo,
        }
    }

    /// Populate TestFile from an existing file by reading its contents and blame
    pub fn from_existing_file(file_path: PathBuf, repo: &'a super::test_repo::TestRepo) -> Self {
        // Check if file exists
        if !file_path.exists() {
            return Self {
                lines: vec![],
                file_path,
                repo,
            };
        }

        // Read file contents
        let contents = fs::read_to_string(&file_path).unwrap_or_default();
        if contents.is_empty() {
            return Self {
                lines: vec![],
                file_path,
                repo,
            };
        }

        // Run blame to get authorship
        let filename = file_path.to_str().expect("valid path");
        let blame_result = repo.git_ai(&["blame", filename]);

        let lines = if let Ok(blame_output) = blame_result {
            // Parse blame output to get authorship for each line
            let content_lines: Vec<&str> = contents.lines().collect();
            let blame_lines = Self::parse_blame_lines(&blame_output);

            content_lines
                .iter()
                .zip(blame_lines.iter())
                .map(|(content, (author, _))| {
                    let author_type = if Self::is_ai_author_helper(author) {
                        AuthorType::Ai
                    } else {
                        AuthorType::Human
                    };
                    ExpectedLine::new(content.to_string(), author_type)
                })
                .collect()
        } else {
            // No blame available, assume all human
            contents
                .lines()
                .map(|line| ExpectedLine::new(line.to_string(), AuthorType::Human))
                .collect()
        };

        Self {
            lines,
            file_path,
            repo,
        }
    }

    /// Helper function to check if an author string indicates AI authorship.
    fn is_ai_author_helper(author: &str) -> bool {
        is_ai_blame_author(author)
    }

    /// Static version of parse_blame_line for use in from_existing_file
    fn parse_blame_line_static(line: &str) -> (String, String) {
        parse_blame_line(line)
    }

    fn parse_blame_lines(blame_output: &str) -> Vec<(String, String)> {
        blame_output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(Self::parse_blame_line_static)
            .collect()
    }

    fn parse_committed_blame_lines(blame_output: &str) -> Vec<(String, String)> {
        Self::parse_blame_lines(blame_output)
            .into_iter()
            .filter(|(author, _)| author != "Not Committed Yet")
            .collect()
    }

    pub fn stage(&self) {
        self.repo
            .git(&["add", self.file_path.to_str().expect("valid path")])
            .expect("add file should succeed");
    }

    fn blame_output(&self) -> String {
        let filename = self.file_path.to_str().expect("valid path");
        self.repo
            .git_ai(&["blame", filename])
            .expect("git-ai blame should succeed")
    }

    pub fn assert_contents_expected(&self) {
        let contents = fs::read_to_string(&self.file_path).unwrap();
        assert_eq!(
            contents,
            self.contents(),
            "Unexpected contents in file: {}",
            self.file_path.display(),
        );
    }

    pub fn assert_blame_snapshot(&self) {
        let blame_output = self.blame_output();

        let formatted = self.format_blame_for_snapshot(&blame_output);
        assert_debug_snapshot!(formatted);
    }

    pub fn assert_lines_and_blame<T: Into<ExpectedLine>>(&mut self, lines: Vec<T>) {
        let expected_lines: Vec<ExpectedLine> = lines.into_iter().map(|l| l.into()).collect();
        let blame_output = self.blame_output();
        let actual_lines = Self::parse_blame_lines(&blame_output);

        Self::assert_blame_lines(
            &actual_lines,
            &expected_lines,
            &blame_output,
            "lines in blame output",
        );
    }

    /// Assert only committed lines (filters out uncommitted lines)
    /// Useful for partial staging tests where some lines aren't committed yet
    pub fn assert_committed_lines<T: Into<ExpectedLine>>(&mut self, lines: Vec<T>) {
        let expected_lines: Vec<ExpectedLine> = lines.into_iter().map(|l| l.into()).collect();
        let blame_output = self.blame_output();
        let committed_lines = Self::parse_committed_blame_lines(&blame_output);

        Self::assert_blame_lines(
            &committed_lines,
            &expected_lines,
            &blame_output,
            "committed lines",
        );
    }

    fn assert_blame_lines(
        actual_lines: &[(String, String)],
        expected_lines: &[ExpectedLine],
        blame_output: &str,
        line_description: &str,
    ) {
        assert_eq!(
            actual_lines.len(),
            expected_lines.len(),
            "Number of {} ({}) doesn't match expected ({})\nBlame output:\n{}",
            line_description,
            actual_lines.len(),
            expected_lines.len(),
            blame_output
        );

        for (i, ((actual_author, actual_content), expected_line)) in
            actual_lines.iter().zip(expected_lines).enumerate()
        {
            let line_num = i + 1;

            assert_eq!(
                actual_content.trim(),
                expected_line.contents.trim(),
                "Line {}: Content mismatch\nExpected: {:?}\nActual: {:?}\nFull blame output:\n{}",
                line_num,
                expected_line.contents,
                actual_content,
                blame_output
            );

            match &expected_line.author_type {
                AuthorType::Ai => {
                    assert!(
                        Self::is_ai_author_helper(actual_author),
                        "Line {}: Expected AI author but got '{}'\nExpected: {:?}\nActual content: {:?}\nFull blame output:\n{}",
                        line_num,
                        actual_author,
                        expected_line,
                        actual_content,
                        blame_output
                    );
                }
                AuthorType::Human | AuthorType::UnattributedHuman => {
                    assert!(
                        !Self::is_ai_author_helper(actual_author),
                        "Line {}: Expected Human author but got AI author '{}'\nExpected: {:?}\nActual content: {:?}\nFull blame output:\n{}",
                        line_num,
                        actual_author,
                        expected_line,
                        actual_content,
                        blame_output
                    );
                }
            }
        }
    }

    /// Format blame output for readable snapshots
    /// Format: Name of user\n\n$author 1) LINE CONTENTS\n$author 2) LINE CONTENTS
    pub fn format_blame_for_snapshot(&self, blame_output: &str) -> String {
        let mut result = String::new();
        let mut current_author: Option<String> = None;

        for (index, (author, content)) in Self::parse_blame_lines(blame_output)
            .into_iter()
            .enumerate()
        {
            let line_num = index + 1;

            // Add header when author changes
            if current_author.as_ref() != Some(&author) {
                if current_author.is_some() {
                    result.push('\n');
                }
                result.push_str(&format!("{}\n\n", author));
                current_author = Some(author.clone());
            }

            // Add the line with author prefix and line number
            result.push_str(&format!("{} {}) {}\n", author, line_num, content));
        }

        result
    }

    /// Parse a single blame line to extract author and content
    /// Format: sha (author date line_num) content
    pub fn parse_blame_line(&self, line: &str) -> (String, String) {
        Self::parse_blame_line_static(line)
    }

    /// Assert that the file at the given path matches the expected contents and authorship
    pub fn assert_blame_contents_expected(&self) {
        let blame_output = self.blame_output();

        // println!(
        //     "\n=== Git-AI Blame Output ===\n{}\n===========================\n",
        //     blame_output
        // );

        let lines_by_author: Vec<_> = Self::parse_blame_lines(&blame_output)
            .into_iter()
            .map(|(author, _)| author)
            .collect();

        // Compare with expected authorship
        assert_eq!(
            lines_by_author.len(),
            self.lines.len(),
            "Number of lines in blame output ({}) doesn't match expected ({})",
            lines_by_author.len(),
            self.lines.len()
        );

        for (i, (actual_author, expected_line)) in
            lines_by_author.iter().zip(&self.lines).enumerate()
        {
            let line_num = i + 1;
            match &expected_line.author_type {
                AuthorType::Ai => {
                    assert!(
                        Self::is_ai_author_helper(actual_author),
                        "Line {}: Expected AI author but got '{}'. Expected line: {:?}\n{}",
                        line_num,
                        actual_author,
                        expected_line,
                        blame_output
                    );
                }
                AuthorType::Human | AuthorType::UnattributedHuman => {
                    assert!(
                        !Self::is_ai_author_helper(actual_author),
                        "Line {}: Expected Human author but got AI author '{}'. Expected line: {:?}\n{}",
                        line_num,
                        actual_author,
                        expected_line,
                        blame_output
                    );
                }
            }
        }
    }

    /// Get lines with a specific author type
    pub fn lines_by_author(&self, author_type: AuthorType) -> Vec<&ExpectedLine> {
        self.lines
            .iter()
            .filter(|line| line.author_type == author_type)
            .collect()
    }

    /// Insert lines at the specified index, mutating the TestFile in place
    pub fn insert_at<T: Into<ExpectedLine>>(
        &mut self,
        starting_index: usize,
        lines: Vec<T>,
    ) -> &mut Self {
        let lines: Vec<ExpectedLine> = lines.into_iter().map(|l| l.into()).collect();

        if lines.is_empty() {
            panic!("[test internals] must insert > 0 lines")
        }

        // Build splits - indices where author type changes
        let mut splits: Vec<usize> = vec![0]; // Always start from index 0
        let mut last_author_type = &lines[0].author_type;

        for (i, line) in lines.iter().enumerate().skip(1) {
            if &line.author_type != last_author_type {
                splits.push(i);
                last_author_type = &line.author_type;
            }
        }

        // Current working state - start with the current file contents
        let mut cumulative_offset = 0;

        // Process each chunk of same-author lines
        for (chunk_idx, &split_start) in splits.iter().enumerate() {
            // Determine the end of this chunk (exclusive)
            let split_end = if chunk_idx + 1 < splits.len() {
                splits[chunk_idx + 1]
            } else {
                lines.len()
            };

            // Get the chunk of lines to insert (all have same author type)
            let chunk = &lines[split_start..split_end];
            let author_type = &chunk[0].author_type;

            // Insert this chunk into self.lines at the appropriate position
            let insert_position = starting_index + cumulative_offset;
            self.lines
                .splice(insert_position..insert_position, chunk.iter().cloned());

            // Write the current state to disk and create a checkpoint
            self.write_and_checkpoint(author_type);

            // Update offset for next insertion (next chunk starts after this one)
            cumulative_offset += chunk.len();
        }

        self
    }

    /// Replace a single line at the specified index, mutating the TestFile in place
    pub fn replace_at<T: Into<ExpectedLine>>(&mut self, index: usize, line: T) -> &mut Self {
        let line = line.into();
        if index < self.lines.len() {
            self.lines[index] = line.clone();
        } else {
            panic!(
                "Index {} out of bounds for {} lines",
                index,
                self.lines.len()
            );
        }

        // Write the updated content to disk and create a checkpoint
        self.write_and_checkpoint(&line.author_type);

        self
    }

    /// Delete a single line at the specified index, mutating the TestFile in place
    /// Deletions are always attributed to humans
    pub fn delete_at(&mut self, index: usize) -> &mut Self {
        if index < self.lines.len() {
            self.lines.remove(index);
        } else {
            panic!(
                "Index {} out of bounds for {} lines",
                index,
                self.lines.len()
            );
        }

        // Write the updated content to disk and create a checkpoint (deletions are human)
        self.write_and_checkpoint(&AuthorType::Human);

        self
    }

    /// Delete a range of lines [start..end), mutating the TestFile in place
    /// Deletions are always attributed to humans
    pub fn delete_range(&mut self, start: usize, end: usize) -> &mut Self {
        if start >= end {
            panic!(
                "[test internals] start index {} must be less than end index {}",
                start, end
            );
        }

        if end > self.lines.len() {
            panic!(
                "End index {} out of bounds for {} lines",
                end,
                self.lines.len()
            );
        }

        self.lines.drain(start..end);

        // Write the updated content to disk and create a checkpoint (deletions are human)
        self.write_and_checkpoint(&AuthorType::Human);

        self
    }

    pub fn set_contents<T: Into<ExpectedLine>>(&mut self, lines: Vec<T>) -> &mut Self {
        self.set_contents_with_staging(lines, true)
    }

    /// Set file contents without staging (but still creates checkpoints for authorship tracking)
    /// Useful for testing scenarios with precise staging control
    pub fn set_contents_no_stage<T: Into<ExpectedLine>>(&mut self, lines: Vec<T>) -> &mut Self {
        self.set_contents_with_staging(lines, false)
    }

    fn set_contents_with_staging<T: Into<ExpectedLine>>(
        &mut self,
        lines: Vec<T>,
        stage: bool,
    ) -> &mut Self {
        let lines: Vec<ExpectedLine> = lines.into_iter().map(|l| l.into()).collect();
        // stub in AI Lines
        let line_contents = lines
            .iter()
            .map(|s| {
                if s.author_type == AuthorType::Ai {
                    "||__AI LINE__ PENDING__||".to_string()
                } else {
                    s.contents.clone()
                }
            })
            .collect::<Vec<String>>()
            .join("\n");

        let human_kind = if lines
            .iter()
            .any(|l| l.author_type == AuthorType::UnattributedHuman)
        {
            &AuthorType::UnattributedHuman
        } else {
            &AuthorType::Human
        };
        self.write_and_checkpoint_with_contents(&line_contents, human_kind, stage);

        let line_contents_with_ai = lines
            .iter()
            .map(|s| s.contents.clone())
            .collect::<Vec<String>>()
            .join("\n");

        self.write_and_checkpoint_with_contents(&line_contents_with_ai, &AuthorType::Ai, stage);

        self.lines = lines;
        self
    }

    pub fn contents(&self) -> String {
        self.lines
            .iter()
            .map(|s| s.contents.clone())
            .collect::<Vec<String>>()
            .join("\n")
    }

    fn repo_relative_path(&self) -> String {
        self.file_path
            .strip_prefix(self.repo.path())
            .expect("test file path should be inside the test repo")
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn run_checkpoint_for_author_type(&self, author_type: &AuthorType) {
        let relative_path = self.repo_relative_path();
        let result = match author_type {
            AuthorType::Ai => self
                .repo
                .git_ai(&["checkpoint", "mock_ai", relative_path.as_str()]),
            AuthorType::Human => {
                self.repo
                    .git_ai(&["checkpoint", "mock_known_human", relative_path.as_str()])
            }
            AuthorType::UnattributedHuman => {
                self.repo
                    .git_ai(&["checkpoint", "--", relative_path.as_str()])
            }
        };
        result.unwrap();
    }

    fn write_and_checkpoint(&self, author_type: &AuthorType) {
        // Create parent directories if they don't exist (important for nested paths)
        if let Some(parent) = self.file_path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent).expect("failed to create parent directories");
        }
        let contents = self.contents();
        fs::write(&self.file_path, contents).unwrap();
        self.run_checkpoint_for_author_type(author_type);
    }

    fn write_and_checkpoint_with_contents(
        &self,
        contents: &str,
        author_type: &AuthorType,
        stage: bool,
    ) {
        // Create parent directories if they don't exist (important for nested paths like src/模块/组件.ts)
        if let Some(parent) = self.file_path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent).expect("failed to create parent directories");
        }
        fs::write(&self.file_path, contents).unwrap();

        if stage {
            // Stage the file first
            self.repo.git(&["add", "-A"]).unwrap();
        }

        self.run_checkpoint_for_author_type(author_type);
    }
}

/// Macro to create a Vec<ExpectedLine> from mixed types
/// Accepts ExpectedLine or any type convertible to ExpectedLine (e.g., &str, String)
/// Plain strings default to Human authorship
#[macro_export]
macro_rules! lines {
    ($($line:expr),* $(,)?) => {{
        {
            use $crate::repos::test_file::ExpectedLine;
            let v: Vec<ExpectedLine> = vec![$(Into::into($line)),*];
            v
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::TestFile;

    #[test]
    fn blame_parser_preserves_multi_word_author_and_email() {
        let line = "abc123 (Jane Mary Doe <jane@example.com> 2026-07-22 12:00:00 -0400 1) content";

        assert_eq!(
            TestFile::parse_blame_line_static(line),
            (
                "Jane Mary Doe <jane@example.com>".to_string(),
                "content".to_string(),
            )
        );
    }

    #[test]
    fn ai_detection_ignores_agent_names_inside_email_addresses() {
        assert!(!TestFile::is_ai_author_helper(
            "Human Developer <amp@example.com>"
        ));
        assert!(TestFile::is_ai_author_helper(
            "GitHub Copilot <human@example.com>"
        ));
    }

    #[test]
    fn malformed_blame_line_is_returned_as_unknown() {
        let line = "abc123 (Jane Doe 2026-07-22 12:00:00 -0400 1 content";

        assert_eq!(
            TestFile::parse_blame_line_static(line),
            ("unknown".to_string(), line.to_string())
        );
    }

    #[test]
    fn committed_line_filter_only_removes_not_committed_yet() {
        let blame_output = "\
abc123 (Jane Doe 2026-07-22 12:00:00 -0400 1) committed human
000000 (Not Committed Yet 2026-07-22 12:00:00 -0400 2) pending
def456 (mock_ai 2026-07-22 12:00:00 -0400 3) committed ai
";

        let all_lines = TestFile::parse_blame_lines(blame_output);
        let committed_lines = TestFile::parse_committed_blame_lines(blame_output);

        assert_eq!(all_lines.len(), 3);
        assert_eq!(all_lines[1].0, "Not Committed Yet");
        assert_eq!(
            committed_lines,
            vec![
                ("Jane Doe".to_string(), "committed human".to_string()),
                ("mock_ai".to_string(), "committed ai".to_string()),
            ]
        );
    }
}
