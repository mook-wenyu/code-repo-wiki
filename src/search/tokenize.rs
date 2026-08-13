//! CJK 检索关键词切分（文本引擎与评测共用的公共逻辑）
//!
//! FTS5 默认分词器 unicode61 把连续汉字当作单一 token（CJK 属于 Lo 类别，
//! 无词边界信息），导致中文子串检索必然零命中。本模块提供统一的 CJK
//! 2-gram 切分，供：
//! - 搜索侧写入/查询（src/search/store.rs）：写入时把 CJK 段切成 2-gram
//!   存入 FTS5 tokens 列，查询时做同构展开，恢复中文子串命中能力；
//! - 评测侧（src/bench/mod.rs）：从需求文本提取关键词做文档检索。
//!
//! 单一真源：搜索与评测必须共用同一套切分逻辑，否则评测与实现不对称
//! （v35 审计发现：bench 侧早已为 CJK 做 2-gram 而搜索侧没有）。

/// 判断是否为 CJK 表意文字（统一表意/扩展 A/兼容区；2-gram 切分只对
/// 汉字有意义，日文假名等非汉字字形不切分，按非 CJK 字符处理）
pub(crate) fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF)
}

/// 从文本提取检索关键词：CJK 连续串按滑动窗口 2-gram 切分
/// （如「安装配置指南」→ 安装/装配/配置/置指/指南；单字不成词，
/// 2-gram 覆盖绝大多数中文术语），英文词/数字串保留原样
/// （"GPT-4" 拆为 "GPT"/"4" 两个关键词）。空串/纯标点返回空 Vec，
/// 调用方按「无关键词可检索」退化处理。
///
/// 确定性契约：输入相同输出必相同（保序），增量与全量路径共用
/// 同一切分才能保证索引一致。
pub(crate) fn extract_keywords(text: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let mut ascii_run = String::new();
    let flush_cjk = |run: &mut Vec<char>, out: &mut Vec<String>| {
        for w in run.windows(2) {
            out.push(w.iter().collect());
        }
        run.clear();
    };
    let flush_ascii = |run: &mut String, out: &mut Vec<String>| {
        if !run.is_empty() {
            out.push(std::mem::take(run));
        }
    };
    for c in text.chars() {
        if is_cjk(c) {
            flush_ascii(&mut ascii_run, &mut keywords);
            cjk_run.push(c);
        } else if c.is_ascii_alphanumeric() {
            flush_cjk(&mut cjk_run, &mut keywords);
            ascii_run.push(c);
        } else {
            flush_cjk(&mut cjk_run, &mut keywords);
            flush_ascii(&mut ascii_run, &mut keywords);
        }
    }
    flush_cjk(&mut cjk_run, &mut keywords);
    flush_ascii(&mut ascii_run, &mut keywords);
    keywords
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_keywords_cjk_bigrams() {
        // 连续汉字按滑动窗口 2-gram 切分
        let kws = extract_keywords("安装配置指南");
        assert_eq!(kws, vec!["安装", "装配", "配置", "置指", "指南"]);
    }

    #[test]
    fn test_extract_keywords_mixed() {
        // 英文词/数字串保留原样；混排时两段互不干扰
        let mixed = extract_keywords("支持 Setup v2 认证");
        assert!(mixed.contains(&"Setup".to_string()));
        assert!(mixed.contains(&"v2".to_string()));
        assert!(mixed.contains(&"认证".to_string()));
    }

    #[test]
    fn test_extract_keywords_empty() {
        assert!(extract_keywords("").is_empty(), "空串返回空");
        assert!(
            extract_keywords("！！！---").is_empty(),
            "纯标点无关键词返回空"
        );
    }
}
