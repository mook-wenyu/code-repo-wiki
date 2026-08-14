//! CLI 功能域集成测试聚合入口（U1：tests 按功能域拆子目录）
#![cfg(test)]

#[path = "../common/mod.rs"]
mod common;

mod test_cli;
mod test_cli_smoke;
mod test_key_cli;
mod test_export_skip;
mod test_card_lock;
