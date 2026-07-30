use crate::generate::chunk::Chunk;
use crate::generate::llm::Message;
use crate::ingest::parser::Entity;
use crate::model::{KnowledgeGraph, ModuleCluster};

/// 生成模块摘要的系统 prompt
fn module_summary_system_prompt(language: &str) -> String {
    format!(
        r#"你是一个资深软件工程师，负责分析代码并生成模块摘要。
请分析输入的代码块信息，输出以下结构：

## 模块概述
简要描述这个模块的职责和功能。

## 核心实体
列出所有重要的结构体、trait、函数，每条一行：
- `实体名`（类型）— 描述

## 依赖关系
列出这个模块引用的外部模块和依赖。

## 设计要点
关键的设计决策和模式。

请用 {} 语言输出。"#,
        language
    )
}

/// 生成模块摘要的 user prompt
fn module_summary_user_prompt(chunk: &Chunk) -> String {
    let mut parts = Vec::new();

    parts.push(format!("模块路径: {}", chunk.module_path.join("::")));

    if !chunk.entities.is_empty() {
        parts.push("\n## 实体列表".to_string());
        for entity in &chunk.entities {
            let doc = entity
                .doc_comment
                .as_deref()
                .map(|d| d.lines().next().unwrap_or(""))
                .unwrap_or("");
            parts.push(format!(
                "- {} ({}): {} [行 {}..{}]",
                entity.name, entity.kind, doc, entity.line_start, entity.line_end
            ));
        }
    }

    if !chunk.imports.is_empty() {
        parts.push("\n## 导入语句".to_string());
        for import in &chunk.imports {
            parts.push(format!("- {}", import.source));
        }
    }

    if !chunk.file_paths.is_empty() {
        parts.push("\n## 关联文件".to_string());
        for path in &chunk.file_paths {
            parts.push(format!("- {}", path.display()));
        }
    }

    parts.join("\n")
}

/// 生成模块摘要的 prompt
pub fn module_summary_prompt(chunk: &Chunk, language: &str) -> Vec<Message> {
    vec![
        Message::system(module_summary_system_prompt(language)),
        Message::user(module_summary_user_prompt(chunk)),
    ]
}

/// 生成架构概览的 system prompt
fn architecture_overview_system_prompt(language: &str) -> String {
    format!(
        r#"你是一个资深软件架构师，负责分析整个项目的模块结构并生成架构概览文档。

请基于输入的模块聚类信息和依赖关系，输出以下结构：

# 项目架构概览

## 架构风格
描述项目采用的架构风格（如分层架构、模块化单体、微服务等）。

## 模块划分
列出所有模块及其职责：
- `模块名` — 职责描述

## 模块间依赖关系
描述各个模块之间的依赖关系和通信方式。

## 数据流
数据在模块间的流转方式。

## 架构决策
可以从此架构中推断出的关键架构决策。

请用 {} 语言输出。保留 Markdown 格式。"#,
        language
    )
}

/// 生成架构概览的 user prompt
fn architecture_overview_user_prompt(modules: &[ModuleCluster], graph: &KnowledgeGraph) -> String {
    let mut parts = Vec::new();

    parts.push("## 模块聚类信息".to_string());
    for module in modules {
        parts.push(format!(
            "- {} (内聚度: {:.2}, 耦合度: {:.2}, 节点数: {})",
            module.name,
            module.cohesion,
            module.coupling,
            module.node_ids.len()
        ));
    }

    parts.push("\n## 图统计".to_string());
    parts.push(format!("- 总节点数: {}", graph.graph.node_count()));
    parts.push(format!("- 总边数: {}", graph.graph.edge_count()));

    parts.join("\n")
}

/// 生成架构概览的 prompt
pub fn architecture_overview_prompt(
    modules: &[ModuleCluster],
    graph: &KnowledgeGraph,
    language: &str,
) -> Vec<Message> {
    vec![
        Message::system(architecture_overview_system_prompt(language)),
        Message::user(architecture_overview_user_prompt(modules, graph)),
    ]
}

/// 生成 Knowledge Card 的 system prompt
fn knowledge_card_system_prompt(language: &str) -> String {
    format!(
        r#"你是一个代码分析专家，负责生成结构化的 Knowledge Card。
Knowledge Card 是给 AI Agent 阅读的模块级结构化摘要。

请严格按以下 JSON 格式输出，不包含其他内容：

```json
{{
  "summary": "模块功能的一句话总结",
  "key_entities": [
    {{"name": "实体名", "kind": "结构体/函数/Trait", "visibility": "public/private/crate", "doc": "文档描述"}}
  ],
  "design_patterns": ["用到的设计模式"],
  "todo_notes": ["待办事项或注意点"]
}}
```

请用 {} 语言输出描述性字段。不要添加 Markdown 代码块标记之外的文字。"#,
        language
    )
}

/// 生成 Knowledge Card 的 prompt
pub fn knowledge_card_prompt(chunk: &Chunk, language: &str) -> Vec<Message> {
    vec![
        Message::system(knowledge_card_system_prompt(language)),
        Message::user(module_summary_user_prompt(chunk)),
    ]
}

/// 生成 Wiki Page 的 system prompt
fn wiki_page_system_prompt(language: &str) -> String {
    format!(
        r#"你是一个技术文档写手，负责生成项目 Wiki 页面。
Wiki 页面是给人类开发者阅读的叙述性文档。

请基于模块信息和卡片摘要，生成以下格式的 Wiki 页面：

# 模块名称

## 概述
用 2-3 句话描述模块的职责和功能。

## 核心实体
- `StructName` — 描述
- `fn_name()` — 描述
- `TraitName` — 描述

## 依赖关系
- `模块A` — 依赖说明

## 使用方式
简要说明如何使用这个模块。

请用 {} 语言输出。保持简洁、清晰。"#,
        language
    )
}

/// 生成 Wiki Page 的 user prompt
fn wiki_page_user_prompt(chunk: &Chunk, module_summary: &str) -> String {
    format!(
        "模块路径: {}\n\n## 代码信息\n实体数: {}, 文件数: {}\n\n## 卡片摘要\n{}",
        chunk.module_path.join("::"),
        chunk.entity_count(),
        chunk.file_paths.len(),
        module_summary
    )
}

/// 生成 Wiki Page 的 prompt
pub fn wiki_page_prompt(chunk: &Chunk, module_summary: &str, language: &str) -> Vec<Message> {
    vec![
        Message::system(wiki_page_system_prompt(language)),
        Message::user(wiki_page_user_prompt(chunk, module_summary)),
    ]
}

/// 生成单个实体摘要的 prompt
pub fn entity_summary_prompt(entity: &Entity, language: &str) -> String {
    format!(
        "系统指令：你是一个代码分析专家。请为以下代码实体生成一段简短的技术摘要。\n\n\
         实体信息：\n\
         - 类型：{}\n\
         - 名称：{}\n\
         - 签名：{}\n\
         - 文档注释：{}\n\
         \n\
         请用 {} 语言回复。",
        entity.kind,
        entity.name,
        entity.signature.as_deref().unwrap_or("无"),
        entity.doc_comment.as_deref().unwrap_or("无"),
        language
    )
}
