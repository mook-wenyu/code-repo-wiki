#![cfg(test)]

//! 社区检测与特征聚类集成测试（演进计划 T1.4）
//!
//! 验证链路：真实文件 → scan_and_parse → build_graph → 模块社区检测
//! （graph.modules）+ 特征聚类（graph.features），并断言模块命名确定性
//! （杜绝 "src::a.rs" 式把文件名当目录段的错误命名）。
//!
//! 注意：`scan_and_parse` 以 `std::env::current_dir()` 为扫描根，本文件
//! 全部断言集中在单个顺序测试函数内（先切 cwd，结束时恢复），避免
//! Rust 并行测试之间的 cwd 竞态。

use std::path::Path;
use std::sync::Mutex;

use repo_wiki::analysis;
use repo_wiki::ingest;

/// 串行化 cwd 切换（与 test_e2e 同一模式）
static CWD_LOCK: Mutex<()> = Mutex::new(());

/// 构造临时仓库：
/// - src/a/ 内两个文件通过跨文件调用协作（lib.rs 调用 helper.rs 的函数）
/// - src/b/ 独立文件，与 a 无任何依赖
fn build_fixture_repo(repo: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(repo.join("src").join("a"))?;
    std::fs::create_dir_all(repo.join("src").join("b"))?;

    std::fs::write(
        repo.join("src").join("a").join("lib.rs"),
        r#"
//! 模块 A 入口
pub mod helper;

pub fn process(data: &[u8]) -> u32 {
    helper::checksum(data)
}
"#,
    )?;
    std::fs::write(
        repo.join("src").join("a").join("helper.rs"),
        r#"
//! 模块 A 辅助函数
pub fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |acc, b| acc.wrapping_add(*b as u32))
}
"#,
    )?;
    std::fs::write(
        repo.join("src").join("b").join("mod.rs"),
        r#"
//! 模块 B：独立功能，与 A 无依赖
pub fn beta() -> &'static str { "beta" }
"#,
    )?;
    Ok(())
}

#[test]
fn test_community_detection_and_features() {
    let _guard = CWD_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!(
        "repo_wiki_clustering_test_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    build_fixture_repo(&tmp).expect("构造测试仓库失败");

    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    let result = (|| -> anyhow::Result<()> {
        let insights = ingest::scan_and_parse(&repo_wiki::config::schema::WikiConfig::default())?;
        assert!(!insights.is_empty(), "应扫描到源文件");
        let mut graph = analysis::build_graph(&insights)?;
        // features 由 lib 层 attach_features 填充（lib 私有函数），
        // 集成测试直接调用 analysis 层公开入口验证聚类本身
        graph.features = repo_wiki::analysis::feature::detect_features(&graph, None)?;

        // 1. 模块检测：社区检测生效，模块名唯一且不含文件名
        assert!(
            !graph.modules.is_empty(),
            "a/b 两个独立目录应检出模块"
        );
        let mut names: Vec<&str> = graph.modules.iter().map(|m| m.name.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            graph.modules.len(),
            "模块名必须唯一: {:?}",
            names
        );
        for name in &names {
            assert!(
                !name.contains(".rs"),
                "模块名不能含文件名（文件名当目录段是错误命名）: {name}"
            );
        }
        // a 目录（含跨文件调用）应成为社区；b 独立目录也应成为社区
        assert!(
            names.iter().any(|n| n.ends_with("a")),
            "a 目录应检出一个模块，实际: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with("b")),
            "b 目录应检出一个模块，实际: {names:?}"
        );

        // 2. 特征聚类：a 内的跨文件调用（process → checksum）应形成特征
        //（纯结构聚类，无需 embedding）
        assert!(
            !graph.features.is_empty(),
            "a 目录的跨文件调用应形成特征"
        );
        let feature_names: Vec<String> = graph
            .features
            .iter()
            .flat_map(|f| {
                f.node_ids
                    .iter()
                    .map(|nid| {
                        graph
                            .graph
                            .node_weight(*nid)
                            .map(|n| n.name.clone())
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(
            feature_names.iter().any(|n| n == "checksum"),
            "特征应包含 checksum: {feature_names:?}"
        );
        assert!(
            feature_names.iter().any(|n| n == "process"),
            "特征应包含 process: {feature_names:?}"
        );

        Ok(())
    })();
    std::env::set_current_dir(&prev).unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    result.expect("集成断言失败");
}
