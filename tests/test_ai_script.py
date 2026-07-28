import importlib.machinery
import importlib.util
import io
import os
import shlex
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from types import SimpleNamespace
from unittest import mock


SCRIPT_PATH = Path(__file__).parents[1] / "scripts" / "ai"
LOADER = importlib.machinery.SourceFileLoader("ai_script", str(SCRIPT_PATH))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
ai_script = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(ai_script)


class ReviewParserTests(unittest.TestCase):
    def test_review_accepts_pr_number_and_preserves_additional_prompt(self):
        additional_prompt = "  focus on async safety\nand keep this spacing  "

        args = ai_script.build_parser().parse_args(
            ["review", "123", additional_prompt, "--harness", "claude"]
        )

        self.assertEqual(args.pr_number, 123)
        self.assertEqual(args.additional_prompt, additional_prompt)
        self.assertEqual(args.harness, "claude")
        self.assertIsNone(args.heavy)

    def test_bare_heavy_defaults_to_two_reviewers_per_harness(self):
        args = ai_script.build_parser().parse_args(["review", "123", "--heavy"])

        self.assertEqual(args.heavy, 2)

    def test_heavy_accepts_one_to_three_reviewers_per_harness(self):
        parser = ai_script.build_parser()

        for reviewer_count in (1, 2, 3):
            with self.subTest(reviewer_count=reviewer_count):
                args = parser.parse_args(
                    ["review", "123", "--heavy", str(reviewer_count)]
                )
                self.assertEqual(args.heavy, reviewer_count)

    def test_heavy_rejects_counts_outside_one_to_three(self):
        parser = ai_script.build_parser()

        for reviewer_count in (0, 4):
            with self.subTest(reviewer_count=reviewer_count):
                with self.assertRaises(SystemExit):
                    with redirect_stderr(io.StringIO()):
                        parser.parse_args(
                            ["review", "123", "--heavy", str(reviewer_count)]
                        )


class ReviewCommandTests(unittest.TestCase):
    def test_harness_commands_preserve_existing_launch_arguments(self):
        self.assertEqual(
            ai_script.build_harness_command(
                "codex",
                resume_session=True,
                prompt="continue",
            ),
            [
                "codex",
                "resume",
                "--dangerously-bypass-approvals-and-sandbox",
                "continue",
            ],
        )
        self.assertEqual(
            ai_script.build_harness_command(
                "claude",
                resume_session=True,
                prompt="continue",
            ),
            [
                "claude",
                "--dangerously-skip-permissions",
                "--resume",
                "continue",
            ],
        )

    def test_review_builds_exact_agent_prompt(self):
        self.assertEqual(ai_script.build_review_prompt(123), "/review PR#123")
        self.assertEqual(
            ai_script.build_review_prompt(123, "  inspect races\ncarefully  "),
            "/review PR#123   inspect races\ncarefully  ",
        )

    @mock.patch.object(ai_script, "open_worktree")
    @mock.patch.object(
        ai_script,
        "checkout_pr_worktree",
        return_value=("/repo/.worktrees/topic", "topic"),
    )
    def test_review_reuses_pr_checkout_and_opens_selected_harness(
        self, checkout_pr_worktree, open_worktree
    ):
        args = SimpleNamespace(
            pr_number=123,
            additional_prompt="focus on tests",
            harness="claude",
            heavy=None,
        )

        ai_script.cmd_review(args)

        checkout_pr_worktree.assert_called_once_with(123)
        open_worktree.assert_called_once_with(
            "/repo/.worktrees/topic",
            "topic",
            harness="claude",
            prompt="/review PR#123 focus on tests",
        )

    @mock.patch.object(ai_script, "open_heavy_review")
    @mock.patch.object(
        ai_script,
        "checkout_pr_worktree",
        return_value=("/repo/.worktrees/pr-123", "contributor-topic"),
    )
    def test_heavy_review_dispatches_requested_count_for_each_harness(
        self, checkout_pr_worktree, open_heavy_review
    ):
        args = SimpleNamespace(
            pr_number=123,
            additional_prompt=None,
            harness="codex",
            heavy=3,
        )

        ai_script.cmd_review(args)

        checkout_pr_worktree.assert_called_once_with(123)
        open_heavy_review.assert_called_once_with(
            "/repo/.worktrees/pr-123",
            "contributor-topic",
            "/review PR#123",
            3,
        )

    @mock.patch.object(ai_script, "open_worktree")
    @mock.patch.object(
        ai_script,
        "checkout_pr_worktree",
        return_value=("/repo/.worktrees/topic", "topic"),
    )
    def test_pr_command_still_uses_shared_checkout(
        self, checkout_pr_worktree, open_worktree
    ):
        ai_script.cmd_pr(SimpleNamespace(pr_number=123, harness="codex"))

        checkout_pr_worktree.assert_called_once_with(123)
        open_worktree.assert_called_once_with(
            "/repo/.worktrees/topic",
            "topic",
            harness="codex",
            resume_session=False,
        )


class HeavyReviewTests(unittest.TestCase):
    @mock.patch.object(ai_script.os, "execvp")
    @mock.patch.object(ai_script.os, "chdir")
    @mock.patch.object(ai_script, "set_tmux_window_name")
    @mock.patch.object(ai_script, "run")
    def test_launches_requested_codex_and_claude_reviewers_in_tiled_panes(
        self, run, set_tmux_window_name, chdir, execvp
    ):
        for reviewer_count in (1, 2, 3):
            with self.subTest(reviewer_count=reviewer_count):
                run.reset_mock()
                set_tmux_window_name.reset_mock()
                chdir.reset_mock()
                execvp.reset_mock()

                with mock.patch.dict(
                    os.environ, {"TMUX": "/tmp/tmux.sock"}, clear=False
                ):
                    with redirect_stdout(io.StringIO()):
                        ai_script.open_heavy_review(
                            "/repo/.worktrees/topic",
                            "topic",
                            "/review PR#123 preserve whitespace",
                            reviewer_count,
                        )

                set_tmux_window_name.assert_called_once_with("topic")
                chdir.assert_called_once_with("/repo/.worktrees/topic")

                split_calls = [
                    call.args[0]
                    for call in run.call_args_list
                    if call.args[0][0:2] == ["tmux", "split-window"]
                ]
                expected_split_harnesses = (
                    ["codex", "claude"] * reviewer_count
                )[1:]
                self.assertEqual(
                    len(split_calls),
                    reviewer_count * 2 - 1,
                )
                split_harnesses = [
                    shlex.split(call[-1])[0] for call in split_calls
                ]
                self.assertEqual(
                    split_harnesses,
                    expected_split_harnesses,
                )
                for call in split_calls:
                    self.assertEqual(
                        call[3:5],
                        ["-c", "/repo/.worktrees/topic"],
                    )
                    self.assertEqual(
                        shlex.split(call[-1])[-1],
                        "/review PR#123 preserve whitespace",
                    )

                run.assert_has_calls(
                    [mock.call(["tmux", "select-layout", "tiled"])]
                )
                execvp.assert_called_once_with(
                    "codex",
                    [
                        "codex",
                        "--dangerously-bypass-approvals-and-sandbox",
                        "/review PR#123 preserve whitespace",
                    ],
                )

    @mock.patch.object(ai_script.os, "execvp")
    @mock.patch.object(ai_script, "run")
    def test_heavy_review_requires_current_tmux_window(self, run, execvp):
        with mock.patch.dict(os.environ, {}, clear=True):
            with redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    ai_script.open_heavy_review(
                        "/repo/.worktrees/topic",
                        "topic",
                        "/review PR#123",
                        2,
                    )

        run.assert_not_called()
        execvp.assert_not_called()


if __name__ == "__main__":
    unittest.main()
