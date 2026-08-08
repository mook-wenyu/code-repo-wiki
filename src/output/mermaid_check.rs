//! Mermaid 语法校验与降级（G2 校验-重试-降级循环的产物侧核心）
//!
//! LLM 生成的 Wiki 页面正文可能包含 Mermaid 代码块（架构图/流程图等）。
//! 语法错误的图渲染失败且难以人工察觉，因此：
//!
//! 1. **校验**：生成层在产出页面后、写盘前，对正文中的 Mermaid fence
//!    逐个用 merman-core 权威解析器校验（纯 Rust，对齐 mermaid@11.15.0
//!    语法基线，无需浏览器/外部进程）。
//! 2. **重试反馈**：坏块错误消息人类可读（t03 POC 实测），拼接为
//!    retry_feedback 注入下一次 LLM 调用，让模型修正语法。
//! 3. **降级**：重试耗尽后坏块替换为 `text` fence + 标记注释（对齐
//!    OpenWiki degrade-and-repair 模式）——坏图不出现在产物中（lint
//!    不再报错），且注释保留错误信息供下次生成时 LLM 参考修复。
//!
//! 校验与 lint 共用本模块（lint 对磁盘产物复核，生成层对内存正文校验）。

use std::sync::OnceLock;

/// Mermaid 校验重试上限（与 generate::wiki 的 `CITATION_RETRY_MAX` 对齐：
/// 首次调用 + 每次坏块反馈后重试，共 `MERMAID_RETRY_MAX + 1` 次调用）。
pub const MERMAID_RETRY_MAX: usize = 2;

/// 单条 Mermaid 语法问题
#[derive(Debug, Clone)]
pub struct MermaidIssue {
    /// 坏块在正文中的序号（0 起，按出现顺序）
    pub block_index: usize,
    /// 解析器错误消息（人类可读，可直接喂回 LLM）
    pub message: String,
}

/// 进程级共享解析器：merman 的引擎注册全部检测器，构建开销一次承担
/// （多次创建会重复注册；OnceLock 保证只初始化一次）。
fn shared_engine() -> &'static merman_core::Engine {
    static ENGINE: OnceLock<merman_core::Engine> = OnceLock::new();
    ENGINE.get_or_init(merman_core::Engine::new)
}

/// 校验正文中的所有 Mermaid 代码块，返回坏块清单（无坏块返回空列表）
///
/// 识别规则：以行首 ```mermaid 开头的围栏块（Mermaid 官方约定），
/// 到下一个行首 ``` 结束。非围栏文本不校验。
pub fn validate_mermaid_blocks(content: &str) -> Vec<MermaidIssue> {
    let mut issues = Vec::new();
    for (idx, block) in extract_mermaid_blocks(content).iter().enumerate() {
        if let Err(e) = shared_engine().parse_diagram_sync(block, merman_core::ParseOptions::strict())
        {
            issues.push(MermaidIssue {
                block_index: idx,
                message: e.to_string(),
            });
        }
    }
    issues
}

/// 围栏行是否为 Mermaid 块（P3 修复：语言标记精确匹配，大小写不敏感）
///
/// ```mermaid 后只能跟空白或行尾——```mermaidx 是别的语言不误命中；
/// ```MERMAID 大写同样识别（原 starts_with("```mermaid") 两处都错）。
/// 行首允许前导空白（缩进围栏）。
fn fence_is_mermaid(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return false;
    };
    rest.trim_end().eq_ignore_ascii_case("mermaid")
}

/// 提取正文中所有 Mermaid 围栏块的代码内容（不含围栏本身）
fn extract_mermaid_blocks(content: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut in_fence = false;
    let mut fence_start = 0usize;

    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        if in_fence {
            // 围栏闭合：行首 ``` （代码内容行以 ``` 开头即闭合）
            if trimmed.starts_with("```") {
                // 块内容 = 起始围栏之后、闭合围栏之前的所有行
                // （不包含围栏行本身；fence_start 记录内容起始行号）
                let start_line = fence_start;
                let end_line = i;
                if end_line > start_line {
                    blocks.push(slice_lines(content, start_line, end_line));
                }
                in_fence = false;
            }
        } else if fence_is_mermaid(line) {
            // 起始围栏：记录内容起始（围栏下一行）
            in_fence = true;
            fence_start = i + 1;
        }
    }
    // 未闭合围栏：内容直到文末（解析器会判错，返回坏块）
    if in_fence {
        let start_line = fence_start;
        let all_lines: Vec<&str> = content.lines().collect();
        if start_line < all_lines.len() {
            blocks.push(slice_lines(content, start_line, all_lines.len()));
        }
    }
    blocks
}

/// 按行号范围切片（start 含、end 不含），保留行尾换行结构
fn slice_lines(content: &str, start: usize, end: usize) -> &str {
    let mut offset = 0usize;
    let mut line_start = 0usize;
    for (i, line) in content.lines().enumerate() {
        if i == start {
            line_start = offset;
        }
        if i == end - 1 {
            // 目标末行的末尾偏移（含该行换行符，若存在）
            return &content[line_start..offset + line.len()];
        }
        offset += line.len() + 1; // +1 为换行符（最后一行无换行时 lines 已耗尽）
    }
    &content[line_start..]
}

/// 构造坏块的重试反馈（注入 LLM 下一次调用的 user 消息）
///
/// 逐块列出错误消息，引导模型修正语法；块号与原文对照可定位。
pub fn mermaid_retry_feedback(issues: &[MermaidIssue]) -> String {
    let mut out = String::from(
        "你输出的 Markdown 中包含 Mermaid 语法错误的代码块，请修正后重试。\n\
         错误清单（块号从 0 开始计数，按正文出现顺序）：\n",
    );
    for issue in issues {
        out.push_str(&format!("- 块 {}: {}\n", issue.block_index, issue.message));
    }
    out.push_str("请确保修正后的 Mermaid 语法合法，或改用普通文本描述。");
    out
}

/// 降级坏块：替换为 `text` fence + 标记注释（OpenWiki degrade-and-repair 模式）
///
/// - 坏块内容保留（信息不丢失），但语言标记改为 `text`——渲染器不解析，
///   lint 的 bad-mermaid 检查不再命中；
/// - 上方插入 HTML 注释 `<!-- code-repo-wiki: mermaid parse failed: ... -->`，
///   错误消息单行化（换行/回车替换为空格，防注释语法逃逸），
///   供人工与下次 LLM 生成时参考修复。
/// - 好块原样保留。
pub fn degrade_mermaid_blocks(content: &str, issues: &[MermaidIssue]) -> String {
    let mut out = String::with_capacity(content.len() + 256);
    let mut pending_degrade: Option<String> = None; // 待降级坏块的错误消息
    // text 围栏是否已打开且未闭合（U04/D6 修复的核心状态：坏块体有内容时
    // pending_degrade 在体首行即被消费，文末是否补闭合只能由本状态判定——
    // 此前文末只补注释与 ```text 开头不补闭合，未闭合 ```text 吞掉其后
    // 全部内容直到下一个围栏或文末，整页结构损坏）
    let mut text_fence_open = false;

    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(msg) = pending_degrade.take() {
            // 块体首行：先输出降级注释（含错误消息），再输出 text 围栏；
            // 该行即坏块内容的第一行，原样保留（信息不丢失）
            out.push_str(&format!(
                "<!-- code-repo-wiki: mermaid parse failed: {} -->\n```text\n{}\n",
                sanitize_message(&msg),
                line
            ));
            text_fence_open = true;
            continue;
        }
        if text_fence_open && trimmed.starts_with("```") {
            // 坏块的原文闭合围栏行：充当 text 围栏的闭合行原样输出
            out.push_str(line);
            out.push('\n');
            text_fence_open = false;
            continue;
        }
        if fence_is_mermaid(line) {
            // 围栏行：判断是否坏块。块号 = 此前出现的 mermaid 起始围栏数。
            // 坏块：记录错误消息并跳过围栏行（注释 + text 围栏在块体首行输出）。
            // 好块：围栏原样输出。
            let block_index = count_mermaid_fences_before(content, i);
            if let Some(issue) = issues.iter().find(|i| i.block_index == block_index) {
                pending_degrade = Some(issue.message.clone());
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    // 文末围栏仍未闭合（U04/D6）：补上闭合 ```，保证降级产物的结构完整。
    // pending_degrade 未消费 = 空体坏块（```mermaid 直接到文末），同样补闭合。
    if text_fence_open || pending_degrade.is_some() {
        if let Some(msg) = pending_degrade {
            out.push_str(&format!(
                "<!-- code-repo-wiki: mermaid parse failed: {} -->\n```text\n",
                sanitize_message(&msg)
            ));
        }
        out.push_str("```\n");
    }
    out
}

/// 统计第 `fence_line` 行之前出现的 mermaid 起始围栏数（=该行的块号）
fn count_mermaid_fences_before(content: &str, fence_line: usize) -> usize {
    content
        .lines()
        .take(fence_line)
        .filter(|l| fence_is_mermaid(l))
        .count()
}

/// 错误消息单行化（HTML 注释内不允许换行，防注释逃逸）
///
/// 除换行外，`-->` 序列也会提前终止 HTML 注释（mermaid 语法错误消息
/// 常含 `-->`，如 "unexpected token '-->'"）——统一替换为 `-→`，
/// 保证降级注释的闭合语义不被错误消息破坏（P3 修复）。
fn sanitize_message(msg: &str) -> String {
    msg.replace("-->", "-→")
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_ok_diagram() {
        let content = "## 图\n\n```mermaid\nflowchart LR\nA[Start] --> B[End]\n```\n";
        assert!(validate_mermaid_blocks(content).is_empty());
    }

    #[test]
    fn test_validate_bad_diagram() {
        // 坏图：节点标签未闭合（t03 POC 验证 merman 报 "Unterminated node label"）
        let content = "```mermaid\nflowchart LR\nA[hello world\nB --> C\n```\n";
        let issues = validate_mermaid_blocks(content);
        assert_eq!(issues.len(), 1, "应识别出 1 个坏块");
        assert!(issues[0].message.contains("Unterminated"), "错误消息应可读: {}", issues[0].message);
    }

    #[test]
    fn test_validate_no_fence() {
        let content = "纯文本，没有代码块\n";
        assert!(validate_mermaid_blocks(content).is_empty());
    }

    #[test]
    fn test_validate_multiple_blocks() {
        // 两个块：第一个好、第二个坏 → 只报坏的那个（块号 1）
        let content = "```mermaid\nflowchart LR\nA --> B\n```\n\n```mermaid\nflowchart TD\nC[unterminated\n```\n";
        let issues = validate_mermaid_blocks(content);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].block_index, 1);
    }

    #[test]
    fn test_validate_unclosed_fence_reports_error() {
        // 未闭合围栏（且内容本身语法错误）：内容整体作为一个块交给解析器 → 必然报错
        let content = "```mermaid\nflowchart LR\nA[hello world\n";
        let issues = validate_mermaid_blocks(content);
        assert_eq!(issues.len(), 1, "未闭合围栏应作为坏块报出");
        assert!(issues[0].message.contains("Unterminated"), "错误消息应可读: {}", issues[0].message);
    }

    #[test]
    fn test_degrade_replaces_only_bad_block() {
        let content = "```mermaid\nflowchart LR\nA[bad\n```\n\n```mermaid\nflowchart LR\nA[OK] --> B\n```\n";
        let issues = validate_mermaid_blocks(content);
        let degraded = degrade_mermaid_blocks(content, &issues);
        // 坏块被降级为 text + 注释
        assert!(degraded.contains("```text"), "坏块应降级为 text fence");
        assert!(degraded.contains("code-repo-wiki: mermaid parse failed"), "应含降级注释");
        assert!(degraded.contains("Unterminated"), "注释应含错误消息");
        // 好块保留原样
        assert!(degraded.contains("```mermaid\nflowchart LR\nA[OK] --> B\n```"), "好块应保留");
        // 坏图原始内容仍在（信息不丢失）
        assert!(degraded.contains("A[bad"), "坏块内容应保留");
    }

    #[test]
    fn test_degrade_no_issues_noop() {
        let content = "```mermaid\nflowchart LR\nA --> B\n```\n";
        let degraded = degrade_mermaid_blocks(content, &[]);
        assert_eq!(degraded, content, "无坏块时输出应与输入一致");
    }

    #[test]
    fn test_retry_feedback_lists_blocks() {
        let issues = vec![MermaidIssue {
            block_index: 0,
            message: "Unterminated node label (missing `]`)".into(),
        }];
        let fb = mermaid_retry_feedback(&issues);
        assert!(fb.contains("块 0"), "反馈应含块号");
        assert!(fb.contains("Unterminated"), "反馈应含错误消息");
    }

    #[test]
    fn test_sanitize_message_single_line() {
        assert_eq!(sanitize_message("a\nb\r\nc"), "a b  c");
    }
}

    /// U04/D6 防回归：未闭合围栏的坏块降级后必须产出闭合的 ```text 围栏
    ///（此前只补注释与 ```text 开头，文末缺闭合 → 整页结构损坏）
    #[test]
    fn test_degrade_unclosed_fence_closes_fence() {
        let content = "```mermaid\nflowchart LR\nA[hello world\n";
        let issues = validate_mermaid_blocks(content);
        assert_eq!(issues.len(), 1, "未闭合围栏应作为坏块报出");
        let degraded = degrade_mermaid_blocks(content, &issues);
        assert!(degraded.contains("```text"), "应降级为 text fence");
        assert!(degraded.contains("code-repo-wiki: mermaid parse failed"), "应含降级注释");
        // 围栏闭合性：恰好 1 对围栏（开+闭），且以闭合围栏结尾
        let fence_count = degraded.matches("```").count();
        assert_eq!(fence_count, 2, "应恰好 1 对围栏（开+闭），实际: {degraded}");
        assert!(degraded.ends_with("```\n"), "降级产物应以闭合围栏结尾, 实际: {degraded}");
    }

    /// U04/P3：```mermaidx 前缀不应被当作 mermaid 块（围栏语言精确匹配）
    #[test]
    fn test_validate_ignores_mermaidx_prefix() {
        let content = "```mermaidx\nflowchart LR\nA --> B\n```\n";
        assert!(validate_mermaid_blocks(content).is_empty(), "mermaidx 不是 mermaid 块");
    }

    /// U04/P3：```MERMAID 大写形式应被识别为 mermaid 块（大小写不敏感）
    #[test]
    fn test_validate_case_insensitive_fence() {
        let content = "```MERMAID\nflowchart LR\nA[unterminated\n```\n";
        let issues = validate_mermaid_blocks(content);
        assert_eq!(issues.len(), 1, "大写 MERMAID 围栏应被识别");
    }

    /// U04/P3：嵌套示例（```text 内含 ```mermaid）不应误报为 mermaid 块
    #[test]
    fn test_validate_skips_nested_example_fence() {
        let content = "```text\n示例:\n```mermaid\nflowchart LR\nA --> B\n```\n```\n";
        assert!(validate_mermaid_blocks(content).is_empty(), "text 块内的 mermaid 示例不应被当作真实 mermaid 块");
    }

    /// U04/P3：降级注释中的 `-->` 不得提前终止 HTML 注释（替换为 -→）
    #[test]
    fn test_sanitize_message_escapes_comment_terminator() {
        let msg = "unexpected token '-->' at line 1";
        let cleaned = sanitize_message(msg);
        assert!(!cleaned.contains("-->"), "--> 应被替换, 实际: {cleaned}");
        assert!(cleaned.contains("-→"), "应含替换后的 -→, 实际: {cleaned}");
    }
