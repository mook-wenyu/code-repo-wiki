#![cfg(test)]

//! 聚类跨次稳定评测（演进计划 T4.2）
//!
//! 社区检测与特征聚类必须确定性可复现（leiden-rs 固定 seed + 输出排序）：
//! 同一图连续两次检测，模块名、每模块 node_ids、特征划分逐项一致。
//! 用合成模块化仓库（10 簇完全图 + 单条跨簇边，对齐 bench 语义）。

use std::path::Path;


/// 构造 10 簇 × 8 文件的模块化仓库：簇内完全图（每文件 f 调用同簇其余
/// 文件的 g）+ 每簇第 0 文件的 g 调下一簇 f（单条跨簇边）
fn build_cluster_repo(dir: &Path) {
    for m in 0..10 {
        let sub = dir.join(format!("m{m}"));
        std::fs::create_dir_all(&sub).unwrap();
        for i in 0..8 {
            let mut body = String::new();
            body.push_str(&format!("pub fn f{m}_{i}(x: u32) -> u32 {{ "));
            for j in 0..8 {
                if j != i {
                    body.push_str(&format!("g{m}_{j}(x) + "));
                }
            }
            body.push_str("x }\n");
            // 每文件定义自己的 g（重复定义会破坏 tree-sitter 解析——每个
            // 文件名不同，函数名同簇内唯一）
            body.push_str(&format!("pub fn g{m}_{i}(x: u32) -> u32 {{ x + {m} }}\n"));
            // 跨簇单边：本簇 g0 调用下一簇 f0（唯一跨簇调用）
            if i == 0 && m < 9 {
                body.push_str(&format!("pub fn cross{m}(x: u32) -> u32 {{ f{}_{0}(x) }}\n", m + 1));
            }
            std::fs::write(sub.join(format!("f{i}.rs")), body).unwrap();
        }
    }
}

/// 同图两次检测结果逐项一致（模块名、node_ids 集合、特征划分）
#[test]
fn test_clustering_stable_across_runs() {
    let dir = std::env::temp_dir().join(format!("repo_wiki_cluster_stab_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    build_cluster_repo(&dir);

    let root = repo_wiki::project::ProjectRoot::new(dir.clone());
    let insights = repo_wiki::ingest::scan_and_parse_at(&root).unwrap().insights;
    let graph = repo_wiki::analysis::build_graph(&insights).unwrap();

    // 两次独立检测
    let modules_1 = repo_wiki::analysis::detect_modules(&graph).unwrap();
    let modules_2 = repo_wiki::analysis::detect_modules(&graph).unwrap();
    let features_1 = repo_wiki::analysis::feature::detect_features(&graph, None).unwrap();
    let features_2 = repo_wiki::analysis::feature::detect_features(&graph, None).unwrap();

    // 1. 模块数 + 名称排序一致
    let mut names_1: Vec<&str> = modules_1.iter().map(|m| m.name.as_str()).collect();
    let mut names_2: Vec<&str> = modules_2.iter().map(|m| m.name.as_str()).collect();
    names_1.sort();
    names_2.sort();
    assert_eq!(names_1, names_2, "模块名称集合必须跨次一致");
    assert!(!names_1.is_empty(), "合成仓库应检出模块");

    // 2. 每模块 node_ids 集合一致（名称 → 排序 node_ids）
    for (m1, m2) in modules_1.iter().zip(modules_2.iter()) {
        let mut ids_1: Vec<_> = m1.node_ids.to_vec();
        let mut ids_2: Vec<_> = m2.node_ids.to_vec();
        ids_1.sort();
        ids_2.sort();
        assert_eq!(ids_1, ids_2, "模块 {} 的节点归属必须跨次一致", m1.name);
    }

    // 3. 特征划分一致
    assert_eq!(features_1.len(), features_2.len(), "特征数必须跨次一致");
    for (f1, f2) in features_1.iter().zip(features_2.iter()) {
        let mut ids_1: Vec<_> = f1.node_ids.to_vec();
        let mut ids_2: Vec<_> = f2.node_ids.to_vec();
        ids_1.sort();
        ids_2.sort();
        assert_eq!(ids_1, ids_2, "特征 {} 的节点归属必须跨次一致", f1.name);
    }

    // 4. 组合稳定性：build_graph 内嵌的 modules 与独立 detect 一致
    let mut g_names: Vec<&str> = graph.modules.iter().map(|m| m.name.as_str()).collect();
    g_names.sort();
    assert_eq!(g_names, names_1, "build_graph 写回的 modules 必须与 detect 一致");

    // 5. 正确性护栏：10 簇弱连接，模块数应接近 10（允许少量合并/拆分）
    assert!(
        (5..=15).contains(&modules_1.len()),
        "模块数应接近 10，实际 {}",
        modules_1.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}