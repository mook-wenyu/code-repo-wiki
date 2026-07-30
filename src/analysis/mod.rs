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
                    }],
                    imports: vec![],
                    doc_comments: vec![],
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
                    }],
                    imports: vec![ImportStmt {
                        source: "crate::main".into(),
                        alias: None,
                        line: 1,
                    }],
                    doc_comments: vec![],
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
}
