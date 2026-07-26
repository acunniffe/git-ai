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
            // Variant 1: Run from subdirectory (original behavior)
            #[test]
            fn [<test_ $test_name _from_subdir>]() $body

            // Variant 1b: Run from subdirectory with a worktree-backed repo
            #[test]
            fn [<test_ $test_name _from_subdir_in_worktree>]() {
                $crate::repos::test_repo::with_worktree_mode(|| {
                    [<test_ $test_name _from_subdir>]();
                });
            }

            // Variant 2: Run with -C flag from arbitrary directory
            #[test]
            fn [<test_ $test_name _with_c_flag>]() {
                type TestRepo =
                    $crate::repos::test_repo_adapters::TestRepoWithCFlag;
                $body
            }

            // Variant 2b: Run with -C flag from arbitrary directory in worktree mode
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
