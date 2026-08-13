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

/// 围栏区间（fence 感知，U04/D7）：行首 ``` 开/闭的行号区间
///
/// 返回 `(start, end)` 字节偏移对，start 为围栏开行起点、end 为闭行
/// 终点（含换行）；文末未闭合的围栏覆盖到文末。仅行首围栏（可带前导
/// 空白）参与配对，代码内容里的 ``` 行视为闭合。
fn fence_ranges(content: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut in_fence = false;
    let mut fence_start = 0usize;
    let mut offset = 0usize;
    for line in content.split('\n') {
        let trimmed = line.trim_start();
        if in_fence {
            if trimmed.starts_with("```") {
                // 闭合行终点含换行符（split 后补回）
                ranges.push((fence_start, offset + line.len() + 1));
                in_fence = false;
            }
        } else if trimmed.starts_with("```") {
            in_fence = true;
            fence_start = offset;
        }
        offset += line.len() + 1;
    }
    // 文末未闭合：覆盖到文末（防引用被"伪闭合"漏掉）
    if in_fence {
        ranges.push((fence_start, offset));
    }
    ranges
}

/// 从文本中提取所有 `path:line` / `path:start-end` 引用
///
/// 规则（最小误报设计）：
/// - 路径必须含点（`src/fs.rs` 的 `.`），排除 `https:`、`C:` 等无点前缀
/// - 路径不含 `//`（排除 URL）且不以 `/` 开头（排除绝对路径，统一相对根）
/// - 冒号后必须紧跟数字；`-数字` 后缀视为区间结束
/// - 行号非零
/// - U04/D7：跳过代码围栏区间（markdown 反引号代码块内/示例代码里的
///   path:line 是代码不是引用——示例代码误报会触发引用契约重试耗尽整页
///   bail，降级后的 text 代码块若含 path:line 又触发 bad-citation，双重盲区）
pub fn extract_citations(content: &str) -> Vec<Citation> {
    let fences = fence_ranges(content);
    let bytes = content.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut fence_idx = 0usize;
    while i < bytes.len() {
        // 跳过当前 fence 区间（含未闭合到文末的情况）
        while fence_idx < fences.len() && i >= fences[fence_idx].1 {
            fence_idx += 1;
        }
        if fence_idx < fences.len() && i >= fences[fence_idx].0 {
            i = fences[fence_idx].1;
            continue;
        }
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

/// 单条引用的文件级校验（文件存在 + 行号不越界 + 路径不逃逸项目根），
/// 返回失败原因；校验通过返回 None。
///
/// 从 validate_citations 提取的共享判定（v14 B 组：文件级与区间级两级
/// 校验共用，避免区间校验重复文件读取逻辑）。
fn check_citation_file_level(
    root: &Path,
    citation: &Citation,
    line_counts: &mut std::collections::HashMap<std::path::PathBuf, Option<usize>>,
) -> Option<String> {
    // .. 段可逃逸项目根（../src/x.rs）或跳过目录层级（src/../lib.rs），
    // 即使目标文件真实存在也按无效处理——引用路径必须位于项目根内
    if citation.path.split(['/', '\\']).any(|seg| seg == "..") {
        return Some("路径含越界段 ..".to_string());
    }
    // Windows 根相对（`\foo`、`/foo`）与盘符相对（`C:foo`）形态：Path::
    // is_absolute() 对二者返回 false，root.join 会把路径引向 root 外（根相对
    // 替换 prefix、盘符相对整体替换 self）——与 lint 层 detect_path_escape 同一
    // 组件级判定（KNOWN-04：提取器已滤 `/` 开头与盘符绝对，但 `\foo`/`C:foo`
    // 会漏出，此处收敛拒掉）。Unix 不受影响（`\foo`/`C:foo` 是普通相对路径）。
    if crate::output::lint::is_root_relative_or_drive_relative(Path::new(&citation.path)) {
        return Some("路径为根相对或盘符相对形态（无法验证 containment）".to_string());
    }
    let abs = root.join(&citation.path);
    // P3-5：同一次校验中同一文件的多条引用只读一次——大文件+多条引用
    // 时避免逐条整文件 read_to_string（O(引用数×文件大小) → O(文件数×文件大小)）；
    // None 也缓存（文件不存在/不可读的结果复用，不再重复 IO）
    let total_lines = match line_counts.get(&abs) {
        Some(&n) => n,
        None => {
            let n = std::fs::read_to_string(&abs)
                .map(|s| s.lines().count())
                .ok();
            line_counts.insert(abs, n);
            n
        }
    };
    match total_lines {
        None => Some("引用文件不存在或不可读".to_string()),
        Some(n) if citation.end > n => {
            Some(format!("行号越界: {}-{} 超出文件总行数 {}", citation.start, citation.end, n))
        }
        _ => None,
    }
}

/// 校验引用：路径在项目根下存在 + 结束行号不超过文件行数
///
/// root 为项目根（相对路径的解析基准）；文件读取失败（非 UTF-8、
/// 权限等）按无效处理——引用目标必须可读才可验证。
pub fn validate_citations(root: &Path, content: &str) -> Vec<InvalidCitation> {
    let mut line_counts: std::collections::HashMap<std::path::PathBuf, Option<usize>> =
        std::collections::HashMap::new();
    extract_citations(content)
        .into_iter()
        .filter_map(|citation| {
            check_citation_file_level(root, &citation, &mut line_counts)
                .map(|reason| InvalidCitation { citation, reason })
        })
        .collect()
}

/// 文件 → 实体行区间列表 映射（v14 B 组区间重叠校验的输入形态）
///
/// 键约定：norm_sep 归一化路径（generate 层 = 相对项目根路径；
/// lint 层 = 绝对路径；两处各自与自己的引用解析基准一致即可——
/// validate_citations_against_entities 内部对引用路径做 norm_sep 后
/// 查表，键必须与调用方构造时同基准）。
pub type EntityRanges = std::collections::HashMap<String, Vec<(usize, usize)>>;

/// 引用区间与实体行区间重叠判定（v14 B 组，t01 方案 A：区间算术）。
///
/// 引用 `path:start-end` 覆盖该文件的任一实体区间即有效：
/// `start <= entity.end && end >= entity.start`（闭区间相交判定，
/// 边界触碰（start == entity.end 或 end == entity.start）算覆盖——
/// 引用区间只需触碰实体边界，不要求完整落在定义范围内；行号完全
/// 落在实体间隙（不触任一实体边界）才算未覆盖）。
///
/// 纯函数（无 I/O），测试直接构造区间。
pub fn citation_overlaps_entity(c: &Citation, ranges: &[(usize, usize)]) -> bool {
    ranges
        .iter()
        .any(|&(entity_start, entity_end)| c.start <= entity_end && c.end >= entity_start)
}

/// 文件级 + 区间级两级引用校验（v14 B 组）
///
/// `entity_ranges` 键为 norm_sep 归一化后的相对路径（Windows 反斜杠与
/// 引用提取的正斜杠必须统一后再比较，否则 Windows 上恒不命中），
/// 值为该文件的实体行区间列表（line_start..=line_end）。
///
/// 关键边界（t03 拍板）：**实体表无该文件键的引用放行**——README.md、
/// 配置文件等非代码文件没有 AST 实体，引用它们（文档引用配置/说明）
/// 是合法行为，区间校验只对"文件有实体但引用区间不覆盖任何实体"判
/// 无效（这正是原校验漏掉的最大缺陷类：行号对但内容错——引用 A 函数
/// 写成 B 行的位置）。
pub fn validate_citations_against_entities(
    root: &Path,
    content: &str,
    entity_ranges: &EntityRanges,
) -> Vec<InvalidCitation> {
    // P3-5：文件级 + 区间级两级校验共用一份行数缓存——先逐引用做文件级
    // 校验（同文件只 read_to_string 一次），无效引用进 invalid，有效引用
    // 进 file_level_valid 供区间校验；与 validate_citations 的判定逻辑
    // 等价，但避免两遍遍历各自重复读取文件
    let mut line_counts: std::collections::HashMap<std::path::PathBuf, Option<usize>> =
        std::collections::HashMap::new();
    let mut invalid = Vec::new();
    let mut file_level_valid: Vec<Citation> = Vec::new();
    for citation in extract_citations(content) {
        match check_citation_file_level(root, &citation, &mut line_counts) {
            Some(reason) => invalid.push(InvalidCitation { citation, reason }),
            None => file_level_valid.push(citation),
        }
    }
    for citation in file_level_valid {
        let key = crate::incremental::norm_sep(&citation.path);
        if let Some(ranges) = entity_ranges.get(&key)
            && !citation_overlaps_entity(&citation, ranges)
        {
            invalid.push(InvalidCitation {
                citation,
                reason: "引用区间未覆盖任何实体（行号可能指向错误位置）".to_string(),
            });
        }
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
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_dotdot_{}", std::process::id()));
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

    /// KNOWN-04：Windows 根相对 `\foo` 引用必须被校验层拒绝——is_absolute 对
    /// 其返回 false，root.join 会把路径引向 root 外（根相对替换 prefix）。
    /// 修复前提取层只滤 `/` 开头与盘符绝对，`\foo` 会漏出并被 root.join 引向
    /// root 外读取。盘符相对 `C:foo` 在提取层即被冒号截断（冒号非路径字符，
    /// 截为普通相对路径 foo.rs），不会逃逸 root，此处仅文档化该行为。Unix 上
    /// `\foo` 是普通相对路径，不适用。
    #[test]
    #[cfg(windows)]
    fn test_validate_rejects_root_relative_and_drive_relative() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_rootrel_cite_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 根相对 `\foo`：提取层保留完整形态，校验层必须拒绝
        let cites = extract_citations(r"见 \foo.rs:5 的实现");
        assert_eq!(cites.len(), 1, "提取层应保留根相对形态: {:?}", cites);
        assert_eq!(cites[0].path, r"\foo.rs");
        let invalid = validate_citations(&dir, r"见 \foo.rs:5 的实现");
        assert_eq!(invalid.len(), 1, "校验层应拒绝根相对: {:?}", invalid);
        assert!(
            invalid[0].reason.contains("根相对或盘符相对"),
            "原因应说明形态不可验证 containment: {:?}",
            invalid
        );

        // 盘符相对 `C:foo`：提取层在冒号处截断为普通相对路径，不会逃逸 root
        let cites = extract_citations("见 C:foo.rs:3 的实现");
        assert_eq!(cites[0].path, "foo.rs", "冒号截断为普通相对路径: {:?}", cites);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_ok_and_missing() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_cite_{}", std::process::id()));
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

    /// U04/D7：代码围栏内的 path:line 不应提取（示例代码是代码不是引用）
    #[test]
    fn test_extract_skips_fenced_code_blocks() {
        let text = "见 src/a.rs:1 的实现。\n\n```rust\nlet cfg = load(\"src/config.rs:99\");\n```\n";
        let cites = extract_citations(text);
        assert_eq!(cites.len(), 1, "围栏外应提取 1 条, 实际: {cites:?}");
        assert_eq!(cites[0].path, "src/a.rs");
    }

    /// U04/D7：mermaid 图内 label 里的 path:line 不应提取
    #[test]
    fn test_extract_skips_mermaid_blocks() {
        let text = "```mermaid\nflowchart LR\nA --> |src/fs.rs:28| B\n```\n正文 src/b.rs:1\n";
        let cites = extract_citations(text);
        assert_eq!(cites.len(), 1, "mermaid 块内不应提取, 实际: {cites:?}");
        assert_eq!(cites[0].path, "src/b.rs");
    }

    /// U04/D7：未闭合围栏覆盖到文末——其后内容全部跳过（防伪闭合漏检）
    #[test]
    fn test_extract_unclosed_fence_skips_to_end() {
        let text = "```rust\nlet x = load(\"src/a.rs:1\");\n正文 src/b.rs:2\n";
        let cites = extract_citations(text);
        assert!(cites.is_empty(), "未闭合围栏后不应提取: {cites:?}");
    }

    /// U04/D7：正文-代码-正文交替时只提取正文引用
    #[test]
    fn test_extract_alternating_fences() {
        let text = "正文一 src/a.rs:1\n```rust\nsrc/x.rs:2\n```\n正文二 src/b.rs:3\n```text\nsrc/y.rs:4\n```\n正文三 src/c.rs:5\n";
        let cites = extract_citations(text);
        let paths: Vec<&str> = cites.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs", "src/c.rs"], "只应提取正文引用: {paths:?}");
    }

    /// U04/D7：缩进围栏（前导空白）同样识别
    #[test]
    fn test_extract_indented_fence() {
        let text = "正文 src/a.rs:1\n    ```rust\n    src/b.rs:2\n    ```\n";
        let cites = extract_citations(text);
        assert_eq!(cites.len(), 1, "缩进围栏内部不应提取, 实际: {cites:?}");
        assert_eq!(cites[0].path, "src/a.rs");
    }

    /// v14 B 组：引用区间与实体行区间的重叠判定（闭区间相交，
    /// 边界触碰不算覆盖——引用行号必须真正落在实体定义范围内）
    #[test]
    fn test_citation_overlaps_entity() {
        let ranges = vec![(10usize, 20usize), (30, 40)];
        let c = |start: usize, end: usize| Citation { path: "x.rs".into(), start, end };
        // 完全覆盖 / 部分重叠 / 单行落在区间内
        assert!(citation_overlaps_entity(&c(12, 18), &ranges));
        assert!(citation_overlaps_entity(&c(8, 15), &ranges), "跨入区间应算覆盖");
        assert!(citation_overlaps_entity(&c(15, 25), &ranges), "跨出区间应算覆盖");
        assert!(citation_overlaps_entity(&c(10, 10), &ranges), "起点即实体起点应覆盖");
        assert!(citation_overlaps_entity(&c(20, 20), &ranges), "终点即实体终点应覆盖");
        assert!(citation_overlaps_entity(&c(35, 35), &ranges), "第二个实体单行");
        // 不覆盖：区间之间空隙、区间前/后、边界外一行
        assert!(!citation_overlaps_entity(&c(21, 29), &ranges), "实体间隙不应覆盖");
        assert!(!citation_overlaps_entity(&c(1, 5), &ranges), "实体之前不应覆盖");
        assert!(!citation_overlaps_entity(&c(41, 50), &ranges), "实体之后不应覆盖");
        assert!(!citation_overlaps_entity(&c(21, 21), &ranges), "紧邻实体终点外一行不覆盖");
        // 空区间表
        assert!(!citation_overlaps_entity(&c(10, 10), &[]));
    }

    /// v14 B 组：两级校验（文件级 + 区间重叠）——文件存在且行号有效但
    /// 引用区间不覆盖任何实体 = 新捕获的缺陷类（行号对但内容错）
    #[test]
    fn test_validate_against_entities_overlap() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_cite_ovl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        // 10 行文件：实体区间 (2,4) 与 (7,9)（如 fn a 在 2-4 行、fn b 在 7-9 行）
        let src = dir.join("src").join("a.rs");
        let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(&src, content).unwrap();
        // README.md 无实体（非代码文件）
        std::fs::write(dir.join("README.md"), "docs\n").unwrap();

        let mut ranges: EntityRanges = EntityRanges::new();
        ranges.insert("src/a.rs".to_string(), vec![(2, 4), (7, 9)]);

        // 覆盖实体 → 通过
        let invalid = validate_citations_against_entities(&dir, "见 src/a.rs:3", &ranges);
        assert!(invalid.is_empty(), "覆盖实体应通过: {invalid:?}");
        let invalid = validate_citations_against_entities(&dir, "见 src/a.rs:7-9", &ranges);
        assert!(invalid.is_empty(), "区间覆盖第二个实体应通过: {invalid:?}");
        // 文件存在 + 行号有效但区间不覆盖实体 → 无效（新缺陷类）
        let invalid = validate_citations_against_entities(&dir, "见 src/a.rs:5-6", &ranges);
        assert_eq!(invalid.len(), 1, "实体间隙引用应无效: {invalid:?}");
        assert!(invalid[0].reason.contains("未覆盖任何实体"), "原因应说明: {:?}", invalid[0]);
        let invalid = validate_citations_against_entities(&dir, "见 src/a.rs:1", &ranges);
        assert_eq!(invalid.len(), 1, "实体之前引用应无效: {invalid:?}");
        // 无实体文件（README）→ 放行（区间校验只对有实体的文件生效）
        let invalid = validate_citations_against_entities(&dir, "见 README.md:1", &ranges);
        assert!(invalid.is_empty(), "无实体文件引用应放行: {invalid:?}");
        // 文件级错误仍被捕获（越界）
        let invalid = validate_citations_against_entities(&dir, "见 src/a.rs:99", &ranges);
        assert_eq!(invalid.len(), 1);
        assert!(invalid[0].reason.contains("越界"));
        // Windows 反斜杠键形态：表键为 src\a.rs（norm_sep 后应命中）
        let mut win_ranges: EntityRanges = EntityRanges::new();
        win_ranges.insert("src\\a.rs".to_string(), vec![(2, 4)]);
        let invalid = validate_citations_against_entities(&dir, "见 src/a.rs:3", &win_ranges);
        assert!(invalid.is_empty(), "反斜杠表键经 norm_sep 应命中: {invalid:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
