use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use petgraph::visit::{EdgeRef, IntoNodeIdentifiers};

use crate::model::{EdgeKind, KnowledgeGraph, NodeId};

use super::change::{EntityChangeKind, EntityChangeSet};

/// body 传播（函数体变化）沿 Incoming Calls 边的最大深度：仅 1 层。
///
/// 函数体行为变化不改变调用约定（签名未变），调用方的调用方式不受影响，
/// 但调用方页面若描述被调函数的行为则需随行为刷新——深度 1 已覆盖
/// "直接调用方"这一层；再深（调用方的调用方）与函数体变化的关系已
/// 无关，继续传播只会造成过度重生成。仅沿 Calls 边：Imports 边是
/// 模块引用关系，不因函数体变化而变。
const BODY_CALLER_DEPTH: usize = 1;

/// 在知识图谱上传播变更影响，返回所有受影响的模块名称
///
/// 从变更文件节点出发，沿 Imports/Calls 边双向 BFS 遍历 3 层，
/// 找到所有直接或间接受影响的模块。
pub fn propagate_impact(
    changed_files: &[PathBuf],
    graph: &KnowledgeGraph,
    max_depth: usize,
) -> Vec<String> {
    if graph.graph.node_count() == 0 {
        return Vec::new();
    }

    let file_paths: Vec<String> = changed_files
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    let start_nodes = find_start_nodes(&file_paths, graph);
    if start_nodes.is_empty() {
        return Vec::new();
    }

    let mut result: Vec<String> = propagate_from(start_nodes, graph, max_depth)
        .into_iter()
        .collect();
    result.sort();
    result
}

/// 将受影响模块名反查为文件路径集合（T2 传播闭环接线）
///
/// 影响传播（propagate_impact / propagate_impact_semantic）产出的是
/// `CodeNode.module_path.join("::")`（目录 + 文件 stem 派生，如
/// "src::net::tcp"），与 `ModuleCluster.name`（社区名，如 "src"）不是
/// 同一套命名——因此反查按 **File 节点的 module_path 精确匹配**，
/// 而非社区名匹配。生成过滤（run_generation_filtered）用返回值把
/// 受影响模块的文件并入变更集，实现"签名变更 → 依赖方模块文档
/// 重生成"的语义传播闭环。
pub fn module_files(affected_modules: &[String], graph: &KnowledgeGraph) -> Vec<PathBuf> {
    let target: HashSet<&str> = affected_modules.iter().map(|s| s.as_str()).collect();
    let mut files: Vec<PathBuf> = graph
        .graph
        .node_identifiers()
        .filter_map(|nid| {
            let node = &graph.graph[nid];
            // File 与实体节点都参与匹配：传播途经节点（含函数等实体）的
            // module_path 带文件 stem（如 "src::http::server"），而 File 节点
            // 的 module_path 只到目录（如 "src::http"）——实体节点同样携带
            // file_path，按其实体名匹配后取文件路径去重。
            let mp = node.module_path.join("::");
            if target.contains(mp.as_str()) {
                node.file_path.as_ref().map(PathBuf::from)
            } else {
                None
            }
        })
        .collect();
    files.sort();
    files.dedup();
    files
}

/// 删除文件的「反向引用失效」：返回引用了任一被删文件的模块名集合
///
/// 根因（Phase A10 / I3）：源码文件被删除时，被删文件在知识图谱中已无节点
/// （find_start_nodes 按路径找不到即返回空起点），语义传播
/// （propagate_impact / propagate_impact_semantic）的起点缺失使「被删文件
/// 自身」不触发任何模块受影响——正文引用了它的页面（path:line 引用、
/// `- `code`` 相关文件列表）残留对已删文件的引用，lint 的 bad-citation /
/// source-missing / orphan 等错误由此产生。
///
/// 本函数从旧导出快照（.state/export_snapshot.json，记录上一轮完整生成集）
/// 构建「源文件 → 引用它的模块」反向索引，复用现有提取工具（DRY）：
/// - citation::extract_citations：页面正文的 `path:line` 引用（LLM 生成
///   内容中最常见的源码引用形态）；
/// - lint::extract_source_files：页面正文 `- `code`` 相关文件列表形态；
/// - KnowledgeCard.related_files：卡片的结构化相关文件字段。
///
/// 凡引用命中被删文件的模块并入返回集，由调用方并入 affected_modules 触发
/// 重生成——重生成时引用校验（validate_citations_against_entities）会强制
/// LLM 剔除对已删文件的引用，页面随之自愈。
///
/// 边界：只失效「确实引用了被删文件」的页面，不做无差别全量重生成
/// （避免烧 credits）；被删文件可能同时被多个模块引用，逐个命中即失效。
/// 全局文档（module_path 为空）无模块归属，不参与（api.md 由代码图渲染
/// 自愈，架构/概览由既有 has_interface_change 路径重生成）。
pub fn reverse_reference_affected_modules(
    deleted_files: &std::collections::HashSet<std::path::PathBuf>,
    snapshot: &crate::output::ExportSnapshot,
) -> Vec<String> {
    // 归一化比较：快照引用路径（git 正斜杠）与 changed_files 路径
    // （Windows 反斜杠）统一替换为 "/" 后按字符串精确比较
    // （与 norm_sep 同规则，见 incremental/mod.rs:137）。
    let deleted: HashSet<String> = deleted_files
        .iter()
        .map(|p| super::norm_sep(&p.to_string_lossy()))
        .collect();
    let mut affected: HashSet<String> = HashSet::new();

    // Wiki 页面正文：path:line 引用 + `- `code`` 相关文件列表
    for doc in &snapshot.documents {
        if doc.kind != crate::model::DocumentKind::WikiPage {
            continue;
        }
        let cites = crate::output::citation::extract_citations(&doc.content);
        let source_files = crate::output::lint::extract_source_files(&doc.content);
        let referenced_deleted = cites
            .iter()
            .map(|c| super::norm_sep(&c.path))
            .any(|p| deleted.contains(&p))
            || source_files
                .iter()
                .map(|f| super::norm_sep(f))
                .any(|f| deleted.contains(&f));
        if referenced_deleted {
            let module = doc.module_path.join("::");
            if !module.is_empty() {
                affected.insert(module);
            }
        }
    }

    // 卡片：related_files 结构化字段（渲染为「相关文件」段的引用形态）
    for card in &snapshot.cards {
        if card
            .related_files
            .iter()
            .map(|f| super::norm_sep(f))
            .any(|f| deleted.contains(&f))
        {
            affected.insert(card.module_name.clone());
        }
    }

    let mut result: Vec<String> = affected.into_iter().collect();
    result.sort();
    result
}

/// 语义影响传播：区分接口级与实现级变化（演进计划 T2.2）
///
/// 分类语义（change.rs）：
/// - 接口级变化（新增/删除/签名变更）：会影响调用方，向依赖方双向传播；
/// - 实现级变化（仅函数体修改）：只影响本模块产物。
///
/// 粒度说明：生成过滤是文件级（run_generation_filtered 按 changed_files），
/// 因此接口级判定也按文件——文件内任一实体是接口级变化，整个文件按
/// 接口级传播（保守，宁多勿漏）。
///
/// `entity_changes` 为空（FileWatch 策略无 git 信息、或分类失败）时
/// 回退保守的双向传播（现状行为），保证不丢影响。
pub fn propagate_impact_semantic(
    changed_files: &[PathBuf],
    entity_changes: &EntityChangeSet,
    graph: &KnowledgeGraph,
    max_depth: usize,
) -> Vec<String> {
    if graph.graph.node_count() == 0 {
        return Vec::new();
    }
    if entity_changes.changes.is_empty() {
        return propagate_impact(changed_files, graph, max_depth);
    }

    // 接口级变化的文件集合
    let interface_files: HashSet<String> = entity_changes
        .changes
        .iter()
        .filter(|c| {
            matches!(
                c.kind,
                EntityChangeKind::Added
                    | EntityChangeKind::Removed
                    | EntityChangeKind::SignatureChanged
            )
        })
        .map(|c| c.file.to_string_lossy().to_string())
        .collect();
    // 实现级变化（BodyChanged）的文件集合：函数体行为变化虽不改变调用约定，
    // 但调用方页面若描述被调函数行为则需随行为刷新（下方 body 传播分支，
    // 修复"body 只影响本模块"导致的调用方页面漏更）。
    let body_files: HashSet<String> = entity_changes
        .changes
        .iter()
        .filter(|c| c.kind == EntityChangeKind::BodyChanged)
        .map(|c| c.file.to_string_lossy().to_string())
        .collect();

    let mut affected: HashSet<String> = HashSet::new();
    // 起点批量查询：一次全图遍历按变更文件分组匹配起点（替代逐文件
    // find_start_nodes 的 N×M 重复遍历——N 个变更文件 × M 个图节点，
    // 大仓库多 body 变更时是语义传播路径的主要常数成本）。
    let file_strings: Vec<String> = changed_files
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let start_nodes_by_file = find_start_nodes_per_file(&file_strings, graph);
    for file in changed_files {
        let fp = file.to_string_lossy().to_string();
        // 图中无此文件的节点（未进入图谱）→ 该文件不产生任何影响
        let Some(start_nodes) = start_nodes_by_file.get(&fp) else {
            continue;
        };
        if interface_files.contains(&fp) {
            // 接口级：双向传播（clone：start_nodes 随后还要供 body 分支用）
            affected.extend(propagate_from(start_nodes.clone(), graph, max_depth));
        } else {
            // 实现级：仅起点所在模块
            for nid in start_nodes {
                let module = graph.graph[*nid].module_path.join("::");
                if !module.is_empty() {
                    affected.insert(module);
                }
            }
        }
        // body 传播分支：函数体行为变化时，直接调用方页面也应刷新（否则
        // 调用方文档停留在旧行为描述，构成文档漂移漏更）。与接口级分支
        // 可共存（同文件既签名变又改体）：body 分支只补深度 1 的 Calls 边
        // 调用方，不沿 Imports、不加深层级、不做局部更新（整页重写）。
        if body_files.contains(&fp) {
            affected.extend(propagate_body_callers(
                start_nodes.clone(),
                graph,
                BODY_CALLER_DEPTH,
            ));
        }
    }

    let mut result: Vec<String> = affected.into_iter().collect();
    result.sort();
    result
}

/// body 传播：沿 Incoming Calls 边向上传播 N 层（实现级变化的调用方刷新）
///
/// 与接口级传播（propagate_from）的区别：
/// - 只沿 Incoming 方向（调用方 ← 被调方），不沿 outgoing 边（被调方的
///   页面不因调用方函数体变化而变）；
/// - 只认 Calls 边，不认 Imports（导入关系不因函数体变化而变）；
/// - 深度受限（调用方传 BODY_CALLER_DEPTH=1），不做无界传播。
fn propagate_body_callers(
    start_nodes: Vec<NodeId>,
    graph: &KnowledgeGraph,
    max_depth: usize,
) -> HashSet<String> {
    let mut affected: HashSet<String> = HashSet::new();

    for &start in &start_nodes {
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut queue: VecDeque<(NodeId, usize)> = VecDeque::new();
        queue.push_back((start, 0));
        visited.insert(start);

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            for edge in graph
                .graph
                .edges_directed(current, petgraph::Direction::Incoming)
            {
                if graph.graph[edge.id()].kind != EdgeKind::Calls {
                    continue;
                }
                let neighbor = edge.source();
                if visited.contains(&neighbor) {
                    continue;
                }
                let module = graph.graph[neighbor].module_path.join("::");
                if !module.is_empty() {
                    affected.insert(module);
                }
                visited.insert(neighbor);
                queue.push_back((neighbor, depth + 1));
            }
        }
    }

    affected
}

/// 路径匹配判定：节点文件路径 fp 是否命中变更路径 cfp（路径段为界）
///
/// fp/cfp 均已 norm_sep 归一化为正斜杠；trim 尾部 '/' 后按路径段比较：
/// "src/a.ts" 不再命中 "src/a.tsx"（a.ts 是 a.tsx 的子串），"src/a"
/// 目录级变更仍命中其下文件（starts_with(cfp + "/")）。find_start_nodes
/// 与 find_start_nodes_per_file 共用，保证两条查询路径匹配语义一致。
fn path_matches(fp: &str, cfp: &str) -> bool {
    let cfp = cfp.trim_end_matches('/');
    fp == cfp || fp.starts_with(&format!("{cfp}/"))
}

/// 按文件路径（精确匹配）找到图中的起点节点
///
/// 匹配以路径段为界：节点 file_path 等于变更路径，或以「变更路径 +
/// '/'」开头（目录级变更兼容，changed_files 理论为文件级，但保留
/// 目录前缀语义避免误伤既有行为）。子串匹配会把 "src/a.ts" 的变更
/// 误判到 "src/a.tsx" 节点（a.ts 与 a.tsx 是不同文件），必须避免。
/// 路径先经 norm_sep 归一化（git 正斜杠 vs Windows 反斜杠），
/// 否则 Windows 上 git diff 路径永远匹配不到图中的节点。
fn find_start_nodes(file_paths: &[String], graph: &KnowledgeGraph) -> Vec<NodeId> {
    let normalized: Vec<String> = file_paths.iter().map(|p| super::norm_sep(p)).collect();
    graph
        .graph
        .node_identifiers()
        .filter(|&nid| {
            let node = &graph.graph[nid];
            node.file_path
                .as_ref()
                .map(|fp| {
                    let fp = super::norm_sep(fp);
                    normalized.iter().any(|cfp| path_matches(&fp, cfp))
                })
                .unwrap_or(false)
        })
        .collect()
}

/// 按文件路径分组查询起点节点（批量版，P2 优化）
///
/// 一次全图遍历把每个变更文件匹配到的起点节点按原路径分组返回，供
/// propagate_impact_semantic 复用——逐文件调用 find_start_nodes 会重复
/// N 次全图遍历与路径归一化（N=变更文件数、M=图节点数，N×M 开销，
/// 大仓库多 body 变更时是语义传播路径的主要常数成本）。组内保持
/// 图节点序，匹配语义与 find_start_nodes 完全一致（共用 path_matches）。
fn find_start_nodes_per_file(
    file_paths: &[String],
    graph: &KnowledgeGraph,
) -> HashMap<String, Vec<NodeId>> {
    let normalized: Vec<String> = file_paths.iter().map(|p| super::norm_sep(p)).collect();
    let mut map: HashMap<String, Vec<NodeId>> = HashMap::new();
    for nid in graph.graph.node_identifiers() {
        let node = &graph.graph[nid];
        if let Some(fp) = node.file_path.as_ref() {
            let fp = super::norm_sep(fp);
            for (i, cfp) in normalized.iter().enumerate() {
                if path_matches(&fp, cfp) {
                    map.entry(file_paths[i].clone()).or_default().push(nid);
                }
            }
        }
    }
    map
}

/// 从起点集合双向 BFS 传播影响，返回受影响模块名集合（起点自身计入）
fn propagate_from(
    start_nodes: Vec<NodeId>,
    graph: &KnowledgeGraph,
    max_depth: usize,
) -> HashSet<String> {
    let mut affected: HashSet<String> = HashSet::new();

    for &start in &start_nodes {
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut queue: VecDeque<(NodeId, usize)> = VecDeque::new();
        queue.push_back((start, 0));
        visited.insert(start);

        // 起始节点本身也标记为受影响
        let start_node = &graph.graph[start];
        if !start_node.module_path.is_empty() {
            affected.insert(start_node.module_path.join("::"));
        }

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }

            // 双向遍历：出边（依赖别人）和入边（被别人依赖）
            // edges() 产出 current 作为 source 的边；edges_directed(_, Incoming)
            // 产出 current 作为 target 的边。合并后逐一检查。
            for edge in graph.graph.edges(current).chain(
                graph
                    .graph
                    .edges_directed(current, petgraph::Direction::Incoming),
            ) {
                // 确定"另一端的节点"
                let neighbor = if edge.source() == current {
                    edge.target()
                } else {
                    edge.source()
                };
                if visited.contains(&neighbor) {
                    continue;
                }

                let kind = &graph.graph[edge.id()].kind;
                if !matches!(kind, EdgeKind::Imports | EdgeKind::Calls) {
                    continue;
                }

                let neighbor_node = &graph.graph[neighbor];
                if !neighbor_node.module_path.is_empty() {
                    affected.insert(neighbor_node.module_path.join("::"));
                }

                visited.insert(neighbor);
                queue.push_back((neighbor, depth + 1));
            }
        }
    }

    affected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CodeEdge, CodeNode, NodeKind};
    use petgraph::stable_graph::StableDiGraph;

    fn make_simple_graph() -> KnowledgeGraph {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();

        let core = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Module,
            name: "core".into(),
            file_path: Some("src/core.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["core".into()],
        });

        let net = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Module,
            name: "net".into(),
            file_path: Some("src/net.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["net".into()],
        });

        let db = g.add_node(CodeNode {
            id: NodeId::new(2),
            kind: NodeKind::Module,
            name: "db".into(),
            file_path: Some("src/db.rs".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["db".into()],
        });

        g.add_edge(
            core,
            net,
            CodeEdge {
                id: petgraph::stable_graph::EdgeIndex::new(0),
                kind: EdgeKind::Imports,
                source: core,
                target: net,
                weight: 1.0,
                location: None,
            },
        );

        g.add_edge(
            net,
            db,
            CodeEdge {
                id: petgraph::stable_graph::EdgeIndex::new(1),
                kind: EdgeKind::Imports,
                source: net,
                target: db,
                weight: 1.0,
                location: None,
            },
        );

        KnowledgeGraph {
            graph: g,
            modules: vec![],
            features: Vec::new(),
        }
    }

    #[test]
    fn test_propagate_impact() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let affected = propagate_impact(&changed, &graph, 3);

        assert!(affected.contains(&"db".to_string()));
        assert!(affected.contains(&"net".to_string()));
        assert!(affected.contains(&"core".to_string()));
    }

    #[test]
    fn test_no_impact_for_unknown_file() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("unknown.rs")];
        let affected = propagate_impact(&changed, &graph, 3);
        assert!(affected.is_empty());
    }

    #[test]
    fn test_empty_graph() {
        let graph = KnowledgeGraph::default();
        let changed = vec![PathBuf::from("src/main.rs")];
        let affected = propagate_impact(&changed, &graph, 3);
        assert!(affected.is_empty());
    }

    #[test]
    fn test_impact_reverse_propagation() {
        // 测试反向传播：db.rs 变更 → net（导入 db）→ core（依赖 net）应全部受影响
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let affected = propagate_impact(&changed, &graph, 3);

        assert!(affected.contains(&"db".to_string()));
        assert!(affected.contains(&"net".to_string()));
        assert!(affected.contains(&"core".to_string()));
        assert_eq!(affected.len(), 3);
    }

    #[test]
    fn test_impact_no_duplicate_modules() {
        // 验证同一模块不会重复出现在结果中
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let affected = propagate_impact(&changed, &graph, 3);

        let unique: std::collections::HashSet<_> = affected.iter().cloned().collect();
        assert_eq!(affected.len(), unique.len());
    }

    /// 语义传播：仅实现级变化（BodyChanged）→ 只影响本模块，不向依赖方传播
    #[test]
    fn test_semantic_body_change_only_local() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let changes = EntityChangeSet {
            changes: vec![crate::incremental::change::EntityChange {
                file: PathBuf::from("src/db.rs"),
                entity_name: "load".into(),
                kind: crate::incremental::change::EntityChangeKind::BodyChanged,
                old_range: Some((1, 5)),
                new_range: Some((1, 8)),
            }],
        };
        let affected = propagate_impact_semantic(&changed, &changes, &graph, 3);
        // 仅 db 自身（net 导入 db、core 依赖 net 都不应受影响）
        assert_eq!(affected, vec!["db".to_string()]);
    }

    /// 构造带 Calls/Imports 混合边的图，验证 body 传播只沿 Incoming Calls 边一层：
    /// - Calls 边：x→a→b→c（x 调用 a，a 调用 b，b 调用 c）
    /// - Imports 边：d→b（d 导入 b，但并非调用）
    fn make_calls_graph() -> KnowledgeGraph {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        let mut mk = |id: usize, name: &str, path: &str| {
            g.add_node(CodeNode {
                id: NodeId::new(id),
                kind: NodeKind::Module,
                name: name.into(),
                file_path: Some(path.into()),
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: vec![name.into()],
            })
        };
        let x = mk(0, "x", "src/x.rs");
        let a = mk(1, "a", "src/a.rs");
        let b = mk(2, "b", "src/b.rs");
        let c = mk(3, "c", "src/c.rs");
        let d = mk(4, "d", "src/d.rs");
        let mut edge_id = 0usize;
        for (src, dst) in [(x, a), (a, b), (b, c)] {
            g.add_edge(
                src,
                dst,
                CodeEdge {
                    id: petgraph::stable_graph::EdgeIndex::new(edge_id),
                    kind: EdgeKind::Calls,
                    source: src,
                    target: dst,
                    weight: 1.0,
                    location: None,
                },
            );
            edge_id += 1;
        }
        g.add_edge(
            d,
            b,
            CodeEdge {
                id: petgraph::stable_graph::EdgeIndex::new(edge_id),
                kind: EdgeKind::Imports,
                source: d,
                target: b,
                weight: 1.0,
                location: None,
            },
        );
        KnowledgeGraph {
            graph: g,
            modules: vec![],
            features: Vec::new(),
        }
    }

    /// body 传播：BodyChanged + Calls 边 → 直接调用方入受影响集；
    /// 深度 2+ 的调用方（x）与仅 Imports 边的导入方（d）不入集；
    /// 被调用方（c，outgoing Calls）不入集。
    #[test]
    fn test_semantic_body_change_calls_propagates_direct_callers() {
        let graph = make_calls_graph();
        let changed = vec![PathBuf::from("src/b.rs")];
        let changes = EntityChangeSet {
            changes: vec![crate::incremental::change::EntityChange {
                file: PathBuf::from("src/b.rs"),
                entity_name: "f".into(),
                kind: crate::incremental::change::EntityChangeKind::BodyChanged,
                old_range: Some((1, 3)),
                new_range: Some((1, 5)),
            }],
        };
        let affected = propagate_impact_semantic(&changed, &changes, &graph, 3);
        assert!(
            affected.contains(&"b".to_string()),
            "起点模块必须受影响: {:?}",
            affected
        );
        assert!(
            affected.contains(&"a".to_string()),
            "直接调用方（Incoming Calls 深度 1）必须受影响: {:?}",
            affected
        );
        assert!(
            !affected.contains(&"x".to_string()),
            "深度 2+ 的调用方不得受影响（body 传播仅 1 层）: {:?}",
            affected
        );
        assert!(
            !affected.contains(&"d".to_string()),
            "仅 Imports 边的导入方不得因函数体变化受影响: {:?}",
            affected
        );
        assert!(
            !affected.contains(&"c".to_string()),
            "被调用方（outgoing 方向）不得受影响: {:?}",
            affected
        );
        assert_eq!(
            affected.len(),
            2,
            "应只含起点模块与直接调用方: {:?}",
            affected
        );
    }

    /// body 传播：仅 Imports 边的图（无 Calls 边）→ 不向任何导入方传播
    #[test]
    fn test_semantic_body_change_imports_only_not_propagated() {
        let graph = make_simple_graph(); // 仅 Imports 边：core→net→db
        let changed = vec![PathBuf::from("src/db.rs")];
        let changes = EntityChangeSet {
            changes: vec![crate::incremental::change::EntityChange {
                file: PathBuf::from("src/db.rs"),
                entity_name: "load".into(),
                kind: crate::incremental::change::EntityChangeKind::BodyChanged,
                old_range: Some((1, 5)),
                new_range: Some((1, 8)),
            }],
        };
        let affected = propagate_impact_semantic(&changed, &changes, &graph, 3);
        assert_eq!(
            affected,
            vec!["db".to_string()],
            "无 Calls 边时 body 变化只影响本模块"
        );
    }

    /// 语义传播：签名变化（接口级）→ 向依赖方传播
    #[test]
    fn test_semantic_signature_change_propagates() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let changes = EntityChangeSet {
            changes: vec![crate::incremental::change::EntityChange {
                file: PathBuf::from("src/db.rs"),
                entity_name: "load".into(),
                kind: crate::incremental::change::EntityChangeKind::SignatureChanged,
                old_range: Some((1, 5)),
                new_range: Some((1, 5)),
            }],
        };
        let affected = propagate_impact_semantic(&changed, &changes, &graph, 3);
        assert!(affected.contains(&"db".to_string()));
        assert!(affected.contains(&"net".to_string()));
        assert!(affected.contains(&"core".to_string()));
    }

    /// 语义传播：删除（接口级）→ 向依赖方传播
    #[test]
    fn test_semantic_removed_propagates() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let changes = EntityChangeSet {
            changes: vec![crate::incremental::change::EntityChange {
                file: PathBuf::from("src/db.rs"),
                entity_name: "load".into(),
                kind: crate::incremental::change::EntityChangeKind::Removed,
                old_range: Some((1, 5)),
                new_range: None,
            }],
        };
        let affected = propagate_impact_semantic(&changed, &changes, &graph, 3);
        assert!(affected.contains(&"net".to_string()), "删除应传播到导入方");
        assert!(affected.contains(&"core".to_string()));
    }

    /// 语义传播：无实体变化信息 → 回退保守双向传播（与 propagate_impact 一致）
    #[test]
    fn test_semantic_empty_changes_falls_back() {
        let graph = make_simple_graph();
        let changed = vec![PathBuf::from("src/db.rs")];
        let changes = EntityChangeSet::default();
        let affected = propagate_impact_semantic(&changed, &changes, &graph, 3);
        assert_eq!(affected.len(), 3, "空实体变化应回退双向传播");
    }

    /// T2 传播闭环：受影响模块名（module_path.join("::") 形式）反查文件路径
    #[test]
    fn test_module_files_resolves_affected_modules() {
        // 构造 File 节点图（反查按 File 节点匹配；make_simple_graph 的节点是 Module 类型不适用）
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        for (i, (path, segs)) in [
            ("src/net.rs", vec!["net"]),
            ("src/db.rs", vec!["db"]),
            ("src/core.rs", vec!["core"]),
        ]
        .into_iter()
        .enumerate()
        {
            g.add_node(CodeNode {
                id: NodeId::new(i),
                kind: NodeKind::File,
                name: path.into(),
                file_path: Some(path.into()),
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: segs.into_iter().map(|s| s.to_string()).collect(),
            });
        }
        let graph = KnowledgeGraph {
            graph: g,
            modules: vec![],
            features: Vec::new(),
        };

        let files = module_files(&["net".into(), "db".into()], &graph);
        assert_eq!(files.len(), 2, "应反查出 net.rs 与 db.rs");
        assert!(files.contains(&PathBuf::from("src/net.rs")));
        assert!(files.contains(&PathBuf::from("src/db.rs")));
        // 未知模块名 → 空
        assert!(module_files(&["not_exist".into()], &graph).is_empty());
        // 空输入 → 空
        assert!(module_files(&[], &graph).is_empty());
        // 去重：同一受影响名重复出现只反查一次
        let files2 = module_files(&["net".into(), "net".into()], &graph);
        assert_eq!(files2.len(), 1);
    }

    /// P2-16 回归锚：路径匹配必须按段精确/目录前缀，而非子串——
    /// 旧 contains 子串匹配会把 src/a.ts 的变更误判到 src/a.tsx 节点
    /// （a.ts 是 a.tsx 的子串）；目录前缀变更（src/）仍须命中其下文件
    /// （find_start_nodes 的 fp.starts_with(cfp + "/") 语义）。
    #[test]
    fn test_impact_exact_path_no_substring_false_positive() {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        let _ts = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Module,
            name: "a_ts".into(),
            file_path: Some("src/a.ts".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["a_ts".into()],
        });
        let _tsx = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Module,
            name: "a_tsx".into(),
            file_path: Some("src/a.tsx".into()),
            line_range: None,
            doc_comment: None,
            signature: None,
            visibility: None,
            module_path: vec!["a_tsx".into()],
        });
        let graph = KnowledgeGraph {
            graph: g,
            modules: vec![],
            features: Vec::new(),
        };

        // 仅变更 src/a.ts：不得命中 src/a.tsx 节点
        let changed = vec![PathBuf::from("src/a.ts")];
        let affected = propagate_impact(&changed, &graph, 3);
        assert_eq!(
            affected,
            vec!["a_ts".to_string()],
            "a.ts 变更只影响 a.ts 所在模块，不得波及 a.tsx"
        );

        // 目录前缀兼容：变更 src/ 命中 src/a.ts 节点（fp.starts_with("src/")）
        let changed_dir = vec![PathBuf::from("src/")];
        let affected_dir = propagate_impact(&changed_dir, &graph, 3);
        assert!(
            affected_dir.contains(&"a_ts".to_string()),
            "目录级变更应命中其下文件节点: {:?}",
            affected_dir
        );
        assert!(
            affected_dir.contains(&"a_tsx".to_string()),
            "src/ 下所有节点都应命中"
        );
    }

    /// P2 批量起点查询（find_start_nodes_per_file）：一次遍历按路径分组，
    /// 与逐文件 find_start_nodes 结果一致——精确匹配不误伤同前缀文件、
    /// 目录前缀命中其下全部节点、未匹配路径无条目。
    #[test]
    fn test_find_start_nodes_per_file_groups_consistently() {
        let mut g = StableDiGraph::<CodeNode, CodeEdge>::new();
        let mut mk = |id: usize, name: &str, path: &str| {
            g.add_node(CodeNode {
                id: NodeId::new(id),
                kind: NodeKind::File,
                name: name.into(),
                file_path: Some(path.into()),
                line_range: None,
                doc_comment: None,
                signature: None,
                visibility: None,
                module_path: vec![name.into()],
            })
        };
        let a = mk(0, "a_ts", "src/a.ts");
        let tsx = mk(1, "a_tsx", "src/a.tsx");
        let b = mk(2, "b", "src/sub/b.rs");
        let graph = KnowledgeGraph {
            graph: g,
            modules: vec![],
            features: Vec::new(),
        };

        let paths: Vec<String> = vec!["src/a.ts".into(), "src/sub/".into(), "src/".into()];
        let batch = find_start_nodes_per_file(&paths, &graph);

        // 精确匹配：src/a.ts 只命中 a_ts 节点（不误伤 a.tsx）
        let expect_a = batch.get("src/a.ts").cloned().unwrap_or_default();
        assert!(expect_a.contains(&a));
        assert!(!expect_a.contains(&tsx), "a.ts 不得命中 a.tsx 节点");

        // 目录前缀：src/sub/ 命中 b 节点；src/ 命中 a.ts 与 a.tsx
        assert!(batch.get("src/sub/").is_some_and(|v| v.contains(&b)));
        assert!(
            batch
                .get("src/")
                .is_some_and(|v| v.contains(&a) && v.contains(&tsx))
        );

        // 与逐文件查询结果一致（批量 = 逐文件的并集语义）
        let per_file: Vec<NodeId> = paths
            .iter()
            .flat_map(|p| find_start_nodes(std::slice::from_ref(p), &graph))
            .collect();
        let batch_all: Vec<NodeId> = batch.values().flat_map(|v| v.iter().copied()).collect();
        assert_eq!(batch_all.len(), per_file.len());

        // 未匹配路径不在表中
        assert!(!batch.contains_key("not_exist.rs"));
        // 空输入 → 空表
        assert!(find_start_nodes_per_file(&[], &graph).is_empty());
    }

    // ==================== 删除文件的「反向引用失效」 ====================

    /// 构造快照：B 模块页正文含对 src/a/x.rs 的 path:line 引用，
    /// C 模块卡片 related_files 含 src/c/z.rs（未删），D 模块卡片
    /// related_files 含 src/a/x.rs。
    fn make_snapshot_with_references() -> crate::output::ExportSnapshot {
        fn doc(title: &str, module: &[&str], content: &str) -> crate::model::WikiDocument {
            crate::model::WikiDocument {
                title: title.into(),
                kind: crate::model::DocumentKind::WikiPage,
                content: content.into(),
                language: "zh".into(),
                module_path: module.iter().map(|s| s.to_string()).collect(),
                references: vec![],
                parent: String::new(),
                last_updated: String::new(),
                based_on_commit: None,
                fingerprint: None,
            }
        }
        fn card(module: &str, related: &[&str]) -> crate::model::KnowledgeCard {
            crate::model::KnowledgeCard {
                module_name: module.into(),
                module_type: "module".into(),
                summary: String::new(),
                key_entities: vec![],
                dependencies: vec![],
                dependents: vec![],
                design_patterns: vec![],
                todo_notes: vec![],
                related_files: related.iter().map(|s| s.to_string()).collect(),
                coding_spec: None,
                tech_stack: vec![],
                architecture: None,
                design_rationale: None,
                pending_manual_edits: vec![],
                features: Vec::new(),
            }
        }
        crate::output::ExportSnapshot {
            version: 1,
            documents: vec![
                doc(
                    "B 模块",
                    &["src", "b"],
                    "## 概述\n\n见 src/a/x.rs:1 的实现。\n",
                ),
                doc("C 模块", &["src", "c"], "## 概述\n\n无引用。\n"),
            ],
            cards: vec![
                card("src::c", &["src/c/y.rs"]),
                card("src::d", &["src/a/x.rs", "src/d/w.rs"]),
            ],
            modules: vec![],
        }
    }

    /// 页面正文 path:line 引用命中被删文件 → 对应模块失效；未引用模块不失效
    #[test]
    fn test_reverse_reference_citation_hit() {
        let deleted: std::collections::HashSet<std::path::PathBuf> =
            [std::path::PathBuf::from("src/a/x.rs")]
                .into_iter()
                .collect();
        let affected =
            reverse_reference_affected_modules(&deleted, &make_snapshot_with_references());
        assert!(
            affected.contains(&"src::b".to_string()),
            "B 页正文引用 x.rs 应失效: {:?}",
            affected
        );
        assert!(
            !affected.contains(&"src::c".to_string()),
            "C 页未引用被删文件不得失效: {:?}",
            affected
        );
    }

    /// 卡片 related_files 命中被删文件 → 该模块失效（跨模块引用场景）
    #[test]
    fn test_reverse_reference_card_related_files_hit() {
        let deleted: std::collections::HashSet<std::path::PathBuf> =
            [std::path::PathBuf::from("src/a/x.rs")]
                .into_iter()
                .collect();
        let affected =
            reverse_reference_affected_modules(&deleted, &make_snapshot_with_references());
        assert!(
            affected.contains(&"src::d".to_string()),
            "D 卡 related_files 含 x.rs 应失效: {:?}",
            affected
        );
        assert_eq!(affected, vec!["src::b".to_string(), "src::d".to_string()]);
    }

    /// 无被删文件 / 无引用命中 → 空结果（不无差别全量重生成）
    #[test]
    fn test_reverse_reference_no_hit_empty() {
        let snapshot = make_snapshot_with_references();
        // 未被任何页面引用的删除文件 → 无失效
        let unrelated: std::collections::HashSet<std::path::PathBuf> =
            [std::path::PathBuf::from("src/ghost.rs")]
                .into_iter()
                .collect();
        assert!(
            reverse_reference_affected_modules(&unrelated, &snapshot).is_empty(),
            "未被引用的删除文件不得失效任何模块"
        );
        // 无删除文件 → 空
        assert!(
            reverse_reference_affected_modules(&Default::default(), &snapshot).is_empty(),
            "无删除文件时应返回空"
        );
    }

    /// Windows 路径分隔符归一化：快照引用正斜杠、被删路径反斜杠时仍命中
    #[test]
    fn test_reverse_reference_normalizes_separators() {
        let deleted: std::collections::HashSet<std::path::PathBuf> =
            [std::path::PathBuf::from("src\\a\\x.rs")]
                .into_iter()
                .collect();
        let affected =
            reverse_reference_affected_modules(&deleted, &make_snapshot_with_references());
        assert!(
            affected.contains(&"src::b".to_string()),
            "反斜杠被删路径应命中正斜杠引用: {:?}",
            affected
        );
    }
}
