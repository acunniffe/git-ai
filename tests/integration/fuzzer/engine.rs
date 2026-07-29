use rand::rngs::StdRng;
use rand::{Rng, RngExt, SeedableRng};

use crate::repos::test_repo::TestRepo;

use super::model::{AttrRegistry, FileModel};
use super::operations::{self, CharAllocator};

#[derive(Debug, Clone)]
pub struct FuzzerConfig {
    pub seed: u64,
    pub ops: usize,
    pub max_lines_per_edit: usize,
    pub rewrite_weight: u32,
}

impl FuzzerConfig {
    pub fn standard(seed: u64, ops: usize) -> Self {
        Self {
            seed,
            ops,
            max_lines_per_edit: 4,
            rewrite_weight: 30,
        }
    }

    pub fn rewrite_heavy(seed: u64, ops: usize) -> Self {
        Self {
            seed,
            ops,
            max_lines_per_edit: 3,
            rewrite_weight: 70,
        }
    }

    pub fn chaos(seed: u64, ops: usize) -> Self {
        Self {
            seed,
            ops,
            max_lines_per_edit: 6,
            rewrite_weight: 85,
        }
    }
}

pub fn run_fuzzer(config: FuzzerConfig) {
    // The fuzzer intentionally performs many checkpoint and rewrite operations.
    // Keep its synchronization traffic off the shared pool so it cannot delay
    // unrelated integration tests running in parallel.
    let repo = TestRepo::new_dedicated_daemon();
    let mut rng = StdRng::seed_from_u64(config.seed);
    let mut alloc = CharAllocator::new();
    let mut registry = AttrRegistry::new();
    let mut op_log: Vec<String> = Vec::new();
    let mut model = FileModel::new("fuzz.txt");

    // Initial content: create file with a few lines, checkpoint as AI, commit
    let initial_chars =
        operations::random_edit(&mut model, &mut registry, &repo, &mut alloc, &mut rng, 3);
    operations::checkpoint_ai(
        &mut model,
        &mut registry,
        &repo,
        &initial_chars,
        &mut op_log,
    );
    operations::commit(
        &mut model,
        &mut registry,
        &repo,
        &mut op_log,
        config.seed,
        "initial commit",
    );

    for i in 0..config.ops {
        let op = pick_operation(&mut rng, &config);
        op_log.push(format!("--- op {} ({:?}) ---", i, op));

        match op {
            Op::EditCommitAi => {
                let chars = operations::random_edit(
                    &mut model,
                    &mut registry,
                    &repo,
                    &mut alloc,
                    &mut rng,
                    config.max_lines_per_edit,
                );
                operations::checkpoint_ai(&mut model, &mut registry, &repo, &chars, &mut op_log);
                operations::commit(
                    &mut model,
                    &mut registry,
                    &repo,
                    &mut op_log,
                    config.seed,
                    &format!("ai edit {}", i),
                );
            }
            Op::EditCommitHuman => {
                let chars = operations::random_edit(
                    &mut model,
                    &mut registry,
                    &repo,
                    &mut alloc,
                    &mut rng,
                    config.max_lines_per_edit,
                );
                operations::checkpoint_human(&mut model, &mut registry, &repo, &chars, &mut op_log);
                operations::commit(
                    &mut model,
                    &mut registry,
                    &repo,
                    &mut op_log,
                    config.seed,
                    &format!("human edit {}", i),
                );
            }
            Op::EditCommitUntracked => {
                let _chars = operations::random_edit(
                    &mut model,
                    &mut registry,
                    &repo,
                    &mut alloc,
                    &mut rng,
                    config.max_lines_per_edit,
                );
                operations::checkpoint_untracked(&model, &repo, &mut op_log);
                operations::commit(
                    &mut model,
                    &mut registry,
                    &repo,
                    &mut op_log,
                    config.seed,
                    &format!("untracked edit {}", i),
                );
            }
            Op::Amend => {
                let chars = operations::random_edit(
                    &mut model,
                    &mut registry,
                    &repo,
                    &mut alloc,
                    &mut rng,
                    config.max_lines_per_edit,
                );
                operations::checkpoint_ai(&mut model, &mut registry, &repo, &chars, &mut op_log);
                operations::amend(&mut model, &mut registry, &repo, &mut op_log, config.seed);
            }
            Op::Rebase => {
                operations::rebase(
                    &mut model,
                    &mut registry,
                    &repo,
                    &mut alloc,
                    &mut rng,
                    &mut op_log,
                    config.seed,
                );
            }
            Op::CherryPick => {
                operations::cherry_pick(
                    &mut model,
                    &mut registry,
                    &repo,
                    &mut alloc,
                    &mut rng,
                    &mut op_log,
                    config.seed,
                );
            }
            Op::SoftResetRecommit => {
                operations::soft_reset_recommit(
                    &mut model,
                    &registry,
                    &repo,
                    &mut op_log,
                    config.seed,
                );
            }
            Op::StashRoundtrip => {
                operations::stash_roundtrip(
                    &mut model,
                    &mut registry,
                    &repo,
                    &mut alloc,
                    &mut rng,
                    &mut op_log,
                    config.seed,
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    EditCommitAi,
    EditCommitHuman,
    EditCommitUntracked,
    Amend,
    Rebase,
    CherryPick,
    SoftResetRecommit,
    StashRoundtrip,
}

fn pick_operation(rng: &mut impl Rng, config: &FuzzerConfig) -> Op {
    let total = 100;
    let rewrite = config.rewrite_weight;
    let standard = total - rewrite;

    let roll = rng.random_range(0..total);

    if roll < standard {
        match rng.random_range(0..10) {
            0..5 => Op::EditCommitAi,
            5..8 => Op::EditCommitHuman,
            _ => Op::EditCommitUntracked,
        }
    } else {
        match rng.random_range(0..5) {
            0 => Op::Amend,
            1 => Op::Rebase,
            2 => Op::CherryPick,
            3 => Op::SoftResetRecommit,
            _ => Op::StashRoundtrip,
        }
    }
}
