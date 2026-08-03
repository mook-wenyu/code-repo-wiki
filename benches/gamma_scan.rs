//! γ（CPM resolution）扫描评测工具（v13 D3）
//!
//! 社区检测的分辨率参数 γ 决定模块粒度：γ 越大社区越细（模块越多）。
//! t08 实测 γ=0.5 在 Unity 仓库（2165 文件）产生 526 模块（过细）。
//! 本工具对目标仓库在多个 γ 下运行社区检测，统计模块数/单文件模块占比/
//! 模块规模分布，为默认 γ 取值提供实测依据（分辨率参数化入口
//! detect_communities_with_resolution 由 v13 D3 拆出）。
//!
//! 用法（目标仓库根经 GAMMA_REPO 环境变量指定，缺省跳过）：
//! ```powershell
//! $env:GAMMA_REPO = "D:\UnityProjects\Project Strategy"
//! cargo test --bench gamma_scan -- --nocapture
//! ```

use std::path::Path;

/// 扫描的 γ 取值集（t08 结论：γ 有效区大概率 0.1-0.5，向细粒度方向补 0.6 观察）
const GAMMAS: [f64; 5] = [0.2, 0.3, 0.4, 0.5, 0.6];

#[test]
fn gamma_scan() {
    let Some(repo) = std::env::var("GAMMA_REPO").ok() else {
        eprintln!("gamma_scan: 未设置 GAMMA_REPO，跳过（目标仓库根目录）");
        return;
    };
    let root = repo_wiki::project::ProjectRoot::new(Path::new(&repo).to_path_buf());
    let mut config = repo_wiki::config::schema::WikiConfig::default();
    // 扫描范围：GAMMA_REPO_SCOPE 逗号分隔的 glob 列表（缺省 C#——Unity 仓库主体）
    let scope = std::env::var("GAMMA_REPO_SCOPE")
        .unwrap_or_else(|_| "**/*.cs".to_string());
    config.scope.include = scope
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let start = std::time::Instant::now();
    let scan = repo_wiki::ingest::scan_and_parse_at(&root, &config).expect("扫描失败");
    eprintln!(
        "扫描完成: {} 文件, {} 个解析失败, 耗时 {:.1}s",
        scan.insights.len(),
        scan.files_failed,
        start.elapsed().as_secs_f32()
    );
    let graph = repo_wiki::analysis::build_graph(&scan.insights).expect("建图失败");

    // 基线：生产默认 γ=0.5 的当前行为（与 t08 结论对照）
    let baseline = repo_wiki::analysis::community::detect_communities(&graph);
    eprintln!("基线 γ=0.5（生产默认）: {} 个模块", baseline.len());

    for gamma in GAMMAS {
        let t = std::time::Instant::now();
        let communities = repo_wiki::analysis::community::detect_communities_with_resolution(&graph, gamma);
        let total = communities.len();
        let single_file = communities.iter().filter(|c| c.len() == 1).count();
        let sizes: Vec<usize> = communities.iter().map(|c| c.len()).collect();
        let max = sizes.iter().copied().max().unwrap_or(0);
        let mean: f64 = if sizes.is_empty() {
            0.0
        } else {
            sizes.iter().sum::<usize>() as f64 / sizes.len() as f64
        };
        eprintln!(
            "γ={:.1}: 模块 {} 个（单文件 {} 个, {:.0}%）, 最大 {} 文件, 平均 {:.1}, {:.2}s",
            gamma,
            total,
            single_file,
            if total > 0 { single_file as f64 * 100.0 / total as f64 } else { 0.0 },
            max,
            mean,
            t.elapsed().as_secs_f32()
        );
    }
}
