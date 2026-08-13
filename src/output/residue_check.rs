//! 模板占位符残留检测（T08b 模板警告 B）：LLM 输出中未渲染的 `{{` 扫描
//!
//! LLM 生成的 wiki 页面/卡片若残留未渲染的 `{{`（提示模板占位符泄漏，
//! 典型如 `{{summary}}`、`{{entity_name}}`），产物即损坏——消费侧看到的
//! 是字面双左大括号而非真实内容。
//!
//! 检测规则：LLM 输出文本中出现 `{{` 即视为残留。理由：正常产物
//! （markdown/JSON）不含双左大括号；提示模板中的 `{{` 是 Rust format!
//! 转义（渲染后为单 `{`），LLM 收到的示例是单括号，输出 `{{` 说明它把
//! 模板占位符概念泄漏进了产物。
//!
//! 残留无法降级（内容缺失没有合理的降级形态），故本模块只做扫描与
//! 重试反馈（validate/feedback 模式，仿 mermaid_check.rs），不做修复。

/// 模板占位符残留——LLM 输出中未渲染的 `{{` 出现位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateResidue {
    /// 行号（1 起）
    pub line: usize,
    /// 该行含残留的文本片段（截断至 120 字符便于日志展示）
    pub snippet: String,
}

/// 扫描 LLM 输出中的模板占位符残留（`{{`）。
///
/// 判定规则：文本中出现 `{{` 即残留。理由：正常产物（markdown/JSON）不含
/// 双左大括号；提示模板里的 `{{` 是 Rust format! 转义（渲染后为单 `{`），
/// LLM 收到的示例是单括号，输出 `{{` 说明它把模板占位符概念泄漏进了产物
/// （典型如 `{{summary}}`、`{{entity_name}}`）。空行/纯空白行不记。
pub fn scan_template_residue(content: &str) -> Vec<TemplateResidue> {
    let mut residues = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.contains("{{") {
            residues.push(TemplateResidue {
                line: idx + 1,
                snippet: line.trim().chars().take(120).collect(),
            });
        }
    }
    residues
}

/// 构造 LLM 重试反馈：列出残留位置并要求替换为真实内容。
///
/// 中文消息：指出输出含未渲染的模板占位符（{{），列出前 5 条 line+snippet，
/// 要求删除或替换为真实内容。残留无法降级，唯一修复路径是让 LLM 重写。
pub fn residue_retry_feedback(residues: &[TemplateResidue]) -> String {
    let mut out = String::from(
        "你输出的内容包含未渲染的模板占位符（{{），请修正后重试。\n\
         残留清单（行号从 1 开始计数，最多列出前 5 条）：\n",
    );
    for residue in residues.iter().take(5) {
        out.push_str(&format!("- 第 {} 行: {}\n", residue.line, residue.snippet));
    }
    out.push_str("要求：删除所有残留的模板占位符，替换为真实的实体/摘要内容，不得保留 {{ 形态。");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_no_residue() {
        // 普通 markdown/JSON 不含 `{{`，不应报任何残留
        let md = "## 概述\n\n本模块负责配置解析。\n";
        assert!(scan_template_residue(md).is_empty());
        let json = "{ \"a\": 1, \"b\": [1, 2] }";
        assert!(scan_template_residue(json).is_empty());
    }

    #[test]
    fn test_scan_single_line() {
        let content = "## 概述\n\n{{summary}}\n";
        let residues = scan_template_residue(content);
        assert_eq!(residues.len(), 1);
        assert_eq!(residues[0].line, 3);
        assert_eq!(residues[0].snippet, "{{summary}}");
    }

    #[test]
    fn test_scan_multiple_lines_ordered() {
        let content = "{{title}}\n正文\n{{summary}}\n";
        let residues = scan_template_residue(content);
        assert_eq!(residues.len(), 2);
        assert_eq!(residues[0].line, 1);
        assert_eq!(residues[1].line, 3);
    }

    #[test]
    fn test_scan_single_brace_not_reported() {
        // 单括号（代码示例/JSON 大括号）不是模板残留，不误报
        assert!(scan_template_residue("{foo}").is_empty());
        assert!(scan_template_residue("{ \"a\": 1 }").is_empty());
        // 混合行：只报含 `{{` 的行
        let content = "{foo}\n{{summary}}\n{ \"a\": 1 }\n";
        let residues = scan_template_residue(content);
        assert_eq!(residues.len(), 1);
        assert_eq!(residues[0].line, 2);
    }

    #[test]
    fn test_retry_feedback_lists_lines() {
        let residues = vec![
            TemplateResidue {
                line: 3,
                snippet: "{{summary}}".into(),
            },
            TemplateResidue {
                line: 5,
                snippet: "{{entity_name}}".into(),
            },
        ];
        let fb = residue_retry_feedback(&residues);
        assert!(fb.contains("第 3 行"), "反馈应含行号: {fb}");
        assert!(fb.contains("第 5 行"), "反馈应含行号: {fb}");
        assert!(fb.contains("模板占位符"), "反馈应含「模板占位符」: {fb}");
    }
}
