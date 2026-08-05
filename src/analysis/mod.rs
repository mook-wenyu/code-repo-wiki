pub mod community;
pub mod feature;
pub mod graph;
pub mod module;


use anyhow::Result;

use crate::ingest::parser::FileInsight;
use crate::model::{KnowledgeGraph, ModuleCluster};

/// 从 FileInsight 列表构建完整知识图谱
pub fn build_graph(insights: &[FileInsight]) -> Result<KnowledgeGraph> {
    graph::build(insights)
}

/// 检测模块边界（社区检测）
pub fn detect_modules(graph: &KnowledgeGraph) -> Result<Vec<ModuleCluster>> {
    let detector = module::ModuleDetector::new(graph);
    Ok(detector.detect())
}

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::ingest::parser::{Entity, FileInsight, ImportStmt};
        use std::path::PathBuf;

        #[test]
        fn test_full_pipeline() {
            let insights = vec![
                FileInsight {
                    path: PathBuf::from("src/main.rs"),
                    language: "rust".into(),
                    entities: vec![Entity {
                        name: "main".into(),
                        kind: "fn".into(),
                        line_start: 1,
                        line_end: 5,
                        doc_comment: None,
                        signature: Some("fn main()".into()),
                        summary: None, visibility: None,
                    }],
                    imports: vec![],
                    doc_comments: vec![],
                    source: String::new(),
                },
                FileInsight {
                    path: PathBuf::from("src/lib.rs"),
                    language: "rust".into(),
                    entities: vec![Entity {
                        name: "add".into(),
                        kind: "fn".into(),
                        line_start: 1,
                        line_end: 3,
                        doc_comment: None,
                        signature: Some("fn add(a: i32, b: i32) -> i32".into()),
                        summary: None, visibility: None,
                    }],
                    imports: vec![ImportStmt {
                        source: "crate::main".into(),
                        alias: None,
                        line: 1,
                    }],
                    doc_comments: vec![],
                    source: String::new(),
                },
            ];

        let graph = build_graph(&insights).expect("构建图失败");
        assert!(graph.graph.node_count() > 0);
        assert!(graph.graph.edge_count() > 0);
        assert_eq!(graph.graph.node_count(), 6); // Project + Module(src) + 2 File + 2 Entity

        let modules = detect_modules(&graph).expect("模块检测失败");
        // 验证模块检测不 panic（当前测试图过小，可能返回空聚类）
        assert!(modules.len() < graph.graph.node_count());
    }

    /// A1 接线回归：build_graph 返回的图必须已填充 modules（生成层
    /// 以 graph.modules 为模块分组唯一来源，恒空会导致按模块生成
    /// 静默退化为按文件分块）
    #[test]
    fn test_build_graph_fills_modules() {
        let insights = vec![
            FileInsight {
                path: PathBuf::from("src/main.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "main".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 5,
                    doc_comment: None,
                    signature: Some("fn main()".into()),
                    summary: None, visibility: None,
                }],
                imports: vec![],
                doc_comments: vec![],
                source: String::new(),
            },
            FileInsight {
                path: PathBuf::from("src/lib.rs"),
                language: "rust".into(),
                entities: vec![Entity {
                    name: "add".into(),
                    kind: "fn".into(),
                    line_start: 1,
                    line_end: 3,
                    doc_comment: None,
                    signature: Some("fn add(a: i32, b: i32) -> i32".into()),
                    summary: None, visibility: None,
                }],
                imports: vec![ImportStmt {
                    source: "crate::main".into(),
                    alias: None,
                    line: 1,
                }],
                doc_comments: vec![],
                source: String::new(),
            },
        ];
        let graph = build_graph(&insights).expect("构建图失败");
        // build_graph 内部必须完成模块检测并写回 graph.modules
        // （此前该字段恒空，是模块聚类未接线的根因）
        assert_eq!(graph.modules, detect_modules(&graph).expect("模块检测失败"));
    }
}
