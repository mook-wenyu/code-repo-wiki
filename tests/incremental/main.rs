//! 增量构建功能域集成测试聚合入口（U1：tests 按功能域拆子目录）
#![cfg(test)]

#[path = "../common/mod.rs"]
mod common;

mod test_git_sync;
mod test_incremental_git_e2e;
mod test_incremental_large_fixture;
mod test_incremental_unchanged_backfill;
mod test_watch_e2e;
