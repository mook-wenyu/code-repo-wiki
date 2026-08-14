//! bench 基准功能域集成测试聚合入口（U1：tests 按功能域拆子目录）
#![cfg(test)]

#[path = "../common/mod.rs"]
mod common;

mod test_bench_completeness_at_k;
mod test_bench_judge_tri_state;
mod test_bench_doc_info_llm;
