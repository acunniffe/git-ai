use indicatif::{ProgressBar, ProgressStyle};

/// Spinner UI component for showing progress
pub struct Spinner {
    pb: ProgressBar,
}

impl Spinner {
    pub fn new(message: &str) -> Self {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        pb.set_message(message.to_string());
        pb.enable_steady_tick(std::time::Duration::from_millis(100));

        Self { pb }
    }

    pub fn start(&self) {
        // Spinner starts automatically when created
    }

    pub fn finish_and_clear(&self) {
        self.pb.finish_and_clear();
    }

    #[allow(dead_code)]
    pub fn update_message(&self, message: &str) {
        self.pb.set_message(message.to_string());
    }

    #[allow(dead_code)]
    pub async fn wait_for(&self, duration_ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;
    }

    pub fn success(&self, message: &str) {
        // Clear spinner and show success with green checkmark and bold green text
        self.pb.finish_and_clear();
        println!("\x1b[1;32m✓ {}\x1b[0m", message);
    }

    pub fn pending(&self, message: &str) {
        // Clear spinner and show pending with yellow warning triangle and bold yellow text
        self.pb.finish_and_clear();
        println!("\x1b[1;33m⚠ {}\x1b[0m", message);
    }

    pub fn error(&self, message: &str) {
        // Clear spinner and show error with red X and bold red text
        self.pb.finish_and_clear();
        println!("\x1b[1;31m✗ {}\x1b[0m", message);
    }

    #[allow(dead_code)]
    pub fn skipped(&self, message: &str) {
        // Clear spinner and show skipped with gray circle and gray text
        self.pb.finish_and_clear();
        println!("\x1b[90m○ {}\x1b[0m", message);
    }
}

/// Print a formatted diff using colors
pub fn print_diff(diff_text: &str) {
    // Print a formatted diff using colors
    for line in diff_text.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            // File headers in bold
            println!("\x1b[1m{}\x1b[0m", line);
        } else if line.starts_with('+') {
            // Additions in green
            println!("\x1b[32m{}\x1b[0m", line);
        } else if line.starts_with('-') {
            // Deletions in red
            println!("\x1b[31m{}\x1b[0m", line);
        } else if line.starts_with("@@") {
            // Hunk headers in cyan
            println!("\x1b[36m{}\x1b[0m", line);
        } else {
            // Context lines normal
            println!("{}", line);
        }
    }
    println!(); // Blank line after diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spinner_creation() {
        let spinner = Spinner::new("Testing spinner");
        // Just verify it doesn't panic
        spinner.start();
    }

    #[test]
    fn test_spinner_success_output() {
        let spinner = Spinner::new("Processing");
        // Verify success message doesn't panic
        spinner.success("Operation completed successfully");
    }

    #[test]
    fn test_spinner_pending_output() {
        let spinner = Spinner::new("Processing");
        spinner.pending("Pending action required");
    }

    #[test]
    fn test_spinner_error_output() {
        let spinner = Spinner::new("Processing");
        spinner.error("An error occurred");
    }

    #[test]
    fn test_spinner_skipped_output() {
        let spinner = Spinner::new("Processing");
        spinner.skipped("Operation skipped");
    }

    #[test]
    fn test_spinner_update_message() {
        let spinner = Spinner::new("Initial message");
        spinner.update_message("Updated message");
        spinner.success("Done");
    }

    #[test]
    fn test_print_diff_additions() {
        let diff = "+new line\n+another new line";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_deletions() {
        let diff = "-removed line\n-another removed line";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_file_headers() {
        let diff = "--- a/file.txt\n+++ b/file.txt";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_hunk_headers() {
        let diff = "@@ -1,3 +1,4 @@";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_context_lines() {
        let diff = " context line 1\n context line 2";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_complete() {
        let diff = "--- a/test.txt\n+++ b/test.txt\n@@ -1,3 +1,4 @@\n context\n-old line\n+new line\n context";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_empty() {
        let diff = "";
        print_diff(diff);
    }

    #[test]
    fn test_print_diff_multiline() {
        let diff = "--- a/file.rs\n+++ b/file.rs\n@@ -10,5 +10,6 @@\n fn main() {\n-    println!(\"old\");\n+    println!(\"new\");\n+    println!(\"extra\");\n }";
        print_diff(diff);
    }
}
