#[macro_use]
pub mod test_file;
pub mod diff_hostility;
pub mod test_repo;
pub(crate) mod test_repo_adapters;

#[macro_export]
macro_rules! subdir_test_variants {
    (
        fn $test_name:ident() $body:block
    ) => {
        paste::paste! {
            // Variant 1: Run from the repository root.
            #[test]
            fn [<test_ $test_name _from_root>]() {
                $crate::repos::test_repo_adapters::with_repo_invocation(
                    $crate::repos::test_repo_adapters::RepoInvocation::Root,
                    || {
                        type TestRepo =
                            $crate::repos::test_repo_adapters::InvocationTestRepo;
                        $body
                    },
                );
            }

            // Variant 1b: Run from the repository root in worktree mode.
            #[test]
            fn [<test_ $test_name _from_root_in_worktree>]() {
                $crate::repos::test_repo::with_worktree_mode(|| {
                    [<test_ $test_name _from_root>]()
                });
            }

            // Variant 2: Run from a subdirectory (original behavior).
            #[test]
            fn [<test_ $test_name _from_subdir>]() {
                $crate::repos::test_repo_adapters::with_repo_invocation(
                    $crate::repos::test_repo_adapters::RepoInvocation::Subdirectory,
                    || {
                        type TestRepo =
                            $crate::repos::test_repo_adapters::InvocationTestRepo;
                        $body
                    },
                );
            }

            // Variant 2b: Run from a subdirectory with a worktree-backed repo.
            #[test]
            fn [<test_ $test_name _from_subdir_in_worktree>]() {
                $crate::repos::test_repo::with_worktree_mode(|| {
                    [<test_ $test_name _from_subdir>]();
                });
            }

            // Variant 3: Run with -C flag from an arbitrary directory.
            #[test]
            fn [<test_ $test_name _with_c_flag>]() {
                $crate::repos::test_repo_adapters::with_repo_invocation(
                    $crate::repos::test_repo_adapters::RepoInvocation::CFlag,
                    || {
                        type TestRepo =
                            $crate::repos::test_repo_adapters::InvocationTestRepo;
                        $body
                    },
                );
            }

            // Variant 3b: Run with -C flag from an arbitrary directory in worktree mode.
            #[test]
            fn [<test_ $test_name _with_c_flag_in_worktree>]() {
                $crate::repos::test_repo::with_worktree_mode(|| {
                    [<test_ $test_name _with_c_flag>]();
                });
            }
        }
    };
}

#[macro_export]
macro_rules! worktree_test_wrappers {
    (
        fn $test_name:ident() $body:block
    ) => {
        paste::paste! {
            #[test]
            fn [<test_ $test_name _in_worktree_daemon_mode>]() {
                type TestRepo =
                    $crate::repos::test_repo_adapters::WorktreeTestRepo;
                $body
            }
        }
    };
}

#[macro_export]
macro_rules! reuse_tests_in_worktree {
    (
        $( $test_name:ident ),+ $(,)?
    ) => {
        paste::paste! {
            $(
                #[test]
                fn [<$test_name _in_worktree>]() {
                    $crate::repos::test_repo::with_worktree_mode(|| {
                        $test_name();
                    })
                }
            )+
        }
    };
}

#[macro_export]
macro_rules! reuse_tests_in_worktree_with_attrs {
    (
        ($($attrs:tt)*)
        $test_name:ident
        $(, $rest:ident)* $(,)?
    ) => {
        $crate::reuse_tests_in_worktree_with_attrs!(@one ($($attrs)*) $test_name);
        $crate::reuse_tests_in_worktree_with_attrs!(($($attrs)*) $($rest),*);
    };
    (
        ($($attrs:tt)*)
    ) => {
    };
    (@one ($($attrs:tt)*) $test_name:ident) => {
        paste::paste! {
            $($attrs)*
            #[test]
            fn [<$test_name _in_worktree>]() {
                $crate::repos::test_repo::with_worktree_mode(|| {
                    $test_name();
                })
            }
        }
    };
}
