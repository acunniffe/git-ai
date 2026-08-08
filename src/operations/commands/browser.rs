use std::process::Command;

#[allow(dead_code)]
pub(super) fn browser_command(url: &str) -> Command {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = Command::new("cmd");
    #[cfg(target_os = "windows")]
    command.args(["/C", "start", ""]);

    command.arg(url);
    command
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::browser_command;

    #[test]
    fn browser_command_uses_platform_program_and_arguments() {
        let url = "https://example.com/path";
        let command = browser_command(url);

        #[cfg(target_os = "macos")]
        let (program, args) = ("open", &[url][..]);
        #[cfg(target_os = "linux")]
        let (program, args) = ("xdg-open", &[url][..]);
        #[cfg(target_os = "windows")]
        let (program, args) = ("cmd", &["/C", "start", "", url][..]);

        assert_eq!(command.get_program(), OsStr::new(program));
        assert!(command.get_args().eq(args.iter().map(OsStr::new)));
    }
}
