//! install 安装功能域集成测试聚合入口（U1：tests 按功能域拆子目录）
#![cfg(test)]

#[path = "../common/mod.rs"]
mod common;

mod test_hook_install;
mod test_install_dsh;
mod test_install_opencode;
mod test_install_wiki;
