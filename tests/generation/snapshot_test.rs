//! 快照测试：验证 KnowledgeCard JSON 输出格式
//!
//! 使用 serde_json 反序列化验证 JSON 结构正确性，
//! 确保 LLM 生成的卡片被正确解析为标准格式。

#[cfg(test)]
mod tests {
    use code_repo_wiki::model::KnowledgeCard;

    #[test]
    fn test_knowledge_card_json_schema() {
        let json = r#"{
            "module_name": "test",
            "module_type": "module",
            "summary": "测试模块",
            "key_entities": [
                {"name": "fn test()", "kind": "function", "visibility": "public", "doc": "测试函数"}
            ],
            "dependencies": [],
            "dependents": [],
            "design_patterns": [],
            "todo_notes": []
        }"#;
        let card: KnowledgeCard = serde_json::from_str(json).unwrap();
        assert_eq!(card.summary, "测试模块");
        assert_eq!(card.key_entities.len(), 1);
        assert_eq!(card.key_entities[0].name, "fn test()");
    }

    #[test]
    fn test_knowledge_card_minimal_json() {
        let json = r#"{
            "module_name": "minimal",
            "module_type": "module",
            "summary": "",
            "key_entities": [],
            "dependencies": [],
            "dependents": [],
            "design_patterns": [],
            "todo_notes": []
        }"#;
        let card: KnowledgeCard = serde_json::from_str(json).unwrap();
        assert!(card.summary.is_empty());
        assert!(card.key_entities.is_empty());
        assert!(card.design_patterns.is_empty());
    }

    #[test]
    fn test_knowledge_card_with_multiple_entities() {
        let json = r#"{
            "module_name": "multi",
            "module_type": "module",
            "summary": "多实体模块",
            "key_entities": [
                {"name": "fn foo()", "kind": "function", "visibility": "public", "doc": null},
                {"name": "struct Bar", "kind": "struct", "visibility": "public", "doc": "Bar 结构"}
            ],
            "dependencies": ["crate::utils"],
            "dependents": ["crate::main"],
            "design_patterns": ["Singleton"],
            "todo_notes": ["需要重构"]
        }"#;
        let card: KnowledgeCard = serde_json::from_str(json).unwrap();
        assert_eq!(card.key_entities.len(), 2);
        assert!(card.dependencies.contains(&"crate::utils".to_string()));
        assert!(card.design_patterns.contains(&"Singleton".to_string()));
        assert_eq!(card.todo_notes.len(), 1);
    }
}
