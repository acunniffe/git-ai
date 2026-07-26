const CONTROL_MODES: &[&str] = &["--abort", "--continue", "--quit", "--skip"];
const CHERRY_PICK_VALUE_OPTIONS: &[&str] = &[
    "-m",
    "--mainline",
    "-X",
    "--strategy-option",
    "--strategy",
    "--cleanup",
    "--empty",
];
const REVERT_VALUE_OPTIONS: &[&str] = &[
    "-m",
    "--mainline",
    "-X",
    "--strategy-option",
    "--strategy",
    "--cleanup",
];

pub(crate) fn cherry_pick_source_args_from_command_args(args: &[String]) -> Vec<&str> {
    source_args(args, CHERRY_PICK_VALUE_OPTIONS)
}

pub(crate) fn revert_source_args_from_command_args(args: &[String]) -> Vec<&str> {
    source_args(args, REVERT_VALUE_OPTIONS)
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
    fn command_args_treat_command_names_as_sources() {
        assert_eq!(
            cherry_pick_source_args_from_command_args(&args(&["cherry-pick", "HEAD~1"])),
            vec!["cherry-pick", "HEAD~1"]
        );
        assert_eq!(
            revert_source_args_from_command_args(&args(&["revert", "HEAD~1"])),
            vec!["revert", "HEAD~1"]
        );
    }

    #[test]
    fn sequencer_value_options_consume_only_their_separate_values() {
        assert_eq!(
            cherry_pick_source_args_from_command_args(&args(&[
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
            revert_source_args_from_command_args(&args(&["--mainline", "1", "HEAD~1"])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            cherry_pick_source_args_from_command_args(&args(&[
                "-m1",
                "-Xrenormalize",
                "--mainline=1",
                "--strategy=ort",
                "HEAD~1",
            ])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args_from_command_args(&args(&["--mainline=1", "HEAD~1"])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            cherry_pick_source_args_from_command_args(&args(&["--strategy"])),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn bare_gpg_sign_does_not_consume_the_following_source() {
        for flag in ["--gpg-sign", "-S", "-Smy-key"] {
            assert_eq!(
                cherry_pick_source_args_from_command_args(&args(&[flag, "HEAD~1"])),
                vec!["HEAD~1"]
            );
            assert_eq!(
                revert_source_args_from_command_args(&args(&[flag, "HEAD~1"])),
                vec!["HEAD~1"]
            );
        }
    }

    #[test]
    fn control_modes_clear_sources_before_the_end_of_options_marker() {
        assert_eq!(
            cherry_pick_source_args_from_command_args(&args(&["topic", "--abort"])),
            Vec::<&str>::new()
        );
        assert_eq!(
            revert_source_args_from_command_args(&args(&["topic", "--quit"])),
            Vec::<&str>::new()
        );
        assert_eq!(
            cherry_pick_source_args_from_command_args(&args(&["--", "--continue", ""])),
            vec!["--continue", ""]
        );
    }

    #[test]
    fn unknown_flags_and_empty_pre_marker_args_are_ignored() {
        assert_eq!(
            cherry_pick_source_args_from_command_args(&args(&["", "--unknown", "HEAD~1"])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args_from_command_args(&args(&["", "-n", "HEAD~1"])),
            vec!["HEAD~1"]
        );
    }

    #[test]
    fn sequencer_options_with_separate_values_do_not_become_sources() {
        assert_eq!(
            cherry_pick_source_args_from_command_args(&args(&[
                "--cleanup",
                "scissors",
                "--empty",
                "keep",
                "HEAD~1",
            ])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args_from_command_args(&args(&[
                "--cleanup",
                "scissors",
                "--strategy",
                "ort",
                "-X",
                "renormalize",
                "HEAD~1",
            ])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args_from_command_args(&args(&[
                "--strategy-option",
                "patience",
                "HEAD~1",
            ])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            cherry_pick_source_args_from_command_args(&args(&[
                "--cleanup=scissors",
                "--empty=keep",
                "HEAD~1",
            ])),
            vec!["HEAD~1"]
        );
        assert_eq!(
            revert_source_args_from_command_args(&args(&[
                "--cleanup=scissors",
                "--strategy=ort",
                "-Xrenormalize",
                "--strategy-option=patience",
                "HEAD~1",
            ])),
            vec!["HEAD~1"]
        );
    }
}
