use crate::config::plan::{PlanTemplateType, ResolvedPlan};
use crate::generate::chunk::Chunk;
use crate::generate::llm::Message;
use crate::ingest::parser::Entity;
use crate::model::{KnowledgeGraph, ModuleCluster};

/// 将全局 notes 追加到 system prompt 末尾（Some 时）
///
/// notes 与模板内容用空行分隔，避免粘连影响格式。
fn append_plan_notes(system: &mut String, plan: Option<&ResolvedPlan>) {
    if let Some(notes) = plan.and_then(|p| p.notes.as_ref()) {
        system.push_str("\n\n");
        system.push_str(notes);
    }
}

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
    plan: Option<&ResolvedPlan>,
) -> Vec<Message> {
    let mut system = module_summary_system_prompt(language);
    append_plan_notes(&mut system, plan);
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
    plan: Option<&ResolvedPlan>,
) -> Vec<Message> {
    let mut system = architecture_overview_system_prompt(language);
    append_plan_notes(&mut system, plan);
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
    plan: Option<&ResolvedPlan>,
    pending_manual_edits: &[String],
) -> Vec<Message> {
    let mut system = knowledge_card_system_prompt(language);
    append_plan_notes(&mut system, plan);
    // 卡片专用 notes 叠加在全局 notes 之后
    if let Some(card_notes) = plan.and_then(|p| p.card_notes.as_ref()) {
        system.push_str("\n\n");
        system.push_str(card_notes);
    }
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

/// 生成 API 参考页的 system prompt
///
/// 由 plan sections 中 template_type=api-ref 的模块触发，
/// 输出面向开发者的紧凑 API 清单而非叙述性文档。
fn api_ref_system_prompt(language: &str) -> String {
    format!(
        r#"你是一个 API 文档专家，负责生成 API 参考页面。
API 参考页面面向开发者，逐条列出公开接口，格式紧凑、无冗余叙述。

请基于模块信息生成以下格式的 Markdown：

# 模块名称 API 参考

## 函数
- `fn_name(args) -> Ret` — 用途说明

## 结构体 / 枚举
- `TypeName` — 用途说明

## Trait / 接口
- `TraitName` — 用途说明

未出现的类别省略。

## 防编造契约（必须遵守）
- 只列出输入实体列表中真实存在的符号：不得编造不存在的函数、类型或
  接口名（api-ref 是全量 API 清单，编造一个不存在的 API 比遗漏危害更大）。
- 符号名称保持输入给出的拼写与形态（含私有字段/属性等细粒度实体时，
  照实列出，不猜测可见性或语义）。
- 不编写任何源码引用/行号（api-ref 不做引用声明，确定性渲染的实体行
  已自带定位信息）。
- 每个 API 条目只写一行，不得添加输入中不存在的说明性细节。

请用 {} 语言输出。保持简洁。"#,
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

/// 判断模块模式是否命中 chunk：同时匹配 :: 与 / 两种路径形态
fn section_matches(pattern: &str, module_path: &[String]) -> bool {
    let by_colon = module_path.join("::");
    let by_slash = module_path.join("/");
    let pat = match glob::Pattern::new(pattern) {
        Ok(p) => p,
        Err(_) => return false, // 无效模式视为不匹配
    };
    if pat.matches(&by_colon) || pat.matches(&by_slash) {
        return true;
    }
    // glob 的尾部 /** 不匹配目录本身（src/config/** 不含 src/config），剥掉后缀补一次匹配
    pattern
        .strip_suffix("/**")
        .is_some_and(|base| base == by_colon || base == by_slash)
}

/// 生成 Wiki Page 的 prompt
pub fn wiki_page_prompt(
    chunk: &Chunk,
    module_summary: &str,
    language: &str,
    plan: Option<&ResolvedPlan>,
) -> Vec<Message> {
    // 命中 sections 时按模板类型切换 system prompt，未命中回退默认模板
    let matching_section = plan
        .and_then(|p| p.sections.iter().find(|s| section_matches(&s.module_pattern, &chunk.module_path)));
    let mut system = match matching_section {
        Some(section) => match section.template_type {
            // api-ref：以 API 参考模板生成
            PlanTemplateType::ApiRef => api_ref_system_prompt(language),
            PlanTemplateType::Architecture | PlanTemplateType::Prd => {
                wiki_page_system_prompt(language)
            }
        },
        None => wiki_page_system_prompt(language),
    };
    // 全局 notes 追加到所有 system prompt 末尾
    append_plan_notes(&mut system, plan);
    // 模块级 notes 叠加在全局 notes 之后
    if let Some(section) = matching_section
        && let Some(ref notes) = section.notes
    {
        system.push_str("\n\n");
        system.push_str(notes);
    }
    // 白名单文档的写作提示：title 匹配模块名时追加到 user 消息末尾
    let module_name = chunk.module_path.last().map(String::as_str);
    let hints = plan
        .and_then(|p| p.whitelist.as_ref())
        .and_then(|docs| {
            docs.iter()
                .find(|d| d.hints.is_some() && Some(d.title.as_str()) == module_name)
                .and_then(|d| d.hints.as_deref())
        });
    let mut user = wiki_page_user_prompt(chunk, module_summary);
    if let Some(hints) = hints {
        user.push_str("\n\n写作提示（用户指定）: ");
        user.push_str(hints);
    }
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
    plan: Option<&ResolvedPlan>,
) -> Vec<Message> {
    let mut system = schema_doc_system_prompt(language);
    append_plan_notes(&mut system, plan);
    let mut user = format!("SQL 文件路径: {}\n\n## 建表语句块\n", path.display());
    for (i, block) in blocks.iter().enumerate() {
        user.push_str(&format!("### 语句块 {}\n```sql\n{}\n```\n\n", i + 1, block));
    }
    vec![Message::system(system), Message::user(user)]
}

/// 生成单个实体摘要的 prompt
pub fn entity_summary_prompt(
    entity: &Entity,
    language: &str,
    plan: Option<&ResolvedPlan>,
) -> String {
    let mut system = format!(
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
    );
    append_plan_notes(&mut system, plan);
    system
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::plan::PlanSection;

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
    fn test_plan_notes_injected_into_prompt() {
        let chunk = make_test_chunk(&["src", "config", "plan"]);
        // notes=Some：全局 notes 应追加到 system prompt 末尾，LLM 能看到计划指令
        let plan = ResolvedPlan {
            notes: Some("优先关注错误处理路径".into()),
            ..Default::default()
        };
        let messages = module_summary_prompt(&chunk, "zh", Some(&plan));
        assert!(messages[0].content.contains("优先关注错误处理路径"));
        // notes=None：system prompt 不应包含 notes 内容（特性开关关闭时零注入）
        let messages = module_summary_prompt(&chunk, "zh", None);
        assert!(!messages[0].content.contains("优先关注错误处理路径"));
    }

    #[test]
    fn test_api_ref_section_selects_api_template() {
        // sections 指定 src/config/** 使用 api-ref 模板
        let plan = ResolvedPlan {
            sections: vec![PlanSection {
                module_pattern: "src/config/**".into(),
                template_type: PlanTemplateType::ApiRef,
                notes: None,
            }],
            ..Default::default()
        };
        // 命中模块：system prompt 切换为 API 参考模板，
        // 特征文本为标题"API 参考"、函数段与签名标注（args -> Ret 即参数/返回值信息）
        let chunk = make_test_chunk(&["src", "config", "plan"]);
        let messages = wiki_page_prompt(&chunk, "摘要", "zh", Some(&plan));
        let system = &messages[0].content;
        assert!(system.contains("API 参考"));
        assert!(system.contains("## 函数"));
        assert!(system.contains("-> Ret"));
        // 未命中模块：回退默认叙述性模板，不应出现 API 参考特征文本
        let chunk = make_test_chunk(&["src", "generate", "prompt"]);
        let messages = wiki_page_prompt(&chunk, "摘要", "zh", Some(&plan));
        let system = &messages[0].content;
        assert!(system.contains("## 概述"));
        assert!(!system.contains("API 参考"));
        assert!(!system.contains("## 函数"));
    }

    #[test]
    fn test_section_matches_both_separators() {
        let path = vec!["src".to_string(), "config".to_string()];
        // 模块路径形态（::）与文件路径形态（/）都应命中
        assert!(section_matches("src::config", &path));
        assert!(section_matches("src/config", &path));
        assert!(section_matches("src/config/**", &path));
        // 无效模式视为不匹配
        assert!(!section_matches("[", &path));
        // 不相关模式不命中
        assert!(!section_matches("src/generate", &path));
    }

    #[test]
    fn test_knowledge_card_prompt_injects_pending_manual_edits() {
        let chunk = make_test_chunk(&["src", "config"]);
        // 存在记录：user 消息包含"人工修改待同步"节与记录内容
        let pending = vec!["人工修改待同步: wiki/zh/src_config.md 内容摘要: 用户改的".into()];
        let messages = knowledge_card_prompt(&chunk, "zh", None, &pending);
        let user = &messages[1].content;
        assert!(user.contains("## 人工修改待同步"));
        assert!(user.contains("wiki/zh/src_config.md"));
        // 无记录：不注入该节（避免空节）
        let messages = knowledge_card_prompt(&chunk, "zh", None, &[]);
        assert!(!messages[1].content.contains("人工修改待同步"));
    }

    #[test]
    fn test_schema_doc_prompt_contains_path_and_blocks() {        let blocks = vec!["CREATE TABLE users (\n    id INTEGER\n);"];
        let messages = schema_doc_prompt(
            std::path::Path::new("db/migrations/001_init.sql"),
            &blocks,
            "zh",
            None,
        );
        let user = &messages[1].content;
        assert!(user.contains("db/migrations/001_init.sql"));
        assert!(user.contains("CREATE TABLE users"));
        assert!(user.contains("```sql"));
        assert!(messages[0].content.contains("erDiagram"));
    }

    /// t04b（v21）：api-ref 模板必须携带防编造契约——api-ref 是全量 API 清单，
    /// 编造一个不存在的 API 比遗漏危害更大；且明确禁止编写源码引用
    /// （确定性渲染的实体行自带定位，LLM 不参与定位声明）。
    #[test]
    fn test_api_ref_prompt_has_anti_fabrication_contract() {
        let system = api_ref_system_prompt("zh");
        assert!(system.contains("防编造契约"));
        assert!(system.contains("不得编造不存在的函数、类型或"));
        assert!(system.contains("不编写任何源码引用"));
        // 契约措辞与 wiki 页模板同一强度层级（都含"不得编造"字样）
        let wiki_system = crate::generate::prompt::wiki_page_system_prompt("zh");
        assert!(wiki_system.contains("不得编造"));
    }
}
