//! 文件级社区检测：用 leiden-rs（Leiden 算法，CPM 质量函数）在文件依赖图上
//! 划分功能内聚的模块社区，替代旧的"目录前缀凝聚聚类"。
//!
//! 设计要点（对应演进计划 T1.2/T1.3）：
//! - 聚类单位是 **File 节点**，依赖边取跨文件的 Imports/Calls
//!   （同文件内调用是内部实现细节，不参与模块划分）；
//! - 边权重与 graph.rs 构建时一致（Imports=0.8 / Calls=0.7），
//!   同对文件间多条边按权重相加聚合；
//! - CPM 质量函数 + 固定种子（确定性输出，可复现）；
//! - 无跨文件依赖时退化为"每文件一个社区"（Leiden 对无边图无划分意义）；
//! - 社区命名三档规则见 [`community_name`]（T1.3）：公共目录前缀 →
//!   文件数最多目录 → module_{n}，保证产物路径与断链检测的确定性。

use std::collections::HashMap;

use petgraph::visit::{EdgeRef, IntoEdgeReferences, IntoNodeReferences};

use leiden_rs::{GraphDataBuilder, Leiden, LeidenConfig, QualityType};

use crate::model::*;

/// Leiden 固定种子：保证同一代码库多次运行产出相同社区划分
const LEIDEN_SEED: u64 = 42;
/// CPM 分辨率参数 γ：越高社区越碎。实测：γ=0.8 时单条跨文件调用边
/// （权重 0.7）不满足合并增益，会把协作文件拆成独立社区（过碎）；
/// γ=0.5 时单边足够合并（0.7−0.5=0.2>0），多边强连接社区不受影响。
/// 对代码图而言"任何跨文件调用都表示协作"，取 0.5（演进计划 D4 实测调参）
const LEIDEN_RESOLUTION: f64 = 0.5;

/// 社区检测边权重（graph.rs 引用本常量，勿再硬编码字面量）
pub const WEIGHT_IMPORTS: f64 = 0.8;
pub const WEIGHT_CALLS: f64 = 0.7;

/// 文件所属目录键：反斜杠归一正斜杠、去尾分隔符、根目录文件归 "<root>"
fn file_dir_key(graph: &KnowledgeGraph, nid: NodeId) -> String {
    let path = graph
        .graph
        .node_weight(nid)
        .and_then(|n| n.file_path.as_deref())
        .unwrap_or("");
    let norm = path.replace('\\', "/");
    let dir = norm.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        "<root>".to_string()
    } else {
        dir.to_string()
    }
}

/// 顶层域：路径首段（"src" / "tests" / "benches"…），根目录散文件归 "<root>"
///
/// 反斜杠归一正斜杠（`/` 与 `\` 均视为分隔符）；无目录段（平铺根散文件、
/// 空串）→ "<root>"。跨域判定唯一真源（U1）：Leiden 社区合并必须不跨越
/// 顶层域，否则 tests/benches 桥 + 跨域 API 调用会凭"同目录前缀"把
/// src/output、src/search 等域内模块缝进同一社区。
///
/// 结构安全：File 节点必有 file_path（graph.rs 构建时强制 Some），但路径
/// 存在与否不改变返回值形状——全部为分隔符/空串时归 "<root>"，无失败路径
/// （纯字符串函数，不 panic）。用字节扫描定位首个非分隔段，避免为反斜杠
/// 归一做 `String` 分配（保持 `-> &str` 零拷贝）。
fn top_level_domain(path: &str) -> &str {
    let bytes = path.as_bytes();
    // 定位首个非分隔符字符（跳过开头若干分隔符，如绝对路径 "/a.rs"）
    let start = bytes.iter().position(|&b| b != b'/' && b != b'\\');
    let Some(start) = start else {
        return "<root>"; // 空串或全为分隔符：根
    };
    // 定位该段之后的下一个分隔符
    let end = bytes[start..]
        .iter()
        .position(|&b| b == b'/' || b == b'\\')
        .map(|off| start + off)
        .unwrap_or(path.len());
    if end == path.len() {
        // 首段后面没有分隔符 → 整个路径是根目录散文件（无目录段）→ 归根
        "<root>"
    } else {
        &path[start..end]
    }
}

/// 目录聚簇键：`file_dir_key` 基础上，目录键以 "/common" 结尾的并入父目录
/// 键（Cargo 集成测试共享 helper 约定：`tests/common/`、`benches/common/`
/// 是全体集成测试共用的支撑目录，应折叠进父域而非独立成模块，否则跨测试
/// 的 common 桩会把不相干的测试文件拉进同一粒度）。
fn cluster_dir_key(graph: &KnowledgeGraph, nid: NodeId) -> String {
    let key = file_dir_key(graph, nid);
    const SUFFIX: &str = "/common";
    if key.ends_with(SUFFIX) {
        key[..key.len() - SUFFIX.len()].to_string()
    } else {
        key
    }
}

/// 文件级社区检测：返回 File 节点的社区划分（每社区一个 `Vec<NodeId>`）
///
/// 输出确定性：社区内按 file_path 字典序排序后作为分组键排序。
pub fn detect_communities(graph: &KnowledgeGraph) -> Vec<Vec<NodeId>> {
    detect_communities_with_resolution(graph, LEIDEN_RESOLUTION)
}

/// 带分辨率参数的社区检测（v13 D3 拆分：评测工具 gamma_scan 需要扫描
/// 多个 γ 取值找最优粒度，生产默认走 detect_communities 的常量分辨率）
pub fn detect_communities_with_resolution(
    graph: &KnowledgeGraph,
    resolution: f64,
) -> Vec<Vec<NodeId>> {
    detect_communities_with_quality(graph, resolution).0
}

/// 带 Leiden quality 的社区检测（gamma_scan 实证用；quality 为 Leiden CPM
/// 目标值，仅覆盖实体级 Leiden 部分，目录聚簇社区不参与；目录分流/无
/// Leiden 时返回 0.0）
///
/// 结构安全：quality=0.0 在目录分流/无 Leiden 路径上是语义值（无 Leiden
/// 即无 CPM 质量可测），不是错误；主路径 quality 恒为有限非负数
/// （leiden-rs 0.8.1 的 `LeidenOutput.quality`，见下方结构安全证据）。
pub fn detect_communities_with_quality(
    graph: &KnowledgeGraph,
    resolution: f64,
) -> (Vec<Vec<NodeId>>, f64) {
    let file_nodes: Vec<NodeId> = graph
        .graph
        .node_references()
        .filter(|(_, n)| n.kind == NodeKind::File)
        .map(|(id, _)| id)
        .collect();

    if file_nodes.is_empty() {
        return (Vec::new(), 0.0);
    }
    if file_nodes.len() == 1 {
        return (vec![file_nodes], 0.0);
    }

    // ===== 大仓库目录页分流（v26 方案 D 修正版）=====
    // 目录超节点 + Leiden 的实测结论：CPM 合并增益按绝对边权重计算
    // （期望量级=实体级单边 0.7~0.8），而超节点图的聚合边权重是跨
    // 目录依赖条数之和（几十到几百），γ=0.5 下几乎任何聚合边都满足
    // 合并条件 → 密集调用仓库全部并入一个社区（实测 99 文件仓库只剩
    // 2 个模块页，信息丢失）。因此目录超节点图上的 Leiden 不可用。
    // 修正：目录数 ≥ MIN_DIRS_FOR_SUPERNODE 的仓库直接以目录为社区
    // （页面名=目录路径，新增/删除文件不改变划分，零随机参数）；
    // 中小仓库仍走实体级 Leiden（γ=0.5 已实测调优，粒度/成本最优）。
    // 阈值 24 的论证（实机数据，v28 t10 复核）：Unity 仓库 52 目录、
    // code-repo-wiki 自身 15 目录（src+tests）属"大仓库"（目录页粒度合适）；
    // 150 文件测试 fixture（15 模块 × 10 文件 = 15 目录）以下走实体级
    // Leiden。24 落在两档之间：≥24 时目录页规模可接受且免超节点图失真
    // （v26 D 实测：密集调用仓库在目录超节点图上全部并入一个社区），
    // <24 时实体级粒度更细（目录数少，页数可负担）。
    const MIN_DIRS_FOR_SUPERNODE: usize = 24;
    let mut dirs: std::collections::BTreeMap<String, Vec<NodeId>> =
        std::collections::BTreeMap::new();
    for &nid in &file_nodes {
        let d = file_dir_key(graph, nid);
        dirs.entry(d).or_default().push(nid);
    }
    // 单目录退化保护（v29）：目录数 ≤ 1（所有源文件平铺在同一目录，如
    // src/ 平铺仓库；根目录散文件归 <root> 同此）时走实体级 Leiden 会把
    // 整库聚成 1-2 个社区甚至每文件一社区，模块划分失去意义——直接走
    // 目录页路径：单一社区 = 全部文件，模块名即目录名（community_name
    // 公共目录前缀档），下游 api.md 的 `## src` 节与模块页引用均正常。
    if dirs.len() <= 1 || dirs.len() >= MIN_DIRS_FOR_SUPERNODE {
        return (dirs.into_values().collect(), 0.0);
    }

    // File 节点 → 紧凑索引（leiden-rs 要求 0..n 连续，StableDiGraph 有洞）
    let compact: HashMap<NodeId, usize> = file_nodes
        .iter()
        .enumerate()
        .map(|(i, &nid)| (nid, i))
        .collect();

    // 实体 → 所属 File 映射（经 Contains 边反查）。
    // 关键语义：Calls 边挂在实体节点上；Imports 边 v52 T11 起源可为
    // **File 节点**（文件级 import 单边建模）或实体节点；File 节点只有
    // Contains 边。社区划分的单位是 File，因此实体必须归位到所属文件：
    // file_of(File)=自身、file_of(实体)=经 Contains 边反查，聚合语义兼容。
    let mut entity_to_file: HashMap<NodeId, NodeId> = HashMap::new();
    for edge in graph.graph.edge_references() {
        let kind = graph.graph.edge_weight(edge.id()).map(|e| e.kind.clone());
        if kind == Some(EdgeKind::Contains) && compact.contains_key(&edge.source()) {
            entity_to_file.insert(edge.target(), edge.source());
        }
    }
    // 端点归位：File 节点取自身，实体节点取其所属 File
    let file_of = |nid: NodeId| -> Option<NodeId> {
        if compact.contains_key(&nid) {
            Some(nid)
        } else {
            entity_to_file.get(&nid).copied()
        }
    };

    // 每个 File 节点所属顶层域（U1 跨域判定唯一真源；File 节点必有
    // file_path，缺省归 "<root>"，见 top_level_domain 结构安全注释）
    let file_domain: Vec<&str> = file_nodes
        .iter()
        .map(|&nid| {
            let p = graph
                .graph
                .node_weight(nid)
                .and_then(|n| n.file_path.as_deref())
                .unwrap_or("");
            top_level_domain(p)
        })
        .collect();

    // 聚合跨文件同域依赖边：同对文件间多条边权重相加；**顶层域不同的两端
    // 直接剔除**（U1 缝合根治——common 桥/跨域 API 调用把不同域模块拉进
    // 同一社区的拼接不再发生），同时收集参与同域边的文件端点（紧凑索引）
    let mut edge_weights: HashMap<(usize, usize), f64> = HashMap::new();
    let mut connected: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for edge in graph.graph.edge_references() {
        // 结构安全证据（R1 审计）：边权重由 build_graph 创建边时同步写入
        //（StableDiGraph 的边权重不是 Option），此处仅做类型层面的取值；
        // 若 graph 构造代码未来绕过权重初始化，本 expect 会立即失败暴露。
        let e = graph.graph.edge_weight(edge.id()).expect("边权重必然存在");
        let w = match e.kind {
            EdgeKind::Imports => WEIGHT_IMPORTS,
            EdgeKind::Calls => WEIGHT_CALLS,
            _ => continue, // 其他类型边（Contains/Implements）不参与社区划分
        };
        let (Some(sf), Some(tf)) = (file_of(edge.source()), file_of(edge.target())) else {
            continue; // 端点既不是 File 也不是实体（Module 等中间节点）
        };
        let (Some(&si), Some(&ti)) = (compact.get(&sf), compact.get(&tf)) else {
            continue;
        };
        if si == ti {
            continue; // 同文件内依赖不构成模块间关系
        }
        if file_domain[si] != file_domain[ti] {
            continue; // 跨顶层域边剔除：不参与 Leiden（同域边照常聚合）
        }
        connected.insert(si);
        connected.insert(ti);
        *edge_weights.entry((si, ti)).or_insert(0.0) += w;
    }

    // ---- 域内策略（U1）----
    // src 域文件：有同域边者进实体级 Leiden；src 域孤立文件（未出现在任何
    // 同域边端点）不进 Leiden，按目录聚簇。非 src 域文件（tests/benches/
    // examples/<root>…）一律不进 Leiden，按目录聚簇（common 目录并入父键）。
    // edge_weights 为空时本策略自然退化为"全部按目录聚簇"，无需单独分支。
    let mut leiden_vector: Vec<(usize, NodeId)> = Vec::new(); // (紧凑索引, nid)
    let mut leiden_pos: HashMap<usize, usize> = HashMap::new();
    for (i, &nid) in file_nodes.iter().enumerate() {
        if file_domain[i] == "src" && connected.contains(&i) {
            leiden_pos.insert(i, leiden_vector.len());
            leiden_vector.push((i, nid));
        }
    }

    let mut communities: Vec<Vec<NodeId>> = Vec::new();
    let mut quality: f64 = 0.0;

    if leiden_vector.is_empty() {
        // 无 src 连通文件：全库按目录聚簇（src 孤立 + 非 src 域）
        let mut dir_clusters: std::collections::BTreeMap<String, Vec<NodeId>> =
            std::collections::BTreeMap::new();
        for &nid in &file_nodes {
            dir_clusters
                .entry(cluster_dir_key(graph, nid))
                .or_default()
                .push(nid);
        }
        for mut cluster in dir_clusters.into_values() {
            sort_files_by_path(graph, &mut cluster);
            communities.push(cluster);
        }
        sort_communities(graph, &mut communities);
        return (communities, quality);
    }

    // 构建 Leiden 输入图（有向、加权；add_edge 校验权重有限且 ≥0，不会失败）。
    // 节点集 = src 连通文件子集；边仅取两端均落在此子集内的（edge_weights
    // 里同属非 src 域的边在此处一并丢弃——非 src 文件不走 Leiden）。
    // 结构安全证据（R1 审计）：权重来源仅两处——常量 WEIGHT_IMPORTS/
    // WEIGHT_CALLS（0.7/0.8，有限非负）与 edge_weights 的累加和；
    // 不存在 NaN/Inf/负值注入路径，add_edge 的 Err 分支不可达。
    let mut builder = GraphDataBuilder::new(leiden_vector.len()).directed();
    for ((s, t), w) in &edge_weights {
        let (Some(&sp), Some(&tp)) = (leiden_pos.get(s), leiden_pos.get(t)) else {
            continue; // 端点不在 Leiden 子集（非 src 文件）→ 不舍弃其权重的干扰
        };
        builder.add_edge(sp, tp, *w).expect("边权重均为有限非负数");
    }
    let data = builder.build().expect("图数据构造失败");

    let config = LeidenConfig {
        quality: QualityType::CPM,
        resolution,
        seed: Some(LEIDEN_SEED),
        ..Default::default()
    };
    // 结构安全证据（R1 审计）：leiden-rs 0.8.1 对用户输入从不 panic——
    // 空图/单节点/无边图均提前安全返回（edge_weights 非空且有 src 连通文件
    // 时，Leiden 输入恒为至少一条边的有效图）；run() 仅返回 internal-error
    // 类 Result（图内部不变量破坏，不可由调用方触发）。quality 取自
    // `LeidenOutput.quality`（leiden.rs:262），恒为有限 CPM 目标值。
    let result = Leiden::new(config).run(&data).expect("Leiden 社区检测失败");
    quality = result.quality;
    let membership = result.partition.as_slice(); // membership[p] = 位置 p 的社区 ID

    // 按社区 ID 分组回 File 节点（src 连通子集）
    let mut groups: HashMap<usize, Vec<NodeId>> = HashMap::new();
    for (p, &comm) in membership.iter().enumerate() {
        groups.entry(comm).or_default().push(leiden_vector[p].1);
    }
    communities.extend(groups.into_values());

    // src 域孤立文件与所有非 src 域文件：按目录聚簇，并入最终输出
    let in_leiden: std::collections::HashSet<NodeId> =
        leiden_vector.iter().map(|&(_, nid)| nid).collect();
    let mut dir_clusters: std::collections::BTreeMap<String, Vec<NodeId>> =
        std::collections::BTreeMap::new();
    for &nid in &file_nodes {
        if in_leiden.contains(&nid) {
            continue;
        }
        dir_clusters
            .entry(cluster_dir_key(graph, nid))
            .or_default()
            .push(nid);
    }
    for mut cluster in dir_clusters.into_values() {
        sort_files_by_path(graph, &mut cluster);
        communities.push(cluster);
    }

    // ---- 社区大小上限拆分（U1 补充）----
    // 实证（本仓库 γ=0.2~0.6）：单 crate 仓库的 src 域实体级 Leiden 会把整个
    // src 聚成一个大社区（内部强连接，边累积权重远超 γ，调参无效，实测 73
    // 文件/γ 全区间 71~75）——大 chunk 致模块页 LLM 只概括代表性实体（质量
    // 降级）。对超过上限的社区按目录键（cluster_dir_key，含 /common 折叠）
    // 拆分子簇：保留 ≤上限 的跨目录功能域合并能力（Leiden 仍有意义），
    // 又杜绝大社区失控。拆分键 = 目录，确定性（BTreeMap 排序）。
    // 阈值 20 依据（本仓库目录规模）：src 各子目录 3~13 文件、tests 功能域
    // 目录 2~8 文件均低于上限不触发；仅 src 大社区（73）触发拆分为目录簇。
    const MAX_COMMUNITY_FILES: usize = 20;
    let mut communities = split_oversized_communities(graph, communities, MAX_COMMUNITY_FILES);

    // 确定性输出：按社区大小降序（Graphify 稳定重索引：大社区编号优先，
    // 新增小社区不改变既有大社区的相对序，产物文件名/模块编号跨次稳定），
    // 同大小按最小 file_path 升序（全序确定，不依赖哈希表遍历序）
    sort_communities(graph, &mut communities);
    (communities, quality)
}

/// 社区大小上限拆分（U1 补充）：超过 `max_files` 的社区按目录键
/// （[`cluster_dir_key`]，含 /common 折叠）拆分子簇；不超过上限的社区
/// 原样保留。纯函数（输入输出均为社区列表），拆分键 = 目录，BTreeMap
/// 排序保证确定性——单测直接构造大社区输入验证，不依赖 Leiden 划分行为。
fn split_oversized_communities(
    graph: &KnowledgeGraph,
    communities: Vec<Vec<NodeId>>,
    max_files: usize,
) -> Vec<Vec<NodeId>> {
    let mut out: Vec<Vec<NodeId>> = Vec::with_capacity(communities.len());
    for community in communities {
        if community.len() <= max_files {
            out.push(community);
            continue;
        }
        let mut sub_clusters: std::collections::BTreeMap<String, Vec<NodeId>> =
            std::collections::BTreeMap::new();
        for &nid in &community {
            sub_clusters
                .entry(cluster_dir_key(graph, nid))
                .or_default()
                .push(nid);
        }
        for mut cluster in sub_clusters.into_values() {
            sort_files_by_path(graph, &mut cluster);
            out.push(cluster);
        }
    }
    out
}

/// 社区内按 file_path 字典序排序（确定性：目录聚簇/Leiden 分组的输出不
/// 依赖哈希表或图节点插入序）
fn sort_files_by_path(graph: &KnowledgeGraph, cluster: &mut [NodeId]) {
    cluster.sort_by(|a, b| {
        let pa = graph
            .graph
            .node_weight(*a)
            .and_then(|n| n.file_path.as_deref())
            .unwrap_or("");
        let pb = graph
            .graph
            .node_weight(*b)
            .and_then(|n| n.file_path.as_deref())
            .unwrap_or("");
        pa.cmp(pb)
    });
}

/// 全局社区排序：大小降序 + 最小 file_path 升序（确定性输出唯一全序）
fn sort_communities(graph: &KnowledgeGraph, communities: &mut [Vec<NodeId>]) {
    communities.sort_by(|a, b| {
        b.len()
            .cmp(&a.len())
            .then_with(|| min_file_path(graph, a).cmp(&min_file_path(graph, b)))
    });
}

/// 社区内最小 file_path（确定性排序键；无路径节点视为空串排最前）
fn min_file_path(graph: &KnowledgeGraph, files: &[NodeId]) -> String {
    files
        .iter()
        .filter_map(|nid| {
            graph
                .graph
                .node_weight(*nid)
                .and_then(|n| n.file_path.clone())
        })
        .min()
        .unwrap_or_default()
}

/// 社区命名三档规则（T1.3）：
///
/// 1. **公共目录前缀**：所有文件父目录的最长公共目录段 → `join("::")`
///    （最贴近"目录=模块"的直觉，且模块名→产物文件名/断链检测全部稳定）；
/// 2. **文件数最多目录**：公共前缀为空（不同根）时，按父目录分组取文件数
///    最多的目录名；
/// 3. **module_{n}**：仍为空（如根目录散文件）时按序号回退。
///
/// 纯函数：仅依赖文件路径，不依赖图结构，可独立单测。
pub fn community_name(files: &[String], fallback_index: usize) -> String {
    if files.is_empty() {
        return format!("module_{fallback_index}");
    }

    let dirs: Vec<Vec<String>> = files.iter().map(|p| dir_segments(p)).collect();

    // 档 1：所有文件父目录的最长公共目录段
    let min_len = dirs.iter().map(|d| d.len()).min().unwrap_or(0);
    let mut common = 0usize;
    'outer: for i in 0..min_len {
        let seg = &dirs[0][i];
        for other in &dirs[1..] {
            if other.get(i) != Some(seg) {
                break 'outer;
            }
        }
        common = i + 1;
    }
    if common > 0 {
        return dirs[0][..common].join("::");
    }

    // 档 2：文件数最多的父目录（根目录散文件用占位键，最终落档 3）
    let mut dir_counts: HashMap<String, usize> = HashMap::new();
    for d in &dirs {
        let key = if d.is_empty() {
            "<root>".to_string()
        } else {
            d.join("::")
        };
        *dir_counts.entry(key).or_insert(0) += 1;
    }
    if let Some((best, _)) = dir_counts
        .into_iter()
        .max_by_key(|(name, count)| (*count, name.clone()))
        && best != "<root>"
    {
        return best;
    }

    // 档 3：确定性回退
    format!("module_{fallback_index}")
}

/// 提取路径的父目录段（不含文件名与盘符 Prefix，Windows 安全）
fn dir_segments(path: &str) -> Vec<String> {
    use std::path::Component;
    std::path::Path::new(path)
        .parent()
        .map(|p| {
            p.components()
                .filter_map(|c| match c {
                    Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个小型知识图谱：src/a.rs、src/b.rs 跨文件调用，src/net/tcp.rs 独立
    fn make_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let add_file = |g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
                        path: &str|
         -> (NodeId, NodeId) {
            let module_path: Vec<String> = dir_segments(path);
            let nid = g.add_node(CodeNode {
                id: NodeId::new(g.node_count()),
                kind: NodeKind::File,
                name: path.into(),
                file_path: Some(path.into()),
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path,
            });
            // File → Entity 的 Contains 边
            let eid = g.add_node(CodeNode {
                id: NodeId::new(g.node_count()),
                kind: NodeKind::Function,
                name: format!("f{}", nid.index()),
                file_path: Some(path.into()),
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: Vec::new(),
            });
            g.add_edge(
                nid,
                eid,
                CodeEdge {
                    id: EdgeId::new(g.edge_count()),
                    kind: EdgeKind::Contains,
                    source: nid,
                    target: eid,
                    weight: 1.0,
                    location: None,
                },
            );
            (nid, eid)
        };
        let (_a, ea) = add_file(g, "src/a.rs");
        let (_b, eb) = add_file(g, "src/b.rs");
        let _tcp = add_file(g, "src/net/tcp.rs");
        // a 的实体 → b 的实体（跨文件 Calls）
        g.add_edge(
            ea,
            eb,
            CodeEdge {
                id: EdgeId::new(g.edge_count()),
                kind: EdgeKind::Calls,
                source: ea,
                target: eb,
                weight: WEIGHT_CALLS,
                location: None,
            },
        );
        kg
    }

    #[test]
    fn test_detect_communities_basic() {
        let kg = make_graph();
        let communities = detect_communities(&kg);
        // a/b 有跨文件调用 → 同社区；tcp.rs 无边 → 自成社区
        assert_eq!(communities.len(), 2, "应产出 2 个社区");
        let ab = communities
            .iter()
            .find(|c| c.len() == 2)
            .expect("应存在含 2 文件的社区");
        let paths: Vec<String> = ab
            .iter()
            .map(|nid| {
                kg.graph
                    .node_weight(*nid)
                    .unwrap()
                    .file_path
                    .clone()
                    .unwrap()
            })
            .collect();
        assert!(paths.contains(&"src/a.rs".to_string()));
        assert!(paths.contains(&"src/b.rs".to_string()));
    }

    #[test]
    fn test_detect_communities_empty_graph() {
        let kg = KnowledgeGraph::default();
        assert!(detect_communities(&kg).is_empty());
    }

    #[test]
    fn test_detect_communities_single_file() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::File,
            name: "src/main.rs".into(),
            file_path: Some("src/main.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["src".into()],
        });
        let communities = detect_communities(&kg);
        assert_eq!(communities.len(), 1);
        assert_eq!(communities[0].len(), 1);
    }

    #[test]
    fn test_community_name_common_prefix() {
        let files = vec!["src/net/tcp.rs".to_string(), "src/net/udp.rs".to_string()];
        assert_eq!(community_name(&files, 0), "src::net");
    }

    #[test]
    fn test_community_name_single_file() {
        let files = vec!["src/config.rs".to_string()];
        assert_eq!(community_name(&files, 3), "src");
    }

    #[test]
    fn test_community_name_most_populated_dir() {
        // 无公共前缀（不同根）：按文件数最多目录回退
        let files = vec![
            "app/main.rs".to_string(),
            "app/util.rs".to_string(),
            "lib/helper.rs".to_string(),
        ];
        assert_eq!(community_name(&files, 1), "app");
    }

    #[test]
    fn test_community_name_fallback() {
        let files = vec!["main.rs".to_string()];
        assert_eq!(community_name(&files, 7), "module_7");
        assert_eq!(community_name(&[], 2), "module_2");
    }

    /// 稳定重排序（t07）：社区按大小降序编号——新增小社区不改变
    /// 既有大社区的相对序，模块编号/产物文件名跨次稳定
    #[test]
    fn test_detect_communities_stable_order() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        // 大社区：src/net/ 下 2 文件 + 2 实体（4 节点）
        let add_comm = |g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
                        path: &str| {
            let nid = g.add_node(CodeNode {
                id: NodeId::new(g.node_count()),
                kind: NodeKind::File,
                name: path.into(),
                file_path: Some(path.into()),
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: dir_segments(path),
            });
            let eid = g.add_node(CodeNode {
                id: NodeId::new(g.node_count()),
                kind: NodeKind::Function,
                name: format!("f{}", nid.index()),
                file_path: Some(path.into()),
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: Vec::new(),
            });
            g.add_edge(
                nid,
                eid,
                CodeEdge {
                    id: EdgeId::new(g.edge_count()),
                    kind: EdgeKind::Contains,
                    source: nid,
                    target: eid,
                    weight: 1.0,
                    location: None,
                },
            );
        };
        add_comm(g, "src/net/tcp.rs");
        add_comm(g, "src/net/udp.rs");
        // 小社区：孤立单文件
        add_comm(g, "src/util.rs");
        // 定位 tcp/udp 的 File 节点
        let f = |g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>, path: &str| {
            g.node_indices()
                .find(|n| g.node_weight(*n).map(|n| n.name == path).unwrap_or(false))
                .unwrap()
        };
        let _tcp = f(g, "src/net/tcp.rs");
        let _udp = f(g, "src/net/udp.rs");
        // tcp 的实体 → udp 的实体（Calls 边促成同社区）
        let fns: Vec<_> = g
            .node_indices()
            .filter(|n| {
                g.node_weight(*n)
                    .map(|n| n.kind == NodeKind::Function)
                    .unwrap_or(false)
            })
            .collect();
        g.add_edge(
            fns[0],
            fns[1],
            CodeEdge {
                id: EdgeId::new(g.edge_count()),
                kind: EdgeKind::Calls,
                source: fns[0],
                target: fns[1],
                weight: WEIGHT_CALLS,
                location: None,
            },
        );
        let communities = detect_communities(&kg);
        assert_eq!(communities.len(), 2);
        // 大社区（2 文件）必须排在单文件社区之前（大小降序）
        assert_eq!(communities[0].len(), 2, "大社区应排前（稳定重排序）");
        assert_eq!(communities[1].len(), 1);
    }

    /// v28 t10：目录阈值分流测试的合成图构造器——
    /// n_dirs 个目录 dirNN/，每目录 files_per_dir 个文件（各带 1 个实体），
    /// connected_pairs 指定跨目录 Calls 边（dir_a 的实体 0 → dir_b 的实体 0，
    /// 每目录至少 1 条边可被引用）。
    fn make_dirs_graph(
        n_dirs: usize,
        files_per_dir: usize,
        connected_pairs: &[(usize, usize)],
    ) -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        // 目录 → 该目录第一个文件实体的节点（供跨目录边引用）
        let mut first_entity: HashMap<String, NodeId> = HashMap::new();
        for d in 0..n_dirs {
            let dir = format!("src/dir{d:02}"); // src 域目录：跨目录 Leiden 混合
            for f in 0..files_per_dir {
                let path = format!("{dir}/f{f}.rs");
                let nid = g.add_node(CodeNode {
                    id: NodeId::new(g.node_count()),
                    kind: NodeKind::File,
                    name: path.clone(),
                    file_path: Some(path.clone()),
                    line_range: None,
                    doc_comment: None,
                    signature: None,
                    visibility: None,
                    module_path: dir_segments(&path),
                });
                let eid = g.add_node(CodeNode {
                    id: NodeId::new(g.node_count()),
                    kind: NodeKind::Function,
                    name: format!("f{f}"),
                    file_path: Some(path),
                    line_range: None,
                    doc_comment: None,
                    signature: None,
                    visibility: None,
                    module_path: Vec::new(),
                });
                g.add_edge(
                    nid,
                    eid,
                    CodeEdge {
                        id: EdgeId::new(g.edge_count()),
                        kind: EdgeKind::Contains,
                        source: nid,
                        target: eid,
                        weight: 1.0,
                        location: None,
                    },
                );
                first_entity.entry(dir.clone()).or_insert(eid);
            }
        }
        for (a, b) in connected_pairs {
            let ea = first_entity[&format!("src/dir{a:02}")];
            let eb = first_entity[&format!("src/dir{b:02}")];
            g.add_edge(
                ea,
                eb,
                CodeEdge {
                    id: EdgeId::new(g.edge_count()),
                    kind: EdgeKind::Calls,
                    source: ea,
                    target: eb,
                    weight: WEIGHT_CALLS,
                    location: None,
                },
            );
        }
        kg
    }

    /// v28 t10：目录阈值分流——20 目录（< 24）走实体级 Leiden：
    /// 跨目录调用把连接链上的文件合并成混合目录社区（社区数 > 目录数，
    /// 因为无边的独立文件按目录聚簇/不进 Leiden，产生更多社区），且两次
    /// 调用结果完全一致（输出确定性，固定种子）。夹具全走 src 顶层域
    /// （src/dirNN/），同一域内允许 Leiden 混合，避免被 U1 跨域剔除打散
    /// 原"跨目录混合社区"的断言意图。
    #[test]
    fn test_detect_communities_entity_level_below_threshold() {
        // 20 目录 × 2 文件；三组跨目录链连接（0-1-2、3-4），其余目录独立
        let kg = make_dirs_graph(20, 2, &[(0, 1), (1, 2), (3, 4)]);

        let first = detect_communities(&kg);
        let second = detect_communities(&kg);
        assert_eq!(first, second, "同图两次划分必须完全一致（确定性）");

        assert!(
            first.len() > 20,
            "实体级划分社区数应多于目录数（独立目录每文件一社区）, 实际 {} 个社区",
            first.len()
        );
        // 存在跨目录混合社区：连接链上的文件被 Leiden 合并
        let mixed = first.iter().any(|c| {
            let dirs_in: std::collections::HashSet<&str> = c
                .iter()
                .filter_map(|nid| {
                    kg.graph
                        .node_weight(*nid)
                        .and_then(|n| n.file_path.as_deref())
                })
                .filter_map(|p| p.rsplit_once('/').map(|(d, _)| d))
                .collect();
            dirs_in.len() > 1
        });
        assert!(mixed, "跨目录调用链应产生混合目录社区, 实际: {:?}", first);
    }

    /// v28 t10：目录阈值分流——24/30/40 目录（≥ 24）走目录级：
    /// 社区数 == 目录数，每社区恰为该目录的完整文件集；即使每对目录间
    /// 都有跨目录依赖边也不合并（目录级零随机参数，新增/删除文件不改
    /// 变划分）；两次调用一致（确定性）。
    #[test]
    fn test_detect_communities_dir_level_at_and_above_threshold() {
        // 24 目录 × 3 文件 / 30 目录 × 2 文件 / 40 目录 × 3 文件
        for (n_dirs, files_per_dir) in [(24usize, 3usize), (30, 2), (40, 3)] {
            // 全链跨目录边（最强制合并压力：目录级分流下必须仍按目录划分）
            let pairs: Vec<(usize, usize)> = (0..n_dirs - 1).map(|a| (a, a + 1)).collect();
            let kg = make_dirs_graph(n_dirs, files_per_dir, &pairs);

            let first = detect_communities(&kg);
            let second = detect_communities(&kg);
            assert_eq!(first, second, "{n_dirs} 目录两次划分必须一致（确定性）");
            assert_eq!(
                first.len(),
                n_dirs,
                "{n_dirs} 目录应产出 {n_dirs} 个社区, 实际 {}",
                first.len()
            );

            for comm in &first {
                let mut paths: Vec<String> = comm
                    .iter()
                    .filter_map(|nid| kg.graph.node_weight(*nid).and_then(|n| n.file_path.clone()))
                    .collect();
                paths.sort();
                assert_eq!(
                    paths.len(),
                    files_per_dir,
                    "每社区应为单目录全部文件: {paths:?}"
                );
                let dirs_in: std::collections::HashSet<&str> = paths
                    .iter()
                    .filter_map(|p| p.rsplit_once('/').map(|(d, _)| d))
                    .collect();
                assert_eq!(dirs_in.len(), 1, "社区内文件必须同属一个目录: {paths:?}");
            }
        }
    }

    /// v29：单目录仓库退化保护——目录数 == 1（平铺仓库，全部文件同一目录）
    /// 时直接走目录页路径：整库产出 1 个社区且包含全部文件（修复前走实体级
    /// Leiden：无跨文件边时每文件一社区、有边时整库聚成 1-2 个社区，模块
    /// 划分失去意义）；两次调用一致（确定性）。根目录散文件（<root> 键）
    /// 同此处理。
    #[test]
    fn test_detect_communities_single_dir_repo() {
        // 10 个文件全在 dir00/，无跨文件边（修复前实体级 Leiden → 10 社区）
        let kg = make_dirs_graph(1, 10, &[]);
        let first = detect_communities(&kg);
        let second = detect_communities(&kg);
        assert_eq!(first, second, "单目录仓库两次划分必须一致（确定性）");
        assert_eq!(
            first.len(),
            1,
            "单目录仓库应产出 1 个社区（整库一个模块）, 实际 {} 个",
            first.len()
        );
        assert_eq!(first[0].len(), 10, "社区应包含全部 10 个文件");
        let all_in_dir00 = first[0].iter().all(|nid| {
            kg.graph
                .node_weight(*nid)
                .and_then(|n| n.file_path.as_deref())
                .is_some_and(|p| p.starts_with("src/dir00/"))
        });
        assert!(all_in_dir00, "社区内所有文件必须同属唯一目录");

        // 根目录散文件（file_dir_key 归 <root>）同样整体一社区
        let mut kg2 = KnowledgeGraph::default();
        let g = &mut kg2.graph;
        for i in 0..5 {
            let path = format!("main{i}.rs");
            let nid = g.add_node(CodeNode {
                id: NodeId::new(g.node_count()),
                kind: NodeKind::File,
                name: path.clone(),
                file_path: Some(path),
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: Vec::new(),
            });
            let eid = g.add_node(CodeNode {
                id: NodeId::new(g.node_count()),
                kind: NodeKind::Function,
                name: format!("f{i}"),
                file_path: None,
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: Vec::new(),
            });
            g.add_edge(
                nid,
                eid,
                CodeEdge {
                    id: EdgeId::new(g.edge_count()),
                    kind: EdgeKind::Contains,
                    source: nid,
                    target: eid,
                    weight: 1.0,
                    location: None,
                },
            );
        }
        let comms = detect_communities(&kg2);
        assert_eq!(comms.len(), 1, "根目录散文件仓库也应整体一社区");
        assert_eq!(comms[0].len(), 5);
    }

    // ---- U1 域内策略新增测试 ----

    /// 新增文件到图：File 节点 + 其 Function 实体（Contains 边），返回 (File, 实体)
    fn add_file_with_entity(
        g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
        path: &str,
    ) -> (NodeId, NodeId) {
        let nid = g.add_node(CodeNode {
            id: NodeId::new(g.node_count()),
            kind: NodeKind::File,
            name: path.into(),
            file_path: Some(path.into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: dir_segments(path),
        });
        let eid = g.add_node(CodeNode {
            id: NodeId::new(g.node_count()),
            kind: NodeKind::Function,
            name: format!("f{}", nid.index()),
            file_path: Some(path.into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: Vec::new(),
        });
        g.add_edge(
            nid,
            eid,
            CodeEdge {
                id: EdgeId::new(g.edge_count()),
                kind: EdgeKind::Contains,
                source: nid,
                target: eid,
                weight: 1.0,
                location: None,
            },
        );
        (nid, eid)
    }

    /// 两实体间 Calls 边
    fn add_calls(
        g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
        from: NodeId,
        to: NodeId,
    ) {
        g.add_edge(
            from,
            to,
            CodeEdge {
                id: EdgeId::new(g.edge_count()),
                kind: EdgeKind::Calls,
                source: from,
                target: to,
                weight: WEIGHT_CALLS,
                location: None,
            },
        );
    }

    /// 按 file_path 归集社区的节点路径集合（断言用）
    fn community_paths(kg: &KnowledgeGraph, comm: &[NodeId]) -> Vec<String> {
        let mut v: Vec<String> = comm
            .iter()
            .filter_map(|nid| kg.graph.node_weight(*nid).and_then(|n| n.file_path.clone()))
            .collect();
        v.sort();
        v
    }

    /// U1：跨顶层域边剔除——src/a.rs ↔ src/b.rs 同域合并一社区，src/a.rs ↔
    /// tests/t.rs 跨域不合并 → tests/t.rs 独立成社区（不走 Leiden，目录聚簇）。
    #[test]
    fn test_cross_domain_edges_excluded() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let (_a, ea) = add_file_with_entity(g, "src/a.rs");
        let (_b, eb) = add_file_with_entity(g, "src/b.rs");
        let (_t, et) = add_file_with_entity(g, "tests/t.rs");
        // 同域 src 互调 + 跨域 src↔tests 调用（均挂 Calls 边）
        add_calls(g, ea, eb);
        add_calls(g, ea, et);

        let communities = detect_communities(&kg);
        // src::a + src::b 应同社区；tests::t 独立
        assert_eq!(
            communities.len(),
            2,
            "应产出 2 个社区, 实际 {:?}",
            communities
        );
        let src_comm = communities
            .iter()
            .find(|c| c.len() == 2)
            .expect("src 同域边应合并为 2 文件社区");
        let sp = community_paths(&kg, src_comm);
        assert!(sp.contains(&"src/a.rs".to_string()));
        assert!(sp.contains(&"src/b.rs".to_string()));
        assert!(
            !sp.contains(&"tests/t.rs".to_string()),
            "跨域文件不得并入 src 社区"
        );

        let tests_comm = communities
            .iter()
            .find(|c| {
                community_paths(&kg, c)
                    .iter()
                    .any(|p| p.starts_with("tests/"))
            })
            .expect("tests/t.rs 应独立成社区");
        assert_eq!(
            tests_comm.len(),
            1,
            "tests 域不参与 Leiden，单文件一社区(tests)"
        );
    }

    /// U1：非 src 域文件全部按目录聚簇（不走 Leiden）——tests/clustering/ 下
    /// 两文件即使互调，也按目录同社区（2 文件）；tests/bench/ 独立文件按目录
    /// 一对文件一社区（1 文件）。
    #[test]
    fn test_non_src_domain_grouped_by_dir() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let (_x, ex) = add_file_with_entity(g, "tests/clustering/x.rs");
        let (_y, ey) = add_file_with_entity(g, "tests/clustering/y.rs");
        let (_z, _ez) = add_file_with_entity(g, "tests/bench/z.rs");
        add_calls(g, ex, ey); // 同域测试文件互调，但非 src → 仍按目录聚簇

        let communities = detect_communities(&kg);
        let pairs: Vec<Vec<String>> = communities
            .iter()
            .map(|c| community_paths(&kg, c))
            .collect();
        // tests/clustering 一社区（2 文件）+ tests/bench 一社区（1 文件）
        let clustering = pairs
            .iter()
            .find(|p| p.contains(&"tests/clustering/x.rs".to_string()))
            .expect("应存在 clustering 社区");
        assert_eq!(
            clustering.len(),
            2,
            "同目录互调测试文件按目录聚簇同社区: {clustering:?}"
        );
        assert!(clustering.contains(&"tests/clustering/y.rs".to_string()));
        let bench = pairs
            .iter()
            .find(|p| p.contains(&"tests/bench/z.rs".to_string()))
            .expect("应存在 bench 社区");
        assert_eq!(bench.len(), 1, "独立测试文件按目录聚簇单文件: {bench:?}");
        // 跨目录互调未把 x 与 z 并入（不同目录键）
        assert!(
            !pairs
                .iter()
                .any(|p| p.contains(&"tests/clustering/x.rs".to_string())
                    && p.contains(&"tests/bench/z.rs".to_string())),
            "非 src 不同目录不得合并: {pairs:?}"
        );
    }

    /// U1：`tests/common/` 目录键并入父目录（tests）——tests/foo.rs 与
    /// tests/common/mod.rs 互调 → 同社区（都为 tests）。
    #[test]
    fn test_common_dir_merged_into_parent() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let (_foo, efoo) = add_file_with_entity(g, "tests/foo.rs");
        let (_common, ecommon) = add_file_with_entity(g, "tests/common/mod.rs");
        add_calls(g, efoo, ecommon);

        let communities = detect_communities(&kg);
        assert_eq!(
            communities.len(),
            1,
            "common 并入父目录后应只剩 1 社区, 实际 {:?}",
            communities
        );
        assert_eq!(
            communities[0].len(),
            2,
            "tests/foo.rs 与 tests/common/mod.rs 应同社区"
        );
    }

    /// U1：src 域孤立文件（无任何同域边）不进 Leiden，按目录聚簇——src/mod/
    /// 下 lib.rs 与 other.rs 互调（进 Leiden，同社区），src/test/git.rs 无任何
    /// 边（src 孤立，按目录聚簇独立成社区）。
    ///
    /// 放置在不同子目录（src/mod、src/test）使目录数 >1 绕过单目录早退，
    /// 否则全部平铺 src/ 会命中 `dirs.len()<=1` 的整库单社区保护。
    #[test]
    fn test_src_isolated_grouped_by_dir() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let (_lib, elib) = add_file_with_entity(g, "src/mod/lib.rs");
        let (_other, eother) = add_file_with_entity(g, "src/mod/other.rs");
        let (_git, _egit) = add_file_with_entity(g, "src/test/git.rs");
        add_calls(g, elib, eother);

        let communities = detect_communities(&kg);
        // lib+other 同社区（Leiden，2 文件）；git.rs 独立成社区（src 孤立目录簇）
        let lib_comm = communities
            .iter()
            .find(|c| c.len() == 2)
            .expect("src 互调文件应合并为 2 文件社区");
        let lp = community_paths(&kg, lib_comm);
        assert!(lp.contains(&"src/mod/lib.rs".to_string()));
        assert!(lp.contains(&"src/mod/other.rs".to_string()));
        assert!(
            !lp.contains(&"src/test/git.rs".to_string()),
            "孤立文件不得并入 lib 社区"
        );

        let git_comm = communities
            .iter()
            .find(|c| {
                community_paths(&kg, c)
                    .iter()
                    .any(|p| p == "src/test/git.rs")
            })
            .expect("src/test/git.rs 应独立成社区");
        assert_eq!(git_comm.len(), 1, "src 孤立文件按目录聚簇为独立单文件社区");
    }

    /// U1：top_level_domain 纯函数表驱动断言（反斜杠归一、根散文件/空串归 root）
    #[test]
    fn test_top_level_domain() {
        assert_eq!(top_level_domain("src/a.rs"), "src");
        assert_eq!(top_level_domain("tests/t.rs"), "tests");
        assert_eq!(top_level_domain("a.rs"), "<root>");
        assert_eq!(top_level_domain(r"src\a\b.rs"), "src");
        assert_eq!(top_level_domain(""), "<root>");
    }

    /// U1：quality API 冒烟——src 域有边时 Leiden 产出有限质量；目录分流早退
    /// （根散文件仓库）返回 0.0 语义值。
    #[test]
    fn test_detect_communities_quality_smoke() {
        // make_graph（src/a↔b 互调 + src/net/tcp 独立）→ Leiden 产出真实 CPM 质量
        let kg = make_graph();
        let (_communities, quality) = detect_communities_with_quality(&kg, LEIDEN_RESOLUTION);
        assert!(
            quality.is_finite(),
            "Leiden quality 应为有限值, 实际 {quality}"
        );
        assert!(quality >= 0.0, "CPM quality 应非负, 实际 {quality}");

        // 根目录散文件仓库（目录数==1 早退）→ quality 语义 0.0
        let mut flat = KnowledgeGraph::default();
        let g = &mut flat.graph;
        add_file_with_entity(g, "main.rs");
        let (_comm, q) = detect_communities_with_quality(&flat, LEIDEN_RESOLUTION);
        assert_eq!(q, 0.0, "目录分流早退无 Leiden，quality 应为 0.0");
    }

    /// U1 补充：社区大小上限拆分（纯函数直测）——25 文件大社区按目录键
    /// 拆成 5 个子簇（每目录 5 文件，目录内不分家）；≤上限社区原样保留。
    #[test]
    fn test_split_oversized_communities_by_dir() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        // 25 文件分 5 目录（每目录 5 文件），构造单一"大社区"输入
        let mut big_community: Vec<NodeId> = Vec::new();
        for d in 0..5 {
            for f in 0..5 {
                let (file, _e) = add_file_with_entity(g, &format!("src/d{d:02}/f{f}.rs"));
                big_community.push(file);
            }
        }
        // 小社区（2 文件，≤20）应原样保留
        let (small_a, _ea) = add_file_with_entity(g, "src/small/a.rs");
        let (small_b, _eb) = add_file_with_entity(g, "src/small/b.rs");

        let out = split_oversized_communities(&kg, vec![big_community, vec![small_a, small_b]], 20);
        // 大社区拆成 5 个子簇（每目录 5 文件），小社区原样 1 簇
        assert_eq!(out.len(), 6, "应拆成 5 子簇 + 1 小社区, 实际: {out:?}");
        for comm in &out {
            assert!(comm.len() <= 20, "社区不得超过上限: {comm:?}");
        }
        // 目录完整性：每个 src/dNN 目录的 5 文件必须同簇（拆分键=目录，
        // BTreeMap 键序 d00..d04 → 前 5 个子簇与目录一一对应）
        for (d, comm) in out.iter().take(5).enumerate() {
            let paths = community_paths(&kg, comm);
            assert_eq!(paths.len(), 5, "子簇应含 5 文件: {paths:?}");
            assert!(
                paths
                    .iter()
                    .all(|p| p.starts_with(&format!("src/d{d:02}/"))),
                "子簇 {d} 应只含 d{d:02} 目录文件: {paths:?}"
            );
        }
        // 小社区原样保留（内容不变）
        let small_paths = community_paths(&kg, &out[5]);
        assert_eq!(
            small_paths,
            vec!["src/small/a.rs".to_string(), "src/small/b.rs".to_string()]
        );
        // 文件总数不变（拆分不丢文件）
        let total: usize = out.iter().map(|c| c.len()).sum();
        assert_eq!(total, 27, "拆分不得丢失文件");
    }

    /// U1 补充：detect 集成断言——src 强连接仓库（25 文件链式互调）划分结果
    /// 任何社区不超过上限 20（大小拆分生效；Leiden 本身可能已切成小段，
    /// 该断言在两种情形下都成立，只约束"不超限"这一不变量）。
    #[test]
    fn test_detect_communities_respect_size_cap() {
        let mut kg = KnowledgeGraph::default();
        let g = &mut kg.graph;
        let mut prev_entity: Option<NodeId> = None;
        for d in 0..5 {
            for f in 0..5 {
                let path = format!("src/d{d:02}/f{f}.rs");
                let (_file, entity) = add_file_with_entity(g, &path);
                if let Some(pe) = prev_entity {
                    add_calls(g, pe, entity);
                }
                prev_entity = Some(entity);
            }
        }

        let communities = detect_communities(&kg);
        assert!(
            communities.iter().all(|c| c.len() <= 20),
            "任何社区不得超过上限 20, 实际: {:?}",
            communities
        );
        let total: usize = communities.iter().map(|c| c.len()).sum();
        assert_eq!(total, 25, "划分不得丢失文件");
    }
}
