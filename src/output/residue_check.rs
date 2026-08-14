//! 模板占位符残留检测（T08b 模板警告 B）：LLM 输出中未渲染占位符形态的扫描
//!
//! LLM 生成的 wiki 页面/卡片若残留未渲染的占位符（提示模板占位符泄漏，
//! 典型如 `{{summary}}`、`{{entity_name}}`），产物即损坏——消费侧看到的
//! 是字面双左大括号而非真实内容。
//!
//! 检测规则：只把「未渲染占位符形态」`{{` + 标识符 + `}}`（如 `{{summary}}`、
//! `{{ foo }}`）判为残留，而不是任何 `{{`。收紧原因（实测误报）：代码/文档
//! 工具自身的源码 doc 注释天然含大量 `{{` 描述（本模块文档就是例子），LLM
//! 照实转述这些描述时，`line.contains("{{")` 会把「描述 `{{` 概念的合法文本」
//! 误判为泄漏——实测 `.code-repo-wiki/cards/zh/tests_gamma_scan.md` 3 处裸
//! `{{`（均无闭合 `}}`，Python 正则验证非 full-placeholder-shape）全部误报，
//! 且 wiki 页路径的假阳性会触发无谓 LLM 重试（wiki.rs 同用此扫描器）。
//!
//! 残留无法降级（内容缺失没有合理的降级形态），故本模块只做扫描与
//! 重试反馈（validate/feedback 模式，仿 mermaid_check.rs），不做修复。

/// 模板占位符残留——LLM 输出中未渲染占位符形态（`{{ident}}`）出现位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateResidue {
    /// 行号（1 起）
    pub line: usize,
    /// 该行含残留的文本片段（截断至 120 字符便于日志展示）
    pub snippet: String,
}

/// 扫描 LLM 输出中的模板占位符残留。
///
/// 判定规则：只报「未渲染占位符形态」——`{{` + 标识符（字母/数字/下划线，
/// 允许 `{{` 与 `}}` 间空白）+ `}}`，典型如 `{{summary}}`、`{{ foo }}`。
/// 不使用 `contains("{{")` 判定：代码/文档工具源码 doc 注释天然含 `{{`
/// 描述（本模块文档就是例子），LLM 转述这些描述时会把合法文本误判为泄漏。
/// 空行/纯空白行不记。
pub fn scan_template_residue(content: &str) -> Vec<TemplateResidue> {
    let mut residues = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if has_unrendered_placeholder(line) {
            residues.push(TemplateResidue {
                line: idx + 1,
                snippet: line.trim().chars().take(120).collect(),
            });
        }
    }
    residues
}

/// 一行中是否存在「未渲染占位符形态」（`{{ident}}` / `{{ foo }}`）。
///
/// 逐段扫描 `{{`，对每处用 [`placeholder_shape_at`] 验证是否构成完整形态；
/// 裸 `{{`（无闭合 `}}`，如描述性文本「`{{` 是 Rust format! 转义」）不命中。
fn has_unrendered_placeholder(line: &str) -> bool {
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find("{{") {
        let start = search_from + rel;
        if placeholder_shape_at(line, start) {
            return true;
        }
        search_from = start + 2;
    }
    false
}

/// `{{`（起始于 `brace_start`）之后是否构成完整占位符形态：`{{` + 标识符
/// （字母/数字/下划线，允许内部空白）+ `}}`。
///
/// 已知天花板（显式声明的简化，非兜底）：`{{foo`（无闭合 `}}` 的截断泄漏）
/// 会漏报。取舍理由：散文里裸 `{{` 更大概率是描述 `{{` 概念的合法引用
/// （实测误报全为此形态），完整占位符才更可能是真实泄漏；要覆盖截断泄漏需
/// 引入 `}}` 缺失启发式，误报风险随之上来，暂不做。
fn placeholder_shape_at(line: &str, brace_start: usize) -> bool {
    let rest = &line[brace_start + 2..];
    let bytes = rest.as_bytes();
    let mut i = 0;
    // 跳过 `{{` 与标识符之间的前导空白（`{{ foo }}`）
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    // 至少一个标识符字符（字母/数字/下划线）
    if i >= bytes.len() || (!bytes[i].is_ascii_alphanumeric() && bytes[i] != b'_') {
        return false;
    }
    // 消费标识符字符与内部空白，直到遇到 `}` 或其他字符
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i].is_ascii_whitespace())
    {
        i += 1;
    }
    // 要求闭合 `}}`
    i + 1 < bytes.len() && bytes[i] == b'}' && bytes[i + 1] == b'}'
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
    fn test_scan_whitespace_placeholder_hits() {
        // 占位符形态允许 `{{` 与 `}}` 之间空白：`{{ foo }}` 仍命中
        let content = "正文 {{ foo }} 结尾\n";
        let residues = scan_template_residue(content);
        assert_eq!(residues.len(), 1);
        assert_eq!(residues[0].line, 1);
    }

    #[test]
    fn test_scan_bare_double_brace_not_reported() {
        // 裸 `{{`（无闭合 `}}`）不是未渲染占位符形态：源码 doc 注释里描述 `{{`
        // 概念的合法文本不应误报（实测 tests_gamma_scan.md 第 17/23/51 行均为此形态）
        let desc = "LLM 输出中未渲染的 `{{` 出现位置。\n提示模板里的 `{{` 是 Rust format! 转义。\n";
        assert!(scan_template_residue(desc).is_empty());
        let code = "if s.contains(\"{{%\") { return; }\n";
        assert!(scan_template_residue(code).is_empty());
    }

    #[test]
    fn test_scan_truncated_placeholder_misses() {
        // 已知天花板（显式声明）：`{{foo`（无闭合 `}}` 的截断泄漏）漏报
        assert!(scan_template_residue("value: {{summary\n").is_empty());
    }

    #[test]
    fn test_scan_mixed_real_leak_still_reported() {
        // 真实泄漏仍报：即使同一行有描述 `{{` 的合法文本，占位符形态照样命中
        let content = "doc 注释描述 `{{`，但输出残留了 {{summary}}\n";
        let residues = scan_template_residue(content);
        assert_eq!(residues.len(), 1);
        assert_eq!(
            residues[0].snippet,
            "doc 注释描述 `{{`，但输出残留了 {{summary}}"
        );
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
