//! v32 8.2 图构建增量索引重构验证（commit 1e3b41c）
//!
//! 重构内容：name_map/path_map 由 build() 一次性增量构建，在每次
//! add_node 处插入；build_import_edges/build_impl_edges/build_call_edges
//! 消费共享索引；build_call_edges 改为函数体标识符 token 化 + seen 去重。
//!
//! 本文件从公开契约（analysis::graph::build）端到端验证：
//! 1. 重名实体边不串（同名目标各建一条边，不指向其他实体）
//! 2. 路径后缀回退（name_map 未命中时 path_map ends_with 匹配）
//! 3. 同文件内重复调用去重（seen 集合，同一函数体只建一条边）
//! 4. 无调用零边
//! 5. 索引与节点同步（跨文件调用、反向 import、前向 import 语义边界、
//!    节点/边总数精确——无漏插、无丢失、无提前插入）
//! 6. CRLF 源文本（Windows 行尾）不破坏调用解析
//! 7. 标识符与 '(' 相邻性边界（helper (1) 不算调用）
//!
//! 纯函数级测试：直接构造 FileInsight 内存对象，无文件 IO、无网络、无 mock。

use repo_wiki::analysis::graph::build;
use repo_wiki::ingest::parser::{Entity, FileInsight, ImportStmt};
use repo_wiki::model::{EdgeKind, KnowledgeGraph, NodeKind, NodeId};
use std::path::PathBuf;

// petgraph 的 EdgeRef trait 提供 EdgeReference::target()/source() 方法，
// 需显式导入（Rust 方法解析不自动包含 trait 方法）。
use petgraph::visit::EdgeRef;

fn fn_entity(name: &str, line_start: usize, line_end: usize) -> Entity {
    Entity {
        name: name.into(),
        kind: "fn".into(),
        line_start,
        line_end,
        doc_comment: None,
        signature: None,
        visibility: None,
    }
}

fn file_insight(path: &str, entities: Vec<Entity>, imports: &[ImportStmt], source: &str) -> FileInsight {
    FileInsight {
        path: PathBuf::from(path),
        language: "rust".into(),
        entities,
        imports: imports.to_vec(),
        doc_comments: vec![],
        source: source.into(),
    }
}

fn count_edges_of_kind(kg: &KnowledgeGraph, kind: EdgeKind) -> usize {
    kg.graph
        .edge_indices()
        .filter(|&e| kg.graph.edge_weight(e).is_some_and(|w| w.kind == kind))
        .count()
}

fn find_node(kg: &KnowledgeGraph, name: &str) -> NodeId {
    kg.graph
        .node_indices()
        .find(|&n| kg.graph[n].name == name)
        .unwrap_or_else(|| panic!("图中必须存在名为 {name} 的节点"))
}

/// 重名实体边不串（调用边）：两个文件定义同名 helper，caller 调用 helper()
/// 应对两个同名实体各建一条 Calls 边（name_map Vec 语义），且任何 Calls 边
/// 目标都必须是 helper 节点（不串到其他实体）。
#[test]
fn multi_name_collision_call_edges() {
    let insights = vec![
        file_insight("src/a.rs", vec![fn_entity("helper", 1, 1)], &[], "fn helper() {}"),
        file_insight("src/b.rs", vec![fn_entity("helper", 1, 1)], &[], "fn helper() {}"),
        file_insight(
            "src/c.rs",
            vec![fn_entity("caller", 1, 3)],
            &[],
            "fn caller() {\n    helper(9);\n}",
        ),
    ];
    let kg = build(&insights).unwrap();
    // 1 project + 1 模块(src) + 3 文件 + 3 实体 = 8
    assert_eq!(kg.graph.node_count(), 8, "索引/节点同步：节点总数必须精确");
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Calls), 2, "同名两个实体都应收到调用边");

    let helper_ids: Vec<NodeId> = kg
        .graph
        .node_indices()
        .filter(|&n| kg.graph[n].name == "helper")
        .collect();
    assert_eq!(helper_ids.len(), 2);
    let caller_id = find_node(&kg, "caller");
    for h in &helper_ids {
        assert_eq!(
            kg.graph.edges_connecting(caller_id, *h).count(),
            1,
            "caller 应对每个同名 helper 各有一条调用边"
        );
    }
    // 不串：所有 Calls 边目标必须是 helper 节点
    for e in kg
        .graph
        .edge_indices()
        .filter(|&e| kg.graph.edge_weight(e).is_some_and(|w| w.kind == EdgeKind::Calls))
    {
        let t = kg.graph.edge_endpoints(e).unwrap().1;
        assert_eq!(kg.graph[t].name, "helper", "调用边目标不得串到其他实体");
    }
}

/// 重名实体边不串（import 边）：同名目标 import 应对全部同名实体建边。
#[test]
fn multi_name_collision_import_edges() {
    let insights = vec![
        file_insight("src/a.rs", vec![fn_entity("helper", 1, 1)], &[], "fn helper() {}"),
        file_insight("src/b.rs", vec![fn_entity("helper", 1, 1)], &[], "fn helper() {}"),
        file_insight(
            "src/c.rs",
            vec![fn_entity("run", 1, 1)],
            &[ImportStmt { source: "crate::a::helper".into(), alias: None, line: 1 }],
            "fn run() {}",
        ),
    ];
    let kg = build(&insights).unwrap();
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Imports), 2, "重名目标应各建一条 import 边");
    for e in kg
        .graph
        .edge_indices()
        .filter(|&e| kg.graph.edge_weight(e).is_some_and(|w| w.kind == EdgeKind::Imports))
    {
        let t = kg.graph.edge_endpoints(e).unwrap().1;
        assert_eq!(kg.graph[t].name, "helper", "import 边目标不得串到其他实体");
    }
}

/// 路径后缀回退：name_map 无目标名（无实体叫 helper）时回退 path_map
/// ends_with 匹配，命中 crate/utils/helper.rs 的实体（模块路径
/// ["crate","utils","helper"]）；回退目标必须是实体节点而非文件/模块节点。
#[test]
fn import_path_suffix_fallback() {
    let insights = vec![
        file_insight(
            "crate/utils/helper.rs",
            vec![fn_entity("do_thing", 1, 1)],
            &[],
            "fn do_thing() {}",
        ),
        file_insight(
            "src/main.rs",
            vec![fn_entity("run", 1, 1)],
            &[ImportStmt { source: "crate::utils::helper".into(), alias: None, line: 1 }],
            "fn run() {}",
        ),
    ];
    let kg = build(&insights).unwrap();
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Imports), 1, "路径后缀回退应命中 helper.rs 的实体");
    let run_id = find_node(&kg, "run");
    let do_thing_id = find_node(&kg, "do_thing");
    let edge = kg
        .graph
        .edges_connecting(run_id, do_thing_id)
        .next()
        .expect("回退边必须存在");
    assert_eq!(edge.weight().kind, EdgeKind::Imports);
    assert_eq!(
        kg.graph[edge.target()].kind,
        NodeKind::Function,
        "回退目标必须是实体节点而非文件/模块节点"
    );
}

/// name 匹配优先于路径后缀回退：name_map 命中即建边，不因路径指向不存在
/// 的模块而丢失边（不误路由）。
#[test]
fn import_name_match_precedes_path_fallback() {
    let insights = vec![
        file_insight("src/a.rs", vec![fn_entity("helper", 1, 1)], &[], "fn helper() {}"),
        file_insight(
            "src/b.rs",
            vec![fn_entity("run", 1, 1)],
            &[ImportStmt { source: "crate::nonexistent::helper".into(), alias: None, line: 1 }],
            "fn run() {}",
        ),
    ];
    let kg = build(&insights).unwrap();
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Imports), 1, "name 命中即建边，不回退");
    let run_id = find_node(&kg, "run");
    let helper_id = find_node(&kg, "helper");
    assert_eq!(kg.graph.edges_connecting(run_id, helper_id).count(), 1);
}

/// 同文件内重复调用去重（seen 集合）：同一函数体三次调用同一 callee
/// 只建一条边；同文件内调用可解析。
#[test]
fn call_dedup_same_body_via_build() {
    let insights = vec![file_insight(
        "src/lib.rs",
        vec![fn_entity("helper", 1, 1), fn_entity("caller", 2, 6)],
        &[],
        "fn helper() {}\nfn caller() {\n    helper(1);\n    helper(2);\n    helper(3);\n}",
    )];
    let kg = build(&insights).unwrap();
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Calls), 1, "同一函数体内重复调用只建一条边");
}

/// 无调用零边（经 build() 端到端）。
#[test]
fn no_call_zero_edges_via_build() {
    let insights = vec![file_insight(
        "src/lib.rs",
        vec![fn_entity("solo", 1, 3)],
        &[],
        "fn solo() {\n    let x = 1;\n}",
    )];
    let kg = build(&insights).unwrap();
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Calls), 0, "无调用应零 Calls 边");
}

/// 索引无漏插：caller 文件先处理、callee 文件后处理，全图构建完成后统一
/// 匹配调用边时，后处理文件的符号必须已在 name_map 中（增量索引同步）。
#[test]
fn cross_file_call_index_sync() {
    let insights = vec![
        file_insight(
            "src/caller.rs",
            vec![fn_entity("caller", 1, 3)],
            &[],
            "fn caller() {\n    callee(9);\n}",
        ),
        file_insight("src/callee.rs", vec![fn_entity("callee", 1, 1)], &[], "fn callee() {}"),
    ];
    let kg = build(&insights).unwrap();
    assert_eq!(
        count_edges_of_kind(&kg, EdgeKind::Calls),
        1,
        "后处理文件的符号必须可被调用解析（索引无漏插）"
    );
    let caller = find_node(&kg, "caller");
    let callee = find_node(&kg, "callee");
    assert_eq!(kg.graph.edges_connecting(caller, callee).count(), 1);
}

/// 索引与节点精确同步（增量边界）：import 边在文件处理时刻构建，索引只含
/// 「已处理文件 + 当前文件」符号（与原 collect_node_names 语义一致）——
/// 前向引用不建边，反向引用建边。防止索引提前含入未处理文件（语义漂移）
/// 或漏插（边丢失）。
#[test]
fn import_resolves_only_processed_files() {
    let insights = vec![
        // 文件1 前向 import 文件2 的实体（处理时刻尚未入索引）→ 不建边
        file_insight(
            "src/f1.rs",
            vec![fn_entity("use_early", 1, 1)],
            &[ImportStmt { source: "later_helper".into(), alias: None, line: 1 }],
            "fn use_early() {}",
        ),
        // 文件2 定义 later_helper
        file_insight("src/f2.rs", vec![fn_entity("later_helper", 1, 1)], &[], "fn later_helper() {}"),
        // 文件3 反向 import 文件2 的实体 → 建边
        file_insight(
            "src/f3.rs",
            vec![fn_entity("use_late", 1, 1)],
            &[ImportStmt { source: "later_helper".into(), alias: None, line: 1 }],
            "fn use_late() {}",
        ),
    ];
    let kg = build(&insights).unwrap();
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Imports), 1, "只有反向（目标已处理）import 建边");
    let use_early = find_node(&kg, "use_early");
    let use_late = find_node(&kg, "use_late");
    let later = find_node(&kg, "later_helper");
    assert_eq!(kg.graph.edges_connecting(use_early, later).count(), 0, "前向 import 不建边");
    assert_eq!(kg.graph.edges_connecting(use_late, later).count(), 1, "反向 import 建边");
}

/// 共享 name_map 的 impl 边：trait 先处理入索引，后处理文件的 impl 实体
/// 经 parse_impl_target + name_map 解析出 Implements 边（跨文件）。
#[test]
fn impl_edge_via_shared_index() {
    let insights = vec![
        file_insight(
            "src/trait_def.rs",
            vec![Entity {
                name: "Greeter".into(),
                kind: "trait".into(),
                line_start: 1,
                line_end: 1,
                doc_comment: None,
                signature: None,
                visibility: None,
            }],
            &[],
            "trait Greeter {}",
        ),
        file_insight(
            "src/impl_def.rs",
            vec![Entity {
                name: "impl Greeter for Foo".into(),
                kind: "impl".into(),
                line_start: 1,
                line_end: 1,
                doc_comment: None,
                signature: None,
                visibility: None,
            }],
            &[],
            "impl Greeter for Foo {}",
        ),
    ];
    let kg = build(&insights).unwrap();
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Implements), 1);
    let impl_id = find_node(&kg, "impl Greeter for Foo");
    let trait_id = find_node(&kg, "Greeter");
    assert_eq!(kg.graph.edges_connecting(impl_id, trait_id).count(), 1);
}

/// CRLF 源文本（Windows 行尾）：extract_body 经 lines() 剥离 \r 后调用解析
/// 不受影响（防止 \r 残留破坏「标识符紧邻 (」匹配）。
#[test]
fn call_edges_crlf_source() {
    let insights = vec![
        file_insight("src/a.rs", vec![fn_entity("helper", 1, 1)], &[], "fn helper() {}\r\n"),
        file_insight(
            "src/b.rs",
            vec![fn_entity("caller", 1, 3)],
            &[],
            "fn caller() {\r\n    helper(7);\r\n}\r\n",
        ),
    ];
    let kg = build(&insights).unwrap();
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Calls), 1, "CRLF 行尾不应破坏调用边解析");
}

/// 标识符与 '(' 之间不能有空白：helper (1) 不算调用（与 find("name(")
/// 语义等价）。
#[test]
fn call_edges_paren_adjacency() {
    let insights = vec![
        file_insight("src/a.rs", vec![fn_entity("helper", 1, 1)], &[], "fn helper() {}"),
        file_insight(
            "src/b.rs",
            vec![fn_entity("caller", 1, 3)],
            &[],
            "fn caller() {\n    helper (1);\n}",
        ),
    ];
    let kg = build(&insights).unwrap();
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Calls), 0, "helper 与 ( 之间有空格不算调用");
}

/// 索引与节点同步（总数精确）：3 文件构建 = 1 project + 2 模块(src, util)
/// + 3 文件 + 4 实体 = 10 节点；Contains 边 9（2 模块链 + 3 文件 + 4 实体）；
// 每个实体节点存在、kind 正确、file_path 归属正确文件。
#[test]
fn node_count_index_sync() {
    let insights = vec![
        file_insight("src/a.rs", vec![fn_entity("fa", 1, 1)], &[], "fn fa() {}"),
        file_insight("src/b.rs", vec![fn_entity("fb", 1, 1)], &[], "fn fb() {}"),
        file_insight(
            "util/c.rs",
            vec![fn_entity("fc", 1, 1), fn_entity("fd", 2, 2)],
            &[],
            "fn fc() {}\nfn fd() {}",
        ),
    ];
    let kg = build(&insights).unwrap();
    assert_eq!(kg.graph.node_count(), 10, "1 project + 2 模块 + 3 文件 + 4 实体");
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Contains), 9);
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Calls), 0);
    assert_eq!(count_edges_of_kind(&kg, EdgeKind::Imports), 0);
    for (name, expect_file) in [("fa", "a.rs"), ("fb", "b.rs"), ("fc", "c.rs"), ("fd", "c.rs")] {
        let n = find_node(&kg, name);
        assert_eq!(kg.graph[n].kind, NodeKind::Function);
        let file = kg.graph[n].file_path.as_deref().expect("实体必须归属文件");
        assert_eq!(
            file.rsplit(['/', '\\']).next().unwrap(),
            expect_file,
            "实体 file_path 必须归属正确文件"
        );
    }
}
