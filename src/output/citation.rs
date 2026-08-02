//! 源码引用契约（P0-1）：LLM 生成正文必须携带可验证的源码引用
//!
//! 业界对齐：RepoDocs 每个声明引用 file:line、DeepWiki 行级点击引用、
//! Google Code Wiki 每节链接源码定义——引用是防幻觉的信任机制。
//! 引用格式：`相对路径:行号`（`src/fs.rs:28`）或 `相对路径:起始行-结束行`
//! （`src/fs.rs:28-45`），路径相对项目根。
//!
//! 校验规则（零成本，无 LLM）：路径在项目根下真实存在，且结束行号
//! 不超过文件实际行数。生成层（wiki.rs 重试）与产物健康检查（lint
//! 引用存在性）共用本模块——lint 只读磁盘产物，生成层校验内存内容。

use std::path::Path;

/// 单条源码引用
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// 相对项目根的路径
    pub path: String,
    /// 起始行（1-based）
    pub start: usize,
    /// 结束行（单行引用时等于 start）
    pub end: usize,
}

/// 无效引用（校验失败的原因）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCitation {
    pub citation: Citation,
    pub reason: String,
}

/// 判断字符是否属于引用路径组成部分（字母数字、点、下划线、斜杠、反斜杠、连字符）
///
/// 反斜杠必须包含：render_api_reference 在 Windows 上输出的实体定位
/// 是 `src\auth.rs:2`（平台路径分隔符），回溯时漏掉 `\` 会把路径截断
/// 成 `auth.rs` 导致 lint 误报引用不存在。
fn is_path_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'/' | b'\\' | b'-')
}

/// 路径最后一段（最后一个 / 或 \ 之后）是否形如「已知扩展名」
///
/// 最后一段必须含点，且点后是字母开头的非空扩展名（.rs/.py/.go 等）。
/// 纯数字后缀（v2.0、1.2）是版本号/时刻格式而非引用路径，靠此规则排除；
/// `src/v1.5.rs` 的最后一段是 `v1.5.rs`，扩展名 .rs 合法，正常放行。
fn has_valid_extension(path: &str) -> bool {
    let last = path.rsplit(['/', '\\']).next().unwrap();
    match last.rfind('.') {
        Some(dot) => {
            let ext = &last[dot + 1..];
            !ext.is_empty() && ext.as_bytes()[0].is_ascii_alphabetic()
        }
        None => false,
    }
}

/// 从文本中提取所有 `path:line` / `path:start-end` 引用
///
/// 规则（最小误报设计）：
/// - 路径必须含点（`src/fs.rs` 的 `.`），排除 `https:`、`C:` 等无点前缀
/// - 路径不含 `//`（排除 URL）且不以 `/` 开头（排除绝对路径，统一相对根）
/// - 冒号后必须紧跟数字；`-数字` 后缀视为区间结束
/// - 行号非零
pub fn extract_citations(content: &str) -> Vec<Citation> {
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' {
            // 回溯提取路径起点
            let mut p = i;
            while p > 0 && is_path_char(bytes[p - 1]) {
                p -= 1;
            }
            let mut path = &content[p..i];
            // 列表项形态 `-src/fs.rs` 会把列表前缀 `-` 吸收进路径，剔除前导连字符；
            // `my-file.rs` 的连字符在路径中间，不受影响
            path = path.trim_start_matches('-');
            // Windows 盘符绝对路径（`C:\...` 或 `C:/...`）不得提取。回溯在盘符
            // 冒号前停止时路径形如 `\repo\x.rs`（盘符在路径外，看路径前的 `X:`）；
            // 前导连字符剔除后路径可能以盘符开头（`C:/repo/x.rs`），直接按形态识别
            let has_drive_prefix = (p >= 2
                && bytes[p - 2].is_ascii_alphabetic()
                && bytes[p - 1] == b':'
                && matches!(bytes.get(p), Some(b'\\' | b'/')))
                || (path.len() >= 3
                    && path.as_bytes()[0].is_ascii_alphabetic()
                    && path.as_bytes()[1] == b':'
                    && matches!(path.as_bytes()[2], b'\\' | b'/'));
            // 冒号后提取行号
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let line_str = &content[i + 1..j];
            // `-数字` 区间结束
            let mut end: Option<usize> = None;
            let mut consumed = j;
            if j < bytes.len() && bytes[j] == b'-' {
                let mut k = j + 1;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k > j + 1 {
                    end = content[j + 1..k].parse().ok();
                    consumed = k;
                }
            }
            if !path.is_empty()
                && path.contains('.')
                && !path.contains("//")
                && !path.starts_with('/')
                && !has_drive_prefix
                && has_valid_extension(path)
                && !line_str.is_empty()
                && let Ok(start) = line_str.parse::<usize>()
                && start > 0
            {
                out.push(Citation {
                    path: path.to_string(),
                    start,
                    end: end.unwrap_or(start).max(start),
                });
            }
            i = consumed;
        } else {
            i += 1;
        }
    }
    out
}

/// 校验引用：路径在项目根下存在 + 结束行号不超过文件行数
///
/// root 为项目根（相对路径的解析基准）；文件读取失败（非 UTF-8、
/// 权限等）按无效处理——引用目标必须可读才可验证。
pub fn validate_citations(root: &Path, content: &str) -> Vec<InvalidCitation> {
    let mut invalid = Vec::new();
    for citation in extract_citations(content) {
        // .. 段可逃逸项目根（../src/x.rs）或跳过目录层级（src/../lib.rs），
        // 即使目标文件真实存在也按无效处理——引用路径必须位于项目根内
        if citation.path.split(['/', '\\']).any(|seg| seg == "..") {
            invalid.push(InvalidCitation {
                citation,
                reason: "路径含越界段 ..".to_string(),
            });
            continue;
        }
        let abs = root.join(&citation.path);
        let total_lines = std::fs::read_to_string(&abs)
            .map(|s| s.lines().count())
            .ok();
        let reason = match total_lines {
            None => "引用文件不存在或不可读".to_string(),
            Some(n) if citation.end > n => {
                format!("行号越界: {}-{} 超出文件总行数 {}", citation.start, citation.end, n)
            }
            _ => continue,
        };
        invalid.push(InvalidCitation { citation, reason });
    }
    invalid
}

/// 将无效引用列表格式化为重试反馈文本（注入 LLM 输入）
pub fn retry_feedback(invalid: &[InvalidCitation]) -> String {
    let mut lines = String::from("上一版输出存在无效的源码引用，请修正后重新输出完整文档：\n");
    for item in invalid {
        lines.push_str(&format!(
            "- `{}:{}` — {}\n",
            item.citation.path, item.citation.start, item.reason
        ));
    }
    lines.push_str(
        "要求：提及任何具体函数/结构体/文件时，必须携带真实存在的 `相对路径:行号` \
         （如 `src/fs.rs:28`）或 `相对路径:起始行-结束行`（如 `src/fs.rs:28-45`）引用；\
         不得编造不存在的文件或行号。",
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_single_line() {
        let text = "见 src/fs.rs:28 的实现";
        let cites = extract_citations(text);
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].path, "src/fs.rs");
        assert_eq!(cites[0].start, 28);
        assert_eq!(cites[0].end, 28);
    }

    #[test]
    fn test_extract_range() {
        let text = "核心逻辑在 src/generate/mod.rs:164-179";
        let cites = extract_citations(text);
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].path, "src/generate/mod.rs");
        assert_eq!(cites[0].start, 164);
        assert_eq!(cites[0].end, 179);
    }

    #[test]
    fn test_ignore_urls_and_times() {
        // URL 与时间格式不得误报
        let text = "见 https://example.com/a.rs:10，发生在 12:30 分";
        let cites = extract_citations(text);
        assert!(cites.is_empty(), "URL/时间格式不得误报: {:?}", cites);
    }

    #[test]
    fn test_ignore_absolute_path_and_zero_line() {
        assert!(extract_citations("绝对路径 /usr/bin/x.rs:5").is_empty());
        assert!(extract_citations("零行号 src/a.rs:0").is_empty());
    }

    #[test]
    fn test_multiple_citations() {
        let text = "a.rs:3 与 src/b.rs:10-12 和 docs/c.md:1";
        let cites = extract_citations(text);
        assert_eq!(cites.len(), 3);
    }

    #[test]
    fn test_windows_path_separator() {
        // render_api_reference 在 Windows 上输出 src\auth.rs:2（平台分隔符）
        let cites = extract_citations("见 src\\auth.rs:2 的实现");
        assert_eq!(cites.len(), 1, "反斜杠路径应完整提取: {:?}", cites);
        assert_eq!(cites[0].path, "src\\auth.rs");
        assert_eq!(cites[0].start, 2);
        // Windows 绝对路径（盘符）不得误报：C: 无点前缀
        assert!(extract_citations("在 C:\\repo\\x.rs:5 中").is_empty(), "盘符绝对路径应忽略");
    }

    #[test]
    fn test_forward_slash_drive_prefix() {
        // 正斜杠盘符（C:/...）与反斜杠盘符（C:\...）都不得提取；
        // 连字符前缀剔除后路径以盘符开头（-C:/repo/x.rs）的情形也要拦截
        assert!(extract_citations("见 C:/repo/x.rs:5 的实现").is_empty(), "正斜杠盘符应忽略");
        assert!(extract_citations("-C:/repo/x.rs:5").is_empty(), "连字符+盘符应忽略");
        assert!(extract_citations("在 C:\\repo\\x.rs:5 中").is_empty(), "反斜杠盘符应忽略");
    }

    #[test]
    fn test_version_numbers_not_extracted() {
        // 版本号/时刻格式（v2.0、1.2）的最后一段扩展名是纯数字，不得误报；
        // src/v1.5.rs 的扩展名是 .rs，应正常提取
        assert!(extract_citations("版本 v2.0:10 发布").is_empty(), "v2.0 是版本号");
        assert!(extract_citations("时刻 1.2:30 记录").is_empty(), "1.2 是时刻");
        let cites = extract_citations("见 src/v1.5.rs:3 的实现");
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].path, "src/v1.5.rs");
        assert_eq!(cites[0].start, 3);
    }

    #[test]
    fn test_leading_hyphen_stripped() {
        // 列表项形态 `-src/fs.rs`：前导连字符是列表前缀，剔除后正常提取；
        // 文件名中间的连字符（my-file.rs）不得误删
        let cites = extract_citations("-src/fs.rs:10");
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].path, "src/fs.rs");
        let cites = extract_citations("见 my-file.rs:3 的实现");
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].path, "my-file.rs");
    }

    #[test]
    fn test_dotdot_paths_rejected_in_validate() {
        // .. 段可逃逸项目根（../src/x.rs）或跳过目录层级（src/../lib.rs）：
        // 提取层保持完整路径不剔除，校验层一律拒绝，即使目标文件真实存在
        let dir = std::env::temp_dir().join(format!("repo_wiki_dotdot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("lib.rs"), "line1\n").unwrap();
        std::fs::write(dir.join("src").join("x.rs"), "line1\n").unwrap();

        let cites = extract_citations("见 ../src/x.rs:5 与 src/../lib.rs:3");
        assert_eq!(cites.len(), 2, "提取层应保留 .. 路径: {:?}", cites);
        assert_eq!(cites[0].path, "../src/x.rs");
        assert_eq!(cites[1].path, "src/../lib.rs");

        let invalid = validate_citations(&dir, "见 ../src/x.rs:5 与 src/../lib.rs:3");
        assert_eq!(invalid.len(), 2, "校验层应拒绝 .. 路径: {:?}", invalid);
        for item in &invalid {
            assert!(item.reason.contains("越界段 .."), "原因应说明越界: {:?}", item);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_ok_and_missing() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_cite_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("a.rs"), "line1\nline2\nline3\n").unwrap();

        let invalid = validate_citations(&dir, "见 src/a.rs:3");
        assert!(invalid.is_empty(), "存在的文件与合法行号应通过: {:?}", invalid);

        let invalid = validate_citations(&dir, "见 src/a.rs:9");
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].citation.path, "src/a.rs");
        assert!(invalid[0].reason.contains("越界"));

        let invalid = validate_citations(&dir, "见 src/missing.rs:1");
        assert_eq!(invalid.len(), 1);
        assert!(invalid[0].reason.contains("不存在"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_retry_feedback_lists_all() {
        let invalid = vec![InvalidCitation {
            citation: Citation { path: "src/a.rs".into(), start: 99, end: 99 },
            reason: "行号越界".into(),
        }];
        let feedback = retry_feedback(&invalid);
        assert!(feedback.contains("src/a.rs:99"));
        assert!(feedback.contains("行号越界"));
        assert!(feedback.contains("重新输出完整文档"));
    }
}
