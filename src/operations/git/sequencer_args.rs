const CONTROL_MODES: &[&str] = &["--abort", "--continue", "--quit", "--skip"];
const CHERRY_PICK_VALUE_OPTIONS: &[&str] =
    &["-m", "--mainline", "-X", "--strategy-option", "--strategy"];
const REVERT_VALUE_OPTIONS: &[&str] = &["-m", "--mainline"];

pub(crate) fn cherry_pick_source_args(args: &[String]) -> Vec<&str> {
    source_args(
        strip_leading_command(args, "cherry-pick"),
        CHERRY_PICK_VALUE_OPTIONS,
    )
}

pub(crate) fn cherry_pick_source_args_from_command_args(args: &[String]) -> Vec<&str> {
    source_args(args, CHERRY_PICK_VALUE_OPTIONS)
}

pub(crate) fn revert_source_args(args: &[String]) -> Vec<&str> {
    source_args(strip_leading_command(args, "revert"), REVERT_VALUE_OPTIONS)
}

fn strip_leading_command<'a>(args: &'a [String], command: &str) -> &'a [String] {
    if args.first().is_some_and(|arg| arg == command) {
        &args[1..]
    } else {
        args
    }
}

fn source_args<'a>(args: &'a [String], value_options: &[&str]) -> Vec<&'a str> {
    let mut sources = Vec::new();
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--" {
            sources.extend(args[idx + 1..].iter().map(String::as_str));
            break;
        }
        if CONTROL_MODES.contains(&arg) {
            return Vec::new();
        }
        if value_options.contains(&arg) {
            idx = idx.saturating_add(2);
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        if !arg.is_empty() {
            sources.push(arg);
        }
        idx += 1;
    }
    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn cherry_pick_command_inclusive_args_strip_only_the_leading_command() {
        assert_eq!(
            cherry_pick_source_args(&args(&["cherry-pick", "HEAD~1", "HEAD~2"])),
            vec!["HEAD~1", "HEAD~2"]
        );
        assert_eq!(
            cherry_pick_source_args(&args(&["cherry-pick"])),
            Vec::<&str>::new()
        );
        assert_eq!(
            cherry_pick_source_args(&args(&["topic", "cherry-pick"])),
            vec!["topic", "cherry-pick"]
        );
    }

    #[test]
    fn cherry_pick_command_args_preserve_a_source_named_like_the_command() {
        assert_eq!(
            cherry_pick_source_args_from_command_args(&args(&["cherry-pick", "HEAD~1"])),
            vec!["cherry-pick", "HEAD~1"]
        );
    }

    #[test]
    fn sequencer_value_options_consume_only_their_separate_values() {
        assert_eq!(
            cherry_pick_source_args(&args(&[
                "-m",
                "1",
                "-X",
                "renormalize",
                "--strategy",
                "ort",
                "--strategy-option=patience",
                "HEAD~1",
            ])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args(&args(&["revert", "--mainline", "1", "HEAD~1"])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            cherry_pick_source_args(&args(&[
                "-m1",
                "-Xrenormalize",
                "--mainline=1",
                "--strategy=ort",
                "HEAD~1",
            ])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args(&args(&["--mainline=1", "HEAD~1"])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            cherry_pick_source_args(&args(&["--strategy"])),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn bare_gpg_sign_does_not_consume_the_following_source() {
        for flag in ["--gpg-sign", "-S", "-Smy-key"] {
            assert_eq!(
                cherry_pick_source_args(&args(&[flag, "HEAD~1"])),
                vec!["HEAD~1"]
            );
            assert_eq!(revert_source_args(&args(&[flag, "HEAD~1"])), vec!["HEAD~1"]);
        }
    }

    #[test]
    fn control_modes_clear_sources_before_the_end_of_options_marker() {
        assert_eq!(
            cherry_pick_source_args(&args(&["topic", "--abort"])),
            Vec::<&str>::new()
        );
        assert_eq!(
            revert_source_args(&args(&["topic", "--quit"])),
            Vec::<&str>::new()
        );
        assert_eq!(
            cherry_pick_source_args(&args(&["--", "--continue", ""])),
            vec!["--continue", ""]
        );
    }

    #[test]
    fn unknown_flags_and_empty_pre_marker_args_are_ignored() {
        assert_eq!(
            cherry_pick_source_args(&args(&["", "--unknown", "HEAD~1"])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args(&args(&["revert", "", "-n", "HEAD~1"])),
            vec!["HEAD~1"]
        );
    }
}
