use crate::generate::chunk::Chunk;
use crate::generate::llm::Message;
use crate::model::{KnowledgeGraph, ModuleCluster};

/// 生成模块摘要的系统 prompt
fn module_summary_system_prompt(language: &str) -> String {
    format!(
        r#"你是一个资深软件工程师，负责分析代码并生成模块摘要。

请按以下步骤**在内部**完成分析（不要输出思考过程，只输出最终结果）：
1. 通读实体列表与导入语句，识别模块的核心职责与边界；
2. 依据实体间调用/导入关系判断模块对外的依赖；
3. 总结关键设计决策与模式；
4. 输出以下结构。

输出结构：

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
pub fn module_summary_prompt(
    chunk: &Chunk,
    language: &str,
) -> Vec<Message> {
    let system = module_summary_system_prompt(language);
    vec![
        Message::system(system),
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

**模块真实性约束（必须遵守）**：模块划分小节只列出输入模块聚类信息中给出的
模块名，不得添加、改名或合并输入中不存在的模块。

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
/// 生成单模块一行职责描述的 prompt（自底向上合成的一层：
/// 架构/概览基于各模块职责描述输出，而非只有模块名+节点数）
///
/// 输入 = 模块名 + 实体名列表（≤30，LLM 据此判断职责边界），
/// 输出约束 = 一句话、≤30 字、只输出描述文本本身（无前缀/引号/换行）。
pub fn module_description_prompt(
    module_name: &str,
    entity_names: &[String],
    language: &str,
) -> Vec<Message> {
    let entities = if entity_names.is_empty() {
        "(无实体)".to_string()
    } else {
        entity_names.join(", ")
    };
    vec![
        Message::system(format!(
            "你是代码架构分析专家。请用一句中文（{} 字以内）概括给定模块的职责。\
             只输出职责描述本身，不要前缀、引号或换行。",
            if language == "zh" { 30 } else { 60 }
        )),
        Message::user(format!(
            "模块名: {module_name}\n包含实体: {entities}\n\n请输出该模块的一句话职责描述。"
        )),
    ]
}

/// 生成架构概览页面（自顶向下的系统级描述）
pub fn architecture_overview_prompt(
    modules: &[ModuleCluster],
    graph: &KnowledgeGraph,
    language: &str,
) -> Vec<Message> {
    let system = architecture_overview_system_prompt(language);
    vec![
        Message::system(system),
        Message::user(architecture_overview_user_prompt(modules, graph)),
    ]
}

/// 生成 Knowledge Card 的 system prompt
fn knowledge_card_system_prompt(language: &str) -> String {
    format!(
        r#"你是一个代码分析专家，负责生成结构化的 Knowledge Card。
Knowledge Card 是给 AI Agent 阅读的模块级结构化摘要。

请按以下步骤**在内部**完成分析（不要输出思考过程，只输出最终 JSON）：
1. 归纳模块职责与边界，形成一句话总结；
2. 识别关键实体及其对外契约（可见性、职责）；
3. 推断设计模式、技术栈与编码规范；
4. 若输入中含"人工修改待同步"记录，将其内容纳入描述（不要删除记录本身）；
5. 严格按 JSON 格式输出最终结果。

**实体真实性约束（必须遵守）**：key_entities 只允许列出输入实体信息中真实存在的
实体（名称与输入一致），不得编造不存在的实体；找不到时列表可以为空。

请严格按以下 JSON 格式输出，不包含其他内容：

```json
{{
  "summary": "模块功能的一句话总结",
  "key_entities": [
    {{"name": "实体名", "kind": "结构体/函数/Trait", "visibility": "public/private/crate", "doc": "文档描述"}}
  ],
  "design_patterns": ["用到的设计模式"],
  "todo_notes": ["待办事项或注意点"],
  "coding_spec": "该模块遵循的编码规范（无则省略该字段）",
  "tech_stack": ["该模块用到的技术栈，如 tokio/serde/petgraph"],
  "architecture": "该模块的内部架构或关键设计说明（无则省略该字段）"
}}
```

请用 {} 语言输出描述性字段。不要添加 Markdown 代码块标记之外的文字。"#,
        language
    )
}

/// 生成 Knowledge Card 的 prompt
///
/// pending_manual_edits 为旧卡片上"人工修改待同步"记录（非空时注入 user 消息，
/// 要求 LLM 生成/更新卡片时考虑这些修改；记录本身由增量管道维护，不由 LLM 产出）。
pub fn knowledge_card_prompt(
    chunk: &Chunk,
    language: &str,
    pending_manual_edits: &[String],
) -> Vec<Message> {
    let system = knowledge_card_system_prompt(language);
    let mut user = module_summary_user_prompt(chunk);
    // 人工修改待同步：只在存在记录时注入，避免空节污染输入
    if !pending_manual_edits.is_empty() {
        user.push_str("\n\n## 人工修改待同步\n\n");
        user.push_str(
            "以下页面被人工修改，与代码最新状态可能不一致。\
             请结合这些修改生成卡片描述（如更新摘要、实体说明），但不要删除下述记录本身：\n",
        );
        for note in pending_manual_edits {
            user.push_str(&format!("- {note}\n"));
        }
    }
    vec![Message::system(system), Message::user(user)]
}

/// 生成卡片编辑/补充/重写指令的 prompt
///
/// mode 为 "modify"/"supplement"/"rewrite"：
/// - modify：现有卡片内容 + 指令
/// - supplement：现有内容保留，末尾追加新内容
/// - rewrite：仅指令 + 模块来源信息（不携带现有内容）
///
/// references 为参考材料段落（空字符串表示无参考文件）。
pub fn edit_card_prompt(
    mode: &str,
    module: &str,
    existing: &str,
    instruction: &str,
    references: &str,
    language: &str,
) -> Vec<Message> {
    let system = format!(
        r#"你是一个代码分析专家，负责编辑 Knowledge Card。
Knowledge Card 是给 AI Agent 阅读的模块级结构化摘要，使用固定 Markdown 格式：

# 模块名

## 摘要
模块功能总结

## 核心实体
- `实体名`（类型）— 描述

## 相关文件
- 文件路径

## 设计模式
- 模式

缺失的字段省略对应小节。请直接输出编辑后的完整卡片 Markdown，不要代码块包裹，不要添加无关内容。请用 {} 语言输出描述。"#,
        language
    );

    // rewrite 不携带现有内容（仅指令 + 模块来源信息）；其余模式携带并给出保留/修改语义
    let mut user = if mode == "rewrite" {
        format!("模块: {module}\n\n指令: {instruction}\n\n忽略任何旧版本内容，全量重写该模块的卡片。")
    } else {
        let hint = if mode == "supplement" {
            "保留现有卡片内容不变，按指令在末尾追加新内容"
        } else {
            "按指令修改现有卡片内容，其余部分保持不变"
        };
        format!("模块: {module}\n\n指令: {instruction}（{hint}）\n\n## 现有卡片内容\n{existing}")
    };
    if !references.is_empty() {
        user.push_str(&format!("\n\n## 参考材料\n{references}"));
    }
    vec![Message::system(system), Message::user(user)]
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

## 源码引用契约（必须遵守）
- 提及任何具体函数、结构体、文件时，必须携带真实存在的源码引用：
  `相对路径:行号`（如 `src/fs.rs:28`）或 `相对路径:起始行-结束行`（如 `src/fs.rs:28-45`），
  写在提及处所在行内。
- 引用必须真实存在：只引用输入实体列表/关联文件中给出的文件与行号，
  不得编造不存在的文件或行号。
- 每个小节至少包含一条引用。

请用 {} 语言输出。保持简洁、清晰。"#,
        language
    )
}

/// 实体签名单行化（v32 7.1 FR-201 签名级片段注入）
///
/// 签名是多行代码文本：压成单行避免破坏清单逐行结构；超 8 行或超 160 字符
/// 截断至 160 字符并追加 …（边界：签名缺失/空白 → 空串，不输出占位）。
/// 只读格式化，不改变 chunk 结构与 insights_cache 格式。
fn entity_signature_line(e: &crate::ingest::parser::Entity) -> String {
    let Some(raw) = &e.signature else {
        return String::new();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let line_count = trimmed.lines().count();
    // lines() 会剥行尾 \r（CRLF 源文件签名实测含 \r\n，parser 原始节点文本
    // 不做归一化——ingest/mod.rs read_to_string 亦不归一化），用 join 压平
    // 同时保证行数与压平一致（reviewer LOW 修复）。
    let mut flat = trimmed.lines().collect::<Vec<_>>().join(" ");
    if line_count > 8 || flat.chars().count() > 160 {
        flat = flat.chars().take(160).collect();
        flat.push('…');
    }
    format!("，签名: {flat}")
}

/// 生成 Wiki Page 的 user prompt
///
/// 输入包含「实体引用清单」：实体名+类型+文件路径:行号（真源），
/// 是源码引用契约（不得编造）唯一允许的引用来源——此前只传卡片摘要，
/// 摘要不含行号，LLM 无法兑现契约只能编造（v29 实测 bad-citation 来源）。
/// v32 7.1 起每条追加签名级片段（≤8 行/≤160 字符），供 LLM 精确引用签名
/// 而无需猜测（FR-201）。实体过多时截断前 80 条并注明总数，避免输入超长。
fn wiki_page_user_prompt(chunk: &Chunk, module_summary: &str) -> String {
    let mut entity_lines: Vec<String> = chunk
        .entities
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let sig = entity_signature_line(e);
            match chunk.entity_sources.get(i) {
                Some(path) => format!("- `{}` ({}) — {}:{}{}", e.name, e.kind, path.display(), e.line_start, sig),
                None => format!(
                    "- `{}` ({}) — 第 {}-{} 行（所属文件未记录）{}",
                    e.name, e.kind, e.line_start, e.line_end, sig
                ),
            }
        })
        .take(80)
        .collect();
    if chunk.entities.len() > 80 {
        entity_lines.push(format!("- …共 {} 个实体，仅列出前 80 个", chunk.entities.len()));
    }
    format!(
        "模块路径: {}\n\n## 代码信息\n实体数: {}, 文件数: {}\n\n## 实体引用清单\n{}\n\n## 卡片摘要\n{}",
        chunk.module_path.join("::"),
        chunk.entity_count(),
        chunk.file_paths.len(),
        entity_lines.join("\n"),
        module_summary
    )
}

/// 生成 Wiki Page 的 prompt
pub fn wiki_page_prompt(
    chunk: &Chunk,
    module_summary: &str,
    language: &str,
) -> Vec<Message> {
    let system = wiki_page_system_prompt(language);
    let user = wiki_page_user_prompt(chunk, module_summary);
    vec![Message::system(system), Message::user(user)]
}

/// 生成数据库 Schema 文档的 system prompt
///
/// 要求输出表结构 Markdown 与 Mermaid erDiagram 代码块。
pub fn schema_doc_system_prompt(language: &str) -> String {
    format!(
        r#"你是一个数据库专家，负责分析 SQL 迁移文件并生成 Schema 文档。

请基于输入的建表语句，输出以下格式的 Markdown：

# 数据库 Schema 文档

## 表结构
对每张表用表格列出字段：列名 | 类型 | 约束 | 说明

## 关系说明
描述表之间的外键关系和约束。

## ER 图
用 Mermaid erDiagram 代码块画出实体关系图。

请用 {} 语言输出。保留 Markdown 与 Mermaid 代码块格式。"#,
        language
    )
}

/// 生成数据库 Schema 文档的 prompt
///
/// user 消息包含 SQL 文件路径与切分出的建表语句块。
pub fn schema_doc_prompt(
    path: &std::path::Path,
    blocks: &[&str],
    language: &str,
) -> Vec<Message> {
    let system = schema_doc_system_prompt(language);
    let mut user = format!("SQL 文件路径: {}\n\n## 建表语句块\n", path.display());
    for (i, block) in blocks.iter().enumerate() {
        user.push_str(&format!("### 语句块 {}\n```sql\n{}\n```\n\n", i + 1, block));
    }
    vec![Message::system(system), Message::user(user)]
}

// 实体摘要 prompt 已删除（v31）：随 generate_entity_summaries 一并移除——
// Entity.summary 字段零消费者，每实体一次 LLM 调用纯浪费（见 mod.rs 注释）。

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造指定模块路径的空 Chunk（测试辅助）
    ///
    /// 实体/导入/依赖留空即可，prompt 层测试只关心模块路径与 notes/sections 注入。
    fn make_test_chunk(module_path: &[&str]) -> Chunk {
        Chunk {
            module_path: module_path.iter().map(|s| s.to_string()).collect(),
            entities: vec![],
            imports: vec![],
            dependencies: vec![],
            file_paths: vec![],
            entity_sources: vec![],
        }
    }

    #[test]
    fn test_knowledge_card_prompt_injects_pending_manual_edits() {
        let chunk = make_test_chunk(&["src", "config"]);
        // 存在记录：user 消息包含"人工修改待同步"节与记录内容
        let pending = vec!["人工修改待同步: wiki/zh/src_config.md 内容摘要: 用户改的".into()];
        let messages = knowledge_card_prompt(&chunk, "zh", &pending);
        let user = &messages[1].content;
        assert!(user.contains("## 人工修改待同步"));
        assert!(user.contains("wiki/zh/src_config.md"));
        // 无记录：不注入该节（避免空节）
        let messages = knowledge_card_prompt(&chunk, "zh", &[]);
        assert!(!messages[1].content.contains("人工修改待同步"));
    }

    #[test]
    fn test_schema_doc_prompt_contains_path_and_blocks() {        let blocks = vec!["CREATE TABLE users (\n    id INTEGER\n);"];
        let messages = schema_doc_prompt(
            std::path::Path::new("db/migrations/001_init.sql"),
            &blocks,
            "zh",
        );
        let user = &messages[1].content;
        assert!(user.contains("db/migrations/001_init.sql"));
        assert!(user.contains("CREATE TABLE users"));
        assert!(user.contains("```sql"));
        assert!(messages[0].content.contains("erDiagram"));
    }

    /// 防编造契约（A1 补强）：卡片与架构 prompt 必须显式约束实体/模块真实性，
    /// 防止 LLM 输出输入中不存在的实体名或模块名（anti-fabrication 契约）。
    #[test]
    fn test_anti_fabrication_constraints_in_card_and_architecture_prompts() {
        let chunk = make_test_chunk(&["src", "config"]);
        let card_messages = knowledge_card_prompt(&chunk, "zh", &[]);
        assert!(
            card_messages[0].content.contains("不得编造"),
            "卡片 prompt 必须含实体真实性约束: {}",
            card_messages[0].content
        );

        let arch = architecture_overview_prompt(&[], &KnowledgeGraph::default(), "zh");
        assert!(
            arch[0].content.contains("不得添加"),
            "架构 prompt 必须含模块真实性约束: {}",
            arch[0].content
        );
    }

    /// 实体引用清单（A8）：wiki page 的 user 输入必须携带实体名+文件:行号真源，
    /// 引用契约才能兑现（LLM 不得编造，且输入中确有其物可引）。
    #[test]
    fn test_wiki_page_user_prompt_contains_entity_reference_list() {
        let mut chunk = make_test_chunk(&["src", "alpha"]);
        chunk.entities = vec![crate::ingest::parser::Entity {
            name: "alpha_fn".into(),
            kind: "fn".into(),
            line_start: 1,
            line_end: 3,
            doc_comment: None,
            signature: None,
            visibility: None,
        }];
        chunk.entity_sources = vec![std::path::PathBuf::from("src/alpha.rs")];
        let user = wiki_page_user_prompt(&chunk, "卡片摘要");
        assert!(
            user.contains("src/alpha.rs:1"),
            "引用清单必须含文件:行号: {}",
            user
        );
        assert!(user.contains("alpha_fn"));
        // 无文件记录时的诚实标注路径（不得编造文件）
        chunk.entity_sources = vec![];
        let user = wiki_page_user_prompt(&chunk, "卡片摘要");
        assert!(user.contains("所属文件未记录"));
    }

    /// 签名级片段注入（v32 7.1 FR-201）：实体清单行必须携带签名（≤8 行/≤160
    /// 字符，超长截断加 …）；签名缺失/空白 → 空串不输出占位。
    #[test]
    fn test_wiki_page_user_prompt_injects_entity_signature() {
        // 签名存在且短：完整输出
        let e = crate::ingest::parser::Entity {
            name: "short_fn".into(),
            kind: "fn".into(),
            line_start: 1,
            line_end: 2,
            doc_comment: None,
            signature: Some("pub fn short_fn(x: u32) -> u32".into()),
            visibility: None,
        };
        assert_eq!(
            entity_signature_line(&e),
            "，签名: pub fn short_fn(x: u32) -> u32"
        );
        // 超 8 行：截断至 160 字符加 …
        let long = (0..10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let e2 = crate::ingest::parser::Entity {
            name: "long_fn".into(),
            kind: "fn".into(),
            line_start: 1,
            line_end: 12,
            doc_comment: None,
            signature: Some(long),
            visibility: None,
        };
        let out = entity_signature_line(&e2);
        assert!(out.starts_with("，签名: "));
        assert!(out.ends_with('…'), "超限签名必须截断加 …: {out}");
        assert!(out.chars().count() <= 167, "截断后不超过 160+签名前缀: {out}");
        // 单行超 160 字符（>8 行分支之外的另一截断触发）：截断加 …
        let wide = "w".repeat(200);
        let e_wide = crate::ingest::parser::Entity {
            name: "wide_fn".into(),
            kind: "fn".into(),
            line_start: 1,
            line_end: 2,
            doc_comment: None,
            signature: Some(wide),
            visibility: None,
        };
        let out_wide = entity_signature_line(&e_wide);
        assert!(out_wide.ends_with('…'), "160 字符截断分支: {out_wide}");
        assert_eq!(out_wide.chars().count(), 166, "160 截断+前缀5+…1");
        // CRLF 源文件（\r\n 换行）：压平后不得残留 \r（reviewer LOW）
        let crlf = "pub fn a(\r\n    x: u32,\r\n) -> u32".to_string();
        let e_crlf = crate::ingest::parser::Entity {
            name: "crlf_fn".into(),
            kind: "fn".into(),
            line_start: 1,
            line_end: 4,
            doc_comment: None,
            signature: Some(crlf),
            visibility: None,
        };
        let out_crlf = entity_signature_line(&e_crlf);
        assert!(!out_crlf.contains('\r'), "CRLF 残留 \r: {out_crlf}");
        // 压平只把换行变成空格，行内缩进原样保留
        assert!(
            out_crlf.contains("pub fn a(     x: u32, ) -> u32"),
            "CRLF 压平: {out_crlf}"
        );
        // 签名缺失 / 空白：空串（不输出占位）
        let e3 = crate::ingest::parser::Entity {
            name: "no_sig".into(),
            kind: "fn".into(),
            line_start: 1,
            line_end: 2,
            doc_comment: None,
            signature: None,
            visibility: None,
        };
        assert_eq!(entity_signature_line(&e3), "");
        let e4 = crate::ingest::parser::Entity {
            name: "blank_sig".into(),
            kind: "fn".into(),
            line_start: 1,
            line_end: 2,
            doc_comment: None,
            signature: Some("   \n  ".into()),
            visibility: None,
        };
        assert_eq!(entity_signature_line(&e4), "");
        // 集成：wiki page prompt 携带签名行
        let mut chunk = make_test_chunk(&["src", "alpha"]);
        chunk.entities = vec![crate::ingest::parser::Entity {
            name: "alpha_fn".into(),
            kind: "fn".into(),
            line_start: 1,
            line_end: 3,
            doc_comment: None,
            signature: Some("pub fn alpha_fn()".into()),
            visibility: None,
        }];
        chunk.entity_sources = vec![std::path::PathBuf::from("src/alpha.rs")];
        let user = wiki_page_user_prompt(&chunk, "卡片摘要");
        assert!(
            user.contains("src/alpha.rs:1，签名: pub fn alpha_fn()"),
            "引用清单行必须含签名: {}",
            user
        );
    }
}
