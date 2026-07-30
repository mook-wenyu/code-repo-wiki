use std::collections::HashMap;

use crate::model::{Reference, WikiDocument};

/// 断链信息
#[derive(Debug, Clone)]
pub struct BrokenLink {
    pub source_doc: String,
    pub broken_target: String,
    pub link_text: String,
}

/// 交叉引用索引
///
/// 从文档集中提取所有交叉引用链接，提供查找和验证功能。
pub struct CrossRefIndex {
    links: Vec<Reference>,
}

impl CrossRefIndex {
    /// 创建空索引
    pub fn new() -> Self {
        Self {
            links: Vec::new(),
        }
    }

    /// 从文档集构建交叉引用索引
    ///
    /// 收集所有文档中的引用关系。
    pub fn build(documents: &[WikiDocument]) -> Self {
        let mut links = Vec::new();

        for doc in documents {
            for reference in &doc.references {
                links.push(reference.clone());
            }
        }

        Self { links }
    }

    /// 查找引用某实体的所有文档
    ///
    /// 返回所有指向指定目标的引用列表。
    pub fn find_references(&self, target: &str) -> Vec<&Reference> {
        self.links
            .iter()
            .filter(|r| r.target_title == target || r.target_path.contains(target))
            .collect()
    }

    /// 验证所有链接是否有效
    ///
    /// 检查所有引用中 `target_title` 是否存在于文档集的标题映射中。
    /// 返回断链列表。
    pub fn validate(&self, documents: &[WikiDocument]) -> Vec<BrokenLink> {
        let valid_titles: HashMap<&str, &WikiDocument> = documents
            .iter()
            .map(|doc| (doc.title.as_str(), doc))
            .collect();

        let mut broken = Vec::new();

        for doc in documents {
            for reference in &doc.references {
                if !valid_titles.contains_key(reference.target_title.as_str()) {
                    broken.push(BrokenLink {
                        source_doc: doc.title.clone(),
                        broken_target: reference.target_title.clone(),
                        link_text: reference.relation.clone(),
                    });
                }
            }
        }

        broken
    }

    /// 返回原始链接列表
    pub fn links(&self) -> &[Reference] {
        &self.links
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DocumentKind;

    fn make_doc(title: &str, refs: Vec<Reference>) -> WikiDocument {
        WikiDocument {
            title: title.into(),
            kind: DocumentKind::WikiPage,
            content: String::new(),
            module_path: vec![],
            references: refs,
            last_updated: String::new(),
            fingerprint: None,
        }
    }

    #[test]
    fn test_build_and_find() {
        let docs = vec![
            make_doc("Core", vec![Reference {
                target_title: "Net".into(),
                target_path: "wiki/Net.md".into(),
                relation: "depends_on".into(),
            }]),
            make_doc("Net", vec![]),
        ];

        let index = CrossRefIndex::build(&docs);
        let refs = index.find_references("Net");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target_title, "Net");
    }

    #[test]
    fn test_validate_valid() {
        let docs = vec![
            make_doc("Core", vec![Reference {
                target_title: "Net".into(),
                target_path: "wiki/Net.md".into(),
                relation: "depends_on".into(),
            }]),
            make_doc("Net", vec![]),
        ];

        let index = CrossRefIndex::build(&docs);
        let broken = index.validate(&docs);
        assert!(broken.is_empty());
    }

    #[test]
    fn test_validate_broken() {
        let docs = vec![make_doc("Core", vec![Reference {
            target_title: "NonExistent".into(),
            target_path: "wiki/NonExistent.md".into(),
            relation: "depends_on".into(),
        }])];

        let index = CrossRefIndex::build(&docs);
        let broken = index.validate(&docs);
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].source_doc, "Core");
        assert_eq!(broken[0].broken_target, "NonExistent");
    }

    #[test]
    fn test_empty_index() {
        let index = CrossRefIndex::new();
        assert!(index.links().is_empty());
        assert!(index.find_references("anything").is_empty());
    }
}
