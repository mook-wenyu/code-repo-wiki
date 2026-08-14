//! content 内容功能域集成测试聚合入口（U1：tests 按功能域拆子目录）
#![cfg(test)]

#[path = "../common/mod.rs"]
mod common;

mod test_config_template;
mod test_index_guide;
mod test_multilang;
mod test_parser_dedup_7lang;
mod test_protected_files;
