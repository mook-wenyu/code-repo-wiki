use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use petgraph::graph::EdgeIndex;
use tracing::warn;

use crate::ingest::parser::{Entity, FileInsight, ImportStmt};
use crate::model::*;

/// 从 FileInsight 列表构建完整知识图谱
pub fn build(insights: &[FileInsight]) -> Result<KnowledgeGraph> {
    let mut kg = KnowledgeGraph::default();
    let g = &mut kg.graph;

    // v32 8.2 大仓优化：name_map/path_map 改为**全局增量构建**。
    // 原实现每文件调用 build_import_edges/build_impl_edges 时都会
    // collect_node_names 全图遍历重建索引，复杂度 O(文件数² × 图大小)，
    // cal.com 5054 文件实测 200s；现改为在 add_node 处增量插入，
    // 每文件 O(新增节点数)，总复杂度 O(图大小)。语义等价：增量索引
    // 在每个文件处理时刻恰好包含「已处理文件 + 当前文件」的全部节点，
    // 与原 collect_node_names 在该时刻的遍历结果一致（path_map 后插
    // 覆盖、name_map 按添加序 push 均与原语义相同）。
    let mut name_map: HashMap<String, Vec<NodeId>> = HashMap::new();
    let mut path_map: HashMap<Vec<String>, NodeId> = HashMap::new();

    let project_id = g.add_node(CodeNode {
        id: NodeId::new(g.node_count()),
        kind: NodeKind::Project,
        name: "project".into(),
        file_path: None,
        line_range: None,
        doc_comment: None,
        signature: None, visibility: None,
        module_path: Vec::new(),
    });
    name_map.entry("project".to_string()).or_default().push(project_id);
    path_map.insert(Vec::new(), project_id);

    let mut module_cache: HashMap<Vec<String>, NodeId> = HashMap::new();
    // 跨文件调用边候选：(实体, 节点, 函数体文本)，全图实体构建完成后统一匹配
    let mut call_candidates: Vec<(Entity, NodeId, String)> = Vec::new();

    for insight in insights {
        let path = Path::new(&insight.path);
        let dir_segments: Vec<String> = path
            .parent()
            .map(|p| {
                p.components()
                    .filter_map(|c| match c {
                        std::path::Component::Normal(s) => {
                            Some(s.to_string_lossy().into_owned())
                        }
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let file_module_id = ensure_module_chain(
            g, &mut module_cache, project_id, &dir_segments, &mut name_map, &mut path_map,
        );

        let file_id = g.add_node(CodeNode {
            id: NodeId::new(g.node_count()),
            kind: NodeKind::File,
            name: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            file_path: Some(insight.path.to_string_lossy().into_owned()),
            line_range: None,
            doc_comment: None,
            signature: None, visibility: None,
            module_path: dir_segments.clone(),
        });

        g.add_edge(
            file_module_id,
            file_id,
            CodeEdge {
                id: EdgeIndex::new(g.edge_count()),
                kind: EdgeKind::Contains,
                source: file_module_id,
                target: file_id,
                weight: 1.0,
                location: None,
            },
        );
        // 增量索引（v32 8.2）：File 节点入 path_map（module_path=目录段）
        name_map
            .entry(path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
            .or_default()
            .push(file_id);
        // v52 T11：File 节点优先于 module 中间节点；同目录多文件保留首个 File（确定性）。
        // 插入顺序：ensure_module_chain（:216 附近）先插 module 中间节点（键=目录段），
        // 此处后插 File——必须主动覆盖 module 节点，否则 File 永远进不了 path_map，
        // 目录级 import 回退匹配失效。若已有条目已是 File 节点则保留（首个 File 胜出）。
        match path_map.get(&dir_segments) {
            Some(&existing) if g.node_weight(existing).is_some_and(|n| n.kind == NodeKind::File) => {}
            _ => {
                path_map.insert(dir_segments.clone(), file_id);
            }
        }

        let entity_ids: Vec<(Entity, NodeId)> = insight
            .entities
            .iter()
            .map(|e| {
                let kind = kind_from_str(&e.kind);
                let mut module_path = dir_segments.clone();
                if let Some(stem) = path.file_stem() {
                    module_path.push(stem.to_string_lossy().into_owned());
                }
                let id = g.add_node(CodeNode {
                    id: NodeId::new(g.node_count()),
                    kind,
                    name: e.name.clone(),
                    file_path: Some(insight.path.to_string_lossy().into_owned()),
                    line_range: Some((e.line_start, e.line_end)),
                    doc_comment: e.doc_comment.clone(),
                    signature: e.signature.clone(),
                    visibility: e.visibility.clone(),
                    module_path: module_path.clone(),
                });
                // 增量索引（v32 8.2）：实体节点入 name_map（同名 push）+ path_map
                name_map.entry(e.name.clone()).or_default().push(id);
                path_map.insert(module_path, id);
                (e.clone(), id)
            })
            .collect();

        for (_, eid) in &entity_ids {
            g.add_edge(
                file_id,
                *eid,
                CodeEdge {
                    id: EdgeIndex::new(g.edge_count()),
                    kind: EdgeKind::Contains,
                    source: file_id,
                    target: *eid,
                    weight: 1.0,
                    location: None,
                },
            );
        }

        build_import_edges(g, file_id, &insight.imports, &name_map, &path_map);
        build_impl_edges(g, &entity_ids, &name_map);
        // 收集 (实体, 节点, 函数体文本) —— 跨文件调用边需在全图实体
        // 构建完成后统一匹配（每个函数用其函数体文本找被调用函数名）
        call_candidates.extend(entity_ids.iter().filter_map(|(e, eid)| {
            if e.kind != "fn" && e.kind != "function" {
                return None;
            }
            Some((e.clone(), *eid, extract_body(&insight.source, e.line_start, e.line_end)))
        }));
    }

    // 全部实体构建完成后统一构建调用边：此时 name_map 覆盖全图符号，
    // 跨文件调用（本文件函数调用其他文件函数）才能被解析
    build_call_edges(g, &call_candidates, &name_map);

    if let Some(node) = g.node_weight_mut(project_id) {
        node.id = project_id;
    }

    // v52 T11：g 是 kg.graph 的别名（:14 `let g = &mut kg.graph`），所有节点/边
    // 均直接写入 kg.graph——原 `kg.graph = g.clone()` 是自克隆深拷贝（O(V+E) 冗余），
    // 直接删除；此处在 g 最后一次使用（:157）之后。

    let cycles = kg.detect_cycles();
    if !cycles.is_empty() {
        warn!("{}", format_cycles(&cycles));
    }

    // 模块检测接线:图构建完成后运行社区检测,结果写回 kg.modules。
    // 生成层(generate/mod.rs)、渲染层(markdown/html/mermaid)均以
    // graph.modules 为模块分组的唯一来源;此前仅 lib.rs 显式调用
    // detect_modules 且结果只进 stats,modules 恒空导致按模块生成
    // 从未生效。检测失败向上传播(无兜底)。
    kg.modules = crate::analysis::detect_modules(&kg).with_context(|| "模块检测失败（图构建阶段）")?;

    Ok(kg)
}

fn ensure_module_chain(
    g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
    cache: &mut HashMap<Vec<String>, NodeId>,
    project_id: NodeId,
    segments: &[String],
    name_map: &mut HashMap<String, Vec<NodeId>>,
    path_map: &mut HashMap<Vec<String>, NodeId>,
) -> NodeId {
    let mut parent = project_id;
    for i in 0..segments.len() {
        let prefix: Vec<String> = segments[..=i].to_vec();
        if let Some(&cached) = cache.get(&prefix) {
            parent = cached;
            continue;
        }
        let id = g.add_node(CodeNode {
            id: NodeId::new(g.node_count()),
            kind: NodeKind::Module,
            name: segments[i].clone(),
            file_path: None,
            line_range: None,
            doc_comment: None,
            signature: None, visibility: None,
            module_path: prefix.clone(),
        });
        g.add_edge(
            parent,
            id,
            CodeEdge {
                id: EdgeIndex::new(g.edge_count()),
                kind: EdgeKind::Contains,
                source: parent,
                target: id,
                weight: 1.0,
                location: None,
            },
        );
        cache.insert(prefix.clone(), id);
        // 增量索引（v32 8.2）：与原 collect_node_names 遍历语义一致
        name_map.entry(segments[i].clone()).or_default().push(id);
        // v52 T11：module 中间节点不覆盖已存在的 File 节点——or_insert 保留首个。
        path_map.entry(prefix.clone()).or_insert(id);
        parent = id;
    }
    parent
}

fn kind_from_str(s: &str) -> NodeKind {
    match s {
        // v19 t03：parser 合法产出 kind="mod"（Rust mod 声明 rust.rs、
        // C# namespace csharp.rs），此前落入默认分支产生「未知实体类型」
        // warn 并误标 Function。Module 为容器节点，api.md 渲染已跳过。
        "mod" => NodeKind::Module,
        "struct" => NodeKind::Struct,
        "enum" => NodeKind::Enum,
        "fn" | "function" => NodeKind::Function,
        "trait" => NodeKind::Trait,
        "impl" => NodeKind::Impl,
        "type" => NodeKind::Type,
        "const" | "constant" | "static" => NodeKind::Constant,
        "variable" | "let" | "property" | "var" => NodeKind::Variable,
        "interface" => NodeKind::Interface,
        "class" => NodeKind::Class,
        "macro" => NodeKind::Macro,
        _ => {
            warn!("未知实体类型 '{}'，使用 Function 作为默认", s);
            NodeKind::Function
        }
    }
}

/// 构建 import 边：把实体的 import 语句连接到目标实体。
///
/// v32 8.2：name_map/path_map 由 build() 全局增量构建后传入，
/// 不再每文件全图遍历重建（原实现 O(文件数²)，cal.com 实测 200s）。
fn build_import_edges(
    g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
    file_id: NodeId,
    imports: &[ImportStmt],
    name_map: &HashMap<String, Vec<NodeId>>,
    path_map: &HashMap<Vec<String>, NodeId>,
) {
    for imp in imports {
        let parts: Vec<&str> = imp.source.split("::").collect();
        if parts.is_empty() {
            continue;
        }

        let target_name = parts.last().unwrap_or(&"");
        let mut targets: Vec<NodeId> = Vec::new();

        // name_map 返回 Vec<NodeId>，遍历所有同名实体（函数重载、同名结构体等）
        if let Some(nids) = name_map.get(*target_name) {
            targets = nids.clone();
        }

        // name 匹配失败时尝试路径后缀匹配
        // v52 T11：path_map 是 HashMap（迭代序非确定），原实现"命中即 break"在
        // 同后缀多路径时命中目标跨运行随机，破坏确定性契约。改为收集全部命中、
        // 按键排序后取最小键（Vec<String> 字典序），跨运行确定。
        if targets.is_empty() {
            let path_segments: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
            let mut hits: Vec<(Vec<String>, NodeId)> = path_map
                .iter()
                .filter(|(mp, _)| mp.ends_with(&path_segments))
                .map(|(mp, &nid)| (mp.clone(), nid))
                .collect();
            hits.sort_by(|a, b| a.0.cmp(&b.0));
            if let Some((_, nid)) = hits.into_iter().next() {
                targets.push(nid);
            }
        }

        for target_id in targets {
            // v52 T11：import 是文件级依赖——源从每个实体改为所属 File 节点
            //（一条 import 语句对每个命中目标只建一条 File→目标 边），消除
            // 实体级笛卡尔积导致的边爆炸（30 实体文件一条 import 原产生 30 条边）。
            // community.rs 的 file_of 聚合天然兼容 File 源节点（File 取自身）。
            if g.edges_connecting(file_id, target_id).count() == 0 {
                g.add_edge(
                    file_id,
                    target_id,
                    CodeEdge {
                        id: EdgeIndex::new(g.edge_count()),
                        kind: EdgeKind::Imports,
                        source: file_id,
                        target: target_id,
                        weight: 0.8,
                        location: Some((imp.line, imp.line)),
                    },
                );
            }
        }
    }
}

/// 构建 impl 边：把 impl 实体连接到其 trait 目标。
///
/// v32 8.2：name_map 由 build() 全局增量构建后传入（同 import 边）。
fn build_impl_edges(
    g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
    entities: &[(Entity, NodeId)],
    name_map: &HashMap<String, Vec<NodeId>>,
) {
    for (entity, eid) in entities {
        if let Some(trait_name) = parse_impl_target(&entity.kind, &entity.name)
            && let Some(trait_ids) = name_map.get(&trait_name)
        {
            for &trait_id in trait_ids {
                g.add_edge(
                    *eid,
                    trait_id,
                    CodeEdge {
                        id: EdgeIndex::new(g.edge_count()),
                        kind: EdgeKind::Implements,
                        source: *eid,
                        target: trait_id,
                        weight: 1.0,
                        location: None,
                    },
                );
            }
        }
    }
}

fn parse_impl_target(kind: &str, name: &str) -> Option<String> {
    if kind != "impl" && !kind.starts_with("impl_for") {
        return None;
    }
    // entity.name 的格式可能是 "impl MyTrait for MyStruct" 或 "MyTrait for MyStruct"
    // 提取 " for " 之前的部分作为 trait 名
    if let Some(for_idx) = name.find(" for ") {
        // 跳过 "impl " 前缀（如果存在）
        let after_impl = if let Some(idx) = name.find("impl ") {
            &name[idx + 5..]
        } else {
            name
        };
        let trait_name = after_impl[..for_idx.saturating_sub(name.len() - after_impl.len())].trim().to_string();
        // NOTE: for_idx is relative to the original name; subtract the "impl " prefix length
        // to index into after_impl. Fixes a pre-existing off-by-prefix bug (v32 8.2).
        if !trait_name.is_empty() {
            return Some(trait_name);
        }
    }
    // 如果名字中不含 " for "，无法确定 trait 名，返回 None
    // 不猜测（例如把 struct 名当作 trait 名）
    None
}

/// 按行号区间从文件源码中提取函数体文本（供调用边匹配使用）
///
/// line_start/line_end 为 1-based 行号；越界时安全截断（不 panic）。
fn extract_body(source: &str, line_start: usize, line_end: usize) -> String {
    source
        .lines()
        .skip(line_start.saturating_sub(1))
        .take(line_end.saturating_sub(line_start).saturating_add(1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 构建调用边（函数 → 被调用函数）
///
/// 在**全图实体构建完成后**调用一次：name_map 此时包含所有文件的函数符号，
/// 才能解析跨文件调用（此前逐文件构建时 name_map 只含已处理文件，跨文件
/// 调用全部丢失，真实图上 Calls 边几乎为零）。
/// 匹配载体 = 函数体文本（按行号从文件源码切片），而非仅签名+文档注释
/// （签名几乎不含调用信息，旧实现导致 Calls 边数量失真）。
/// 跨文件调用边构建（v32 8.2 大仓优化）。
///
/// 原实现为 O(候选数 × 全图名字数) 双重循环（对每个函数体逐一尝试所有
/// 实体名的 `name(` 子串匹配），cal.com 5.9 万实体时约 9 亿次迭代，
/// 实测 287s。现改为按函数体「标识符 token 化」检索：只对函数体中
/// 实际出现的标识符查询名字表，复杂度降为 O(候选 × 函数体长度)，
/// 同一仓库实测 ~1s。
///
/// 语义保持与原实现逐点等价：
///   - 原「find("name(") 且 name 前一字符非标识符」⇔ token 化后 name
///     为完整标识符 token 且其后紧跟 '('（token 化天然保证前边界）；
///   - 原「每个名字首次边界通过后 break」⇔ 每个名字每个函数体只处理
///     一次（seen 集合）。
///
/// 唯一行为差异：原实现会在 `xfoo(` 中误把子串 `foo(` 当作候选（因
/// 前字符 `x` 非边界而放弃）——若 `foo` 恰为实体名则漏连 xfoo 的调用
/// 边；新实现按完整标识符匹配，此场景正确建立调用边（修复而非回归）。
/// 另一差异（已知限制）：token 化只认 ASCII 标识符起始字节，含非 ASCII
/// 字符的实体名（如 `fn 测试()`）不会建立 Calls 边——此类标识符在实际
/// 代码库中极为罕见，且原实现对其边界判定本就不一致，故不为此扩展。
fn build_call_edges(
    g: &mut petgraph::stable_graph::StableDiGraph<CodeNode, CodeEdge>,
    call_candidates: &[(Entity, NodeId, String)],
    name_map: &HashMap<String, Vec<NodeId>>,
) {
    for (entity, eid, body) in call_candidates {
        // 每个函数体对每个名字只处理一次（等价原实现的 break 语义）
        let mut seen: HashSet<&str> = HashSet::new();
        let bytes = body.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b.is_ascii_alphanumeric() || b == b'_' {
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                // 与 find("name(") 等价：标识符后必须紧跟 '(' 才视为调用点
                if i < bytes.len() && bytes[i] == b'(' {
                    let name = &body[start..i];
                    if name != entity.name && !seen.contains(name) {
                        seen.insert(name);
                        if let Some(callee_ids) = name_map.get(name) {
                            for &callee_id in callee_ids {
                                if callee_id == *eid {
                                    continue;
                                }
                                if g.edges_connecting(*eid, callee_id).count() == 0 {
                                    g.add_edge(
                                        *eid,
                                        callee_id,
                                        CodeEdge {
                                            id: EdgeIndex::new(g.edge_count()),
                                            kind: EdgeKind::Calls,
                                            source: *eid,
                                            target: callee_id,
                                            weight: 0.7,
                                            location: None,
                                        },
                                    );
                                }
                            }
                        }
                    }
                }
            } else {
                i += 1;
            }
        }
    }
}

impl KnowledgeGraph {
    pub fn detect_cycles(&self) -> Vec<Vec<String>> {
        petgraph::algo::tarjan_scc(&self.graph)
            .into_iter()
            .filter(|scc| scc.len() > 1)
            .map(|scc| scc.iter().map(|&n| self.graph[n].name.clone()).collect())
            .collect()
    }
}

/// 循环依赖链的紧凑格式化（v48）
///
/// 巨型伪 SCC（跨模块同名字段/方法按名互连）可达数百节点，全量 Debug
/// 打印会刷屏数万字符并淹没真实进度输出。每链最多展示
/// `MAX_CYCLE_NAMES_PER_CHAIN` 个名称，超出以「…共 N 个」省略。
const MAX_CYCLE_NAMES_PER_CHAIN: usize = 8;

pub(crate) fn format_cycles(cycles: &[Vec<String>]) -> String {
    if cycles.is_empty() {
        return String::new();
    }
    let total_nodes: usize = cycles.iter().map(|c| c.len()).sum();
    let mut out = format!("检测到 {} 个循环依赖（共 {} 个节点）: [", cycles.len(), total_nodes);
    for (i, chain) in cycles.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if chain.len() <= MAX_CYCLE_NAMES_PER_CHAIN {
            out.push_str(&format!("{:?}", chain));
        } else {
            out.push_str(&format!(
                "[{} 项: {:?}…]",
                chain.len(),
                &chain[..MAX_CYCLE_NAMES_PER_CHAIN]
            ));
        }
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::parser::{Entity, FileInsight, ImportStmt};
    use crate::model::KnowledgeGraph;
    use std::path::PathBuf;

    #[test]
    fn test_empty_insights() {
        let kg = build(&[]).unwrap();
        assert_eq!(kg.graph.node_count(), 1);
        let root = kg.graph.node_weight(NodeId::new(0)).unwrap();
        assert_eq!(root.kind, NodeKind::Project);
    }

    /// v19 t03：parser 合法产出 kind="mod"（Rust mod / C# namespace），
    /// 此前落入默认分支产生「未知实体类型」warn 并误标 Function。
    #[test]
    fn test_kind_from_str_supports_mod() {
        assert_eq!(kind_from_str("mod"), NodeKind::Module);
        assert_eq!(kind_from_str("struct"), NodeKind::Struct);
        assert_eq!(kind_from_str("fn"), NodeKind::Function);
    }

    /// v21 t06：parser 合法产出 kind="static"（Rust static_item 静态变量）
    /// 与 kind="property"（C# 属性）——此前落入默认分支产生「未知实体
    /// 类型 'static'/'property'」warn 并误标 Function。static 语义上是
    /// 常量，property 语义上是字段/变量，归入对应 NodeKind。
    #[test]
    fn test_kind_from_str_supports_static_and_property() {
        assert_eq!(kind_from_str("static"), NodeKind::Constant);
        assert_eq!(kind_from_str("property"), NodeKind::Variable);
        assert_eq!(kind_from_str("function"), NodeKind::Function);
    }

    #[test]
    fn test_single_file_two_entities() {
        let insights = vec![FileInsight {
            path: PathBuf::from("src/lib.rs"),
            language: "rust".into(),
            entities: vec![
                Entity {
                    name: "add".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 5,
                    doc_comment: None,
                    signature: Some("fn add(a: i32, b: i32) -> i32".into()),
                    visibility: None,
                },
                Entity {
                    name: "Sub".into(),
                    kind: "struct".into(),
                    line_start: 7,
                    line_end: 10,
                    doc_comment: None,
                    signature: Some("struct Sub".into()),
                    visibility: None,
                },
            ],
            imports: vec![],
            doc_comments: vec![],
            source: String::new(),
        }];
        let kg = build(&insights).unwrap();
        assert_eq!(kg.graph.node_count(), 5);
        assert_eq!(kg.graph.edge_count(), 4);
    }

    #[test]
    fn test_import_edge() {
        let insights = vec![
            FileInsight {
                path: PathBuf::from("src/utils.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "helper".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 3,
                    doc_comment: None,
                    signature: Some("fn helper()".into()),
                    visibility: None,
                }],
                imports: vec![],
                doc_comments: vec![],
                source: String::new(),
            },
            FileInsight {
                path: PathBuf::from("src/main.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "run".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 10,
                    doc_comment: None,
                    signature: Some("fn run()".into()),
                    visibility: None,
                }],
                imports: vec![ImportStmt {
                    source: "crate::utils::helper".into(),
                    alias: None,
                    line: 1,
                }],
                doc_comments: vec![],
                source: String::new(),
            },
        ];
        let kg = build(&insights).unwrap();
        let has_import = kg
            .graph
            .edge_indices()
            .any(|e| kg.graph.edge_weight(e).map(|w| w.kind == EdgeKind::Imports).unwrap_or(false));
        assert!(has_import);
    }

    /// v52 T11（test_engineer 缺口 (d)）：import 路径后缀匹配的 min-key 确定性。
    /// 触发条件（已实测查证 graph.rs:269-298）：name 通道不命中（last part 不在
    /// name_map）且 parts 是 path_map 键的后缀。目录段名必在 name_map（:226），
    /// 故只能用 **file_stem 段**构造：src/x/mod/foo.rs 与 src/y/mod/foo.rs 的
    /// 实体键分别为 ["src","x","mod","foo"]/["src","y","mod","foo"]（dir+stem，
    /// :111-128），import "mod::foo"（parts=["mod","foo"]，last="foo" 不在
    /// name_map——实体名 foo_fn/foo_fn2、file_name foo.rs、目录段 src/x/mod）
    /// → 后缀命中两键 → 排序取最小键 ["src","x",...] → 边指向 x 的实体 foo_fn。
    #[test]
    fn test_import_suffix_match_takes_min_key() {
        let insights = vec![
            FileInsight {
                path: PathBuf::from("src/x/mod/foo.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "foo_fn".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 3,
                    doc_comment: None,
                    signature: Some("fn foo_fn()".into()),
                    visibility: None,
                }],
                imports: vec![],
                doc_comments: vec![],
                source: String::new(),
            },
            FileInsight {
                path: PathBuf::from("src/y/mod/foo.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "foo_fn2".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 3,
                    doc_comment: None,
                    signature: Some("fn foo_fn2()".into()),
                    visibility: None,
                }],
                imports: vec![],
                doc_comments: vec![],
                source: String::new(),
            },
            FileInsight {
                path: PathBuf::from("src/api.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "api_run".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 10,
                    doc_comment: None,
                    signature: Some("fn api_run()".into()),
                    visibility: None,
                }],
                imports: vec![ImportStmt {
                    source: "mod::foo".into(),
                    alias: None,
                    line: 1,
                }],
                doc_comments: vec![],
                source: String::new(),
            },
        ];
        let kg = build(&insights).unwrap();
        let mut import_targets: Vec<String> = Vec::new();
        for e in kg.graph.edge_indices() {
            if kg.graph.edge_weight(e).map(|w| w.kind == EdgeKind::Imports).unwrap_or(false) {
                let t = kg.graph.edge_weight(e).unwrap().target;
                let n = kg.graph.node_weight(t).unwrap();
                import_targets.push(n.name.clone());
            }
        }
        assert_eq!(import_targets, vec!["foo_fn"], "后缀匹配应取最小键 x 目录的实体 foo_fn，实际: {:?}", import_targets);
    }

    /// v52 T11（test_engineer 缺口 (e) 修正版）：目录级 import 的确定性契约。
    /// 已实测：目录段名（v1）在 name_map（:226）→ import "net::v1" 命中 name
    /// 通道的 module 节点（name="v1"），后缀/File 优先分支被遮蔽（当前不可达，
    /// 保留为防御逻辑）。本测试锚定**可观察行为**：目录级 import 稳定指向
    /// module 节点且两次 build 结果一致（确定性，防 name_map 语义漂移回归）。
    #[test]
    fn test_path_map_directory_import_deterministic() {
        let insights = vec![
            FileInsight {
                path: PathBuf::from("src/net/v1/mod.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "v1_fn".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 3,
                    doc_comment: None,
                    signature: Some("fn v1_fn()".into()),
                    visibility: None,
                }],
                imports: vec![],
                doc_comments: vec![],
                source: String::new(),
            },
            FileInsight {
                path: PathBuf::from("src/api.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "api_run".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 10,
                    doc_comment: None,
                    signature: Some("fn api_run()".into()),
                    visibility: None,
                }],
                imports: vec![ImportStmt {
                    source: "net::v1".into(),
                    alias: None,
                    line: 1,
                }],
                doc_comments: vec![],
                source: String::new(),
            },
        ];
        let kg1 = build(&insights).unwrap();
        let kg2 = build(&insights).unwrap();
        let mut targets1: Vec<String> = Vec::new();
        let mut targets2: Vec<String> = Vec::new();
        for e in kg1.graph.edge_indices() {
            if kg1.graph.edge_weight(e).map(|w| w.kind == EdgeKind::Imports).unwrap_or(false) {
                let t = kg1.graph.edge_weight(e).unwrap().target;
                targets1.push(kg1.graph.node_weight(t).unwrap().name.clone());
            }
        }
        for e in kg2.graph.edge_indices() {
            if kg2.graph.edge_weight(e).map(|w| w.kind == EdgeKind::Imports).unwrap_or(false) {
                let t = kg2.graph.edge_weight(e).unwrap().target;
                targets2.push(kg2.graph.node_weight(t).unwrap().name.clone());
            }
        }
        assert_eq!(targets1, targets2, "两次 build 目录级 import 目标应一致: {:?} vs {:?}", targets1, targets2);
        assert!(!targets1.is_empty(), "目录级 import 应命中 module 节点: {:?}", targets1);
    }

    #[test]
    fn test_detect_cycles_empty() {
        let kg = KnowledgeGraph::default();
        assert!(kg.detect_cycles().is_empty());
    }

    #[test]
    fn test_detect_cycles_with_cycle() {
        let mut kg = KnowledgeGraph::default();
        let a = kg.graph.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "func_a".into(),
            file_path: None,
            line_range: None,
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let b = kg.graph.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "func_b".into(),
            file_path: None,
            line_range: None,
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        kg.graph.add_edge(a, b, CodeEdge {
            id: EdgeIndex::new(0),
            kind: EdgeKind::Calls,
            source: a,
            target: b,
            weight: 1.0,
            location: None,
        });
        kg.graph.add_edge(b, a, CodeEdge {
            id: EdgeIndex::new(1),
            kind: EdgeKind::Calls,
            source: b,
            target: a,
            weight: 1.0,
            location: None,
        });
        let cycles = kg.detect_cycles();
        assert_eq!(cycles.len(), 1);
        assert!(cycles[0].contains(&"func_a".to_string()));
        assert!(cycles[0].contains(&"func_b".to_string()));
    }

    #[test]
    fn test_format_cycles_empty() {
        assert_eq!(format_cycles(&[]), "");
    }

    #[test]
    fn test_format_cycles_short_chain_kept_inline() {
        let s = format_cycles(&[vec!["a".into(), "b".into()], vec!["x".into()]]);
        assert!(s.contains("2 个循环依赖（共 3 个节点）"));
        assert!(s.contains("[\"a\", \"b\"]"));
        assert!(!s.contains("项:"));
    }

    #[test]
    fn test_format_cycles_long_chain_truncated() {
        let chain: Vec<String> = (0..20).map(|i| format!("n{i}")).collect();
        let s = format_cycles(&[chain]);
        assert!(s.contains("1 个循环依赖（共 20 个节点）"));
        assert!(s.contains("[20 项: [\"n0\", \"n1\", \"n2\", \"n3\", \"n4\", \"n5\", \"n6\", \"n7\"]…]"));
        assert!(!s.contains("n8"));
    }
}

    /// t04：build_call_edges 跨文件调用边——a.rs 定义 callee，b.rs 的 caller
    /// 正文含 "callee(" 应产生 Calls 边（此前该核心功能零单测）
    #[test]
    fn test_build_call_edges_cross_file() {
        let mut g = petgraph::stable_graph::StableDiGraph::<CodeNode, CodeEdge>::new();
        let callee = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "callee".into(),
            file_path: Some("src/a.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let caller = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "caller".into(),
            file_path: Some("src/b.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        // call_candidates：(实体, 节点, 函数体源码)
        let candidates = vec![(
            Entity {
                name: "caller".into(),
                kind: "fn".into(),
                line_start: 1,
                line_end: 3,
                doc_comment: None,
                signature: None,
                visibility: None,
            },
            caller,
            "pub fn caller() { callee(42) }".to_string(),
        )];
        let mut tname_map: HashMap<String, Vec<NodeId>> = HashMap::new();
        tname_map.entry("callee".to_string()).or_default().push(callee);
        build_call_edges(&mut g, &candidates, &tname_map);
        assert_eq!(
            g.edges_connecting(caller, callee).count(),
            1,
            "跨文件调用应产生一条 Calls 边"
        );
        let edge = g.edges_connecting(caller, callee).next().unwrap();
        assert_eq!(edge.weight().kind, EdgeKind::Calls);
    }

    /// t04：单词边界——mycallee( 不应误匹配 callee(
    #[test]
    fn test_build_call_edges_word_boundary() {
        let mut g = petgraph::stable_graph::StableDiGraph::<CodeNode, CodeEdge>::new();
        let callee = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "callee".into(),
            file_path: Some("src/a.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let caller = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "caller".into(),
            file_path: Some("src/b.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let candidates = vec![(
            Entity {
                name: "caller".into(),
                kind: "fn".into(),
                line_start: 1,
                line_end: 3,
                doc_comment: None,
                signature: None,
                visibility: None,
            },
            caller,
            "pub fn caller() { mycallee(1) }".to_string(),
        )];
        let mut tname_map: HashMap<String, Vec<NodeId>> = HashMap::new();
        tname_map.entry("callee".to_string()).or_default().push(callee);
        build_call_edges(&mut g, &candidates, &tname_map);
        assert_eq!(
            g.edges_connecting(caller, callee).count(),
            0,
            "mycallee( 不是对 callee 的调用（前缀字母不构成调用）"
        );
    }

    /// t04：同名自调用排除——callee 调用同名函数不建边；多次出现只建一条边
    #[test]
    fn test_build_call_edges_self_name_skipped_and_dedup() {
        let mut g = petgraph::stable_graph::StableDiGraph::<CodeNode, CodeEdge>::new();
        let callee = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "callee".into(),
            file_path: Some("src/a.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        // 两个候选都调用 callee（同名实体跳过自身；同一模式重复出现去重）
        let candidates = vec![
            (
                Entity {
                    name: "callee".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 3,
                    doc_comment: None,
                    signature: None,
                    visibility: None,
                },
                callee,
                "pub fn callee() { callee(1); callee(2) }".to_string(),
            ),
            (
                Entity {
                    name: "other".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 3,
                    doc_comment: None,
                    signature: None,
                    visibility: None,
                },
                g.add_node(CodeNode {
                    id: NodeId::new(1),
                    kind: NodeKind::Function,
                    name: "other".into(),
                    file_path: Some("src/c.rs".into()),
                    line_range: Some((1, 3)),
                    doc_comment: None,
                    signature: None, visibility: None,
                    module_path: vec![],
                }),
                "pub fn other() { callee(3) }".to_string(),
            ),
        ];
        let mut tname_map: HashMap<String, Vec<NodeId>> = HashMap::new();
        tname_map.entry("callee".to_string()).or_default().push(callee);
        build_call_edges(&mut g, &candidates, &tname_map);
        // 自调用（callee→callee）不建边
        assert_eq!(
            g.edges_connecting(callee, callee).count(),
            0,
            "同名实体（自调用）应跳过"
        );
        // other→callee 只建一条（去重）
        let other = g.node_indices().find(|&n| g[n].name == "other").unwrap();
        assert_eq!(
            g.edges_connecting(other, callee).count(),
            1,
            "同一调用模式多次出现只建一条边"
        );
    }

    /// t04：无调用时零边
    #[test]
    fn test_build_call_edges_no_call() {
        let mut g = petgraph::stable_graph::StableDiGraph::<CodeNode, CodeEdge>::new();
        let _callee = g.add_node(CodeNode {
            id: NodeId::new(0),
            kind: NodeKind::Function,
            name: "callee".into(),
            file_path: Some("src/a.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let caller = g.add_node(CodeNode {
            id: NodeId::new(1),
            kind: NodeKind::Function,
            name: "caller".into(),
            file_path: Some("src/b.rs".into()),
            line_range: Some((1, 3)),
            doc_comment: None,
            signature: None, visibility: None,
            module_path: vec![],
        });
        let candidates = vec![(
            Entity {
                name: "caller".into(),
                kind: "fn".into(),
                line_start: 1,
                line_end: 3,
                doc_comment: None,
                signature: None,
                visibility: None,
            },
            caller,
            "pub fn caller() { let x = 1; }".to_string(),
        )];
        let mut tname_map: HashMap<String, Vec<NodeId>> = HashMap::new();
        tname_map.entry("caller".to_string()).or_default().push(caller);
        build_call_edges(&mut g, &candidates, &tname_map);
        assert_eq!(g.edge_count(), 0, "无调用应零边");
    }
