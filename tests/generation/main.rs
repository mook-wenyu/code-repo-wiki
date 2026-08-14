//! generation 生成功能域集成测试聚合入口（U1：tests 按功能域拆子目录）
#![cfg(test)]

#[path = "../common/mod.rs"]
mod common;

mod integration_test;
mod test_e2e;
mod test_based_on_commit;
mod output_override_test;
mod progress_test;
mod test_determinism;
mod snapshot_test;
mod test_v31_empty_chunk_fix;
