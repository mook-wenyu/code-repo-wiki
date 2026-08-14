//! γ（CPM resolution）扫描评测工具（v13 D3，U1 语义增强）
//!
//! 社区检测的分辨率参数 γ 决定模块粒度：γ 越大社区越细（模块越多）。
//! t08 实测 γ=0.5 在 Unity 仓库（2165 文件）产生 526 模块（过细）。
//! 本工具对目标仓库在多个 γ 下运行社区检测，统计模块数/单文件模块占比/
//! 模块规模分布/Leiden quality/跨域社区数，为默认 γ 取值提供实测依据
//! （分辨率参数化入口 detect_communities_with_resolution 由 v13 D3 拆出，
//! U1 后统一走 quality API detect_communities_with_quality）。
//!
//! ## U1 约束下的 γ 扫描语义
//! - **跨顶层域边剔除**（U1 缝合根治）：src↔tests/benches 等跨域依赖不再参与
//!   Leiden 合并；非 src 域文件一律按目录聚簇，`src` 域孤立文件同样按目录
//!   聚簇。**因此 γ 扫描只影响 src 域实体级 Leiden 部分**——γ 越大，src 域内
//!   有同域依赖文件的分区越细；tests/benches/根散文件按目录聚簇的社区
//!   与 γ 无关。
//! - **quality 语义（信息性指标，勿跨 γ 直接比较）**：quality 是 Leiden CPM
//!   目标值（leiden-rs 0.8.1，Traag 2019）。CPM 的 H 值随 γ 变化目标函数
//!   不同，**跨 γ 不可直接比较**；γ 选优应看「模块数-γ 曲线的稳定平台/拐点」
//!   与粒度是否符合预期。同一 γ 下 quality 用于衡量该网格划分质量；目录
//!   分流/无 Leiden 时 quality=0.0 为语义值（无 Leiden 即无 CPM 质量可测）。
//!   quality 仅覆盖 src 域实体级 Leiden 部分，目录聚簇社区不参与。
//! - **跨域社区数=0 是缝合消失的度量**：若某社区内文件跨多个顶层域
//!   （如 src 与 tests 文件被缝进同一社区），说明 U1 约束未生效或回归。
//!   输出 `跨域社区 {n} 个`，U1 后恒为 0，是回归检测哨兵。
//!
//! 用法（目标仓库根经 GAMMA_REPO 环境变量指定，缺省跳过）：
//! ```powershell
//! $env:GAMMA_REPO = "D:\UnityProjects\Project Strategy"
//! cargo test --bench gamma_scan -- --nocapture
//! ```

use std::path::Path;

use code_repo_wiki::model::NodeId;

/// 扫描的 γ 取值集（t08 结论：γ 有效区大概率 0.1-0.5，向细粒度方向补 0.6 观察）
const GAMMAS: [f64; 5] = [0.2, 0.3, 0.4, 0.5, 0.6];

/// 顶层域：路径首段（"src" / "tests" / "benches"…），根目录散文件归 "<root>"
///
/// 反斜杠归一正斜杠（`/` 与 `\` 均视为分隔符）；无目录段（平铺根散文件、
/// 空串）→ "<root>"。与 community.rs 的 `top_level_domain` 语义一致——
/// 该函数在 src 内是 private，bench 是独立编译的 crate 无法复用；此处复制
/// 一份纯字符串函数（约 8 行）换取 bench 独立编译，**两处需同步**（改 src
/// 版本后须同步改这里，反之亦然）。DRY 权衡：跨域统计是评测工具的
/// 结构性断言，独立于主库实现可避免 bench 编译耦合。
fn top_level_domain(path: &str) -> &str {
    let bytes = path.as_bytes();
    let start = bytes.iter().position(|&b| b != b'/' && b != b'\\');
    let Some(start) = start else {
        return "<root>"; // 空串或全为分隔符：根
    };
    let end = bytes[start..]
        .iter()
        .position(|&b| b == b'/' || b == b'\\')
        .map(|off| start + off)
        .unwrap_or(path.len());
    if end == path.len() {
        "<root>" // 首段后无分隔符 → 根目录散文件
    } else {
        &path[start..end]
    }
}

/// 跨域社区数：社区内所有 File 的顶层域集合大小 > 1 的社区个数
///
/// U1 约束后该值恒为 0（跨域边剔除 + 非 src 按目录聚簇，社区不会缝合
/// 多个顶层域），是缝合消失的回归检测度量。
fn count_cross_domain_communities(
    graph: &code_repo_wiki::model::KnowledgeGraph,
    communities: &[Vec<NodeId>],
) -> usize {
    communities
        .iter()
        .filter(|comm| {
            let mut domains: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            for &nid in comm.iter() {
                let p = graph
                    .graph
                    .node_weight(nid)
                    .and_then(|n| n.file_path.as_deref())
                    .unwrap_or("");
                domains.insert(top_level_domain(p));
            }
            domains.len() > 1
        })
        .count()
}

#[test]
fn gamma_scan() {
    let Some(repo) = std::env::var("GAMMA_REPO").ok() else {
        eprintln!("gamma_scan: 未设置 GAMMA_REPO，跳过（目标仓库根目录）");
        return;
    };
    let root = code_repo_wiki::project::ProjectRoot::new(Path::new(&repo).to_path_buf());
    // v30+：扫描范围硬编码为全量+内置过滤（支持语言扩展名/噪音目录），
    // Unity 仓库的 .cs 自动覆盖，GAMMA_REPO_SCOPE 环境变量已删除

    let start = std::time::Instant::now();
    let scan = code_repo_wiki::ingest::scan_and_parse_at(&root).expect("扫描失败");
    eprintln!(
        "扫描完成: {} 文件, {} 个解析失败, 耗时 {:.1}s",
        scan.insights.len(),
        scan.files_failed,
        start.elapsed().as_secs_f32()
    );
    let graph = code_repo_wiki::analysis::build_graph(&scan.insights).expect("建图失败");

    // 基线：生产默认 γ=0.5（与 t08 结论对照）。改调 quality API，
    // 同时打印 Leiden CPM quality（仅覆盖 src 域 Leiden 部分，信息性指标）
    let baseline_t = std::time::Instant::now();
    let (baseline, baseline_q) =
        code_repo_wiki::analysis::community::detect_communities_with_quality(&graph, 0.5);
    let baseline_cross = count_cross_domain_communities(&graph, &baseline);
    eprintln!(
        "基线 γ=0.5（生产默认）: {} 个模块, 跨域社区 {} 个, quality {:.4}, {:.2}s",
        baseline.len(),
        baseline_cross,
        baseline_q,
        baseline_t.elapsed().as_secs_f32()
    );

    for gamma in GAMMAS {
        let t = std::time::Instant::now();
        let (communities, quality) =
            code_repo_wiki::analysis::community::detect_communities_with_quality(&graph, gamma);
        let total = communities.len();
        let single_file = communities.iter().filter(|c| c.len() == 1).count();
        let sizes: Vec<usize> = communities.iter().map(|c| c.len()).collect();
        let max = sizes.iter().copied().max().unwrap_or(0);
        let mean: f64 = if sizes.is_empty() {
            0.0
        } else {
            sizes.iter().sum::<usize>() as f64 / sizes.len() as f64
        };
        let cross = count_cross_domain_communities(&graph, &communities);
        eprintln!(
            "γ={:.1}: 模块 {} 个（单文件 {} 个, {:.0}%）, 最大 {} 文件, 平均 {:.1}, \
             跨域社区 {} 个, quality {:.4}, {:.2}s",
            gamma,
            total,
            single_file,
            if total > 0 {
                single_file as f64 * 100.0 / total as f64
            } else {
                0.0
            },
            max,
            mean,
            cross,
            quality,
            t.elapsed().as_secs_f32()
        );
    }
}
