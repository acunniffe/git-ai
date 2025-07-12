/// Debug logging utility function
///
/// Prints debug messages with a colored prefix when debug assertions are enabled.
/// This function only outputs messages when the code is compiled with debug assertions.
///
/// # Arguments
///
/// * `msg` - The debug message to print
pub fn debug_log(msg: &str) {
    if cfg!(debug_assertions) {
        eprintln!("\x1b[1;33m[git-ai]\x1b[0m {}", msg);
    }
}
