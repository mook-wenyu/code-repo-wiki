//! 结构感知代码块——语义索引的最小单元
//!
//! 语义嵌入/检索从「裸实体源码片段」升级为「结构感知块」（v0.7.2）：
//! 每个块承载模块路径、文件行、可见性、签名、doc 首段与
//! body 源码，嵌入文本即「模块路径 + 签名 + body」的拼接——对应 T2
//! 最佳实践：避免裸函数体（裸 body 丢失语义上下文，向量退化为词袋）。
//!
//! 块由 chunker（src/search/chunker.rs）按 tree-sitter 顶层定义节点切分，
//! 本文件只定义数据结构与嵌入文本构造。

use crate::model::NodeKind;

/// 嵌入预算：块文本总长上限（字符）。
///
/// qwen3.7-text-embedding 支持 131K token 上下文，但保守起见按 8000
/// 字符（约 2K token）预截断——中文 1 字≈1 token、英文约 4 字符≈1 token，
/// 8000 字符对中英文都不超模型上下文；超大块 body 取头 + `…` + 告警
/// （见 chunker 的嵌入文本构造）。embed.rs 的 EMBED_MAX_INPUT_CHARS 是
/// API 层最终安全网，两处同值 8000 保持一致。
pub const BLOCK_TEXT_MAX_CHARS: usize = 8000;

/// 签名截断上限（块文本中签名的长度天花板，防超长 where 子句/模板参数
/// 稀释 name/scope 的信号强度）
const SIGNATURE_MAX_CHARS: usize = 160;

/// doc 首段截断上限（注释首段超过此长度只保留头部——首段是语义最浓部分）
const DOC_MAX_CHARS: usize = 500;

/// 结构感知代码块
#[derive(Debug, Clone)]
pub struct Block {
    /// 全局唯一块 ID（`{file_path}#{start}-{end}`，作 vec0 `block_id` 列键；
    /// 同文件顶层定义不重叠，路径 + 行范围唯一）
    pub id: String,
    /// 相对项目根路径
    pub file_path: String,
    pub language: String,
    /// 模块路径（目录段 + 文件名 stem，与图 CodeNode.module_path 同派生规则）
    pub module_path: Vec<String>,
    pub kind: NodeKind,
    pub name: String,
    /// 1-based 行范围 [start, end]
    pub line_range: (usize, usize),
    /// 声明头（body 字段之前的部分，如 `pub fn foo(a: i32)`）
    pub signature: String,
    pub visibility: Option<String>,
    pub doc_comment: Option<String>,
    /// 嵌入文本（模块路径 + 文件行 + 可见性 + 签名 + doc + body）
    pub text: String,
    /// 源码级实体引用（与图 CodeNode 关联的轻量键）
    pub entity: EntityRef,
}

/// 源码级实体引用：块在文件中的身份键，供 lib.rs 将块与图 CodeNode 关联
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRef {
    pub name: String,
    pub file_path: String,
    pub line_range: (usize, usize),
}

/// 构造嵌入文本：`模块路径::name kind [file:start-end] (vis)` +
/// 签名（≤160 字符）+ doc 首段 + body（整体截断到 BLOCK_TEXT_MAX_CHARS）。
///
/// 结构组成与 T2「作用域前缀 + 签名 + body」一致：模块路径与 name 打头
/// 提供检索定位信号，签名携带 API 形状，doc 提供语义描述，body 供
/// 「长函数中段 token」也能被向量命中。超大块整体截断天然保留头部
/// （函数头/签名语义密度最高）。
///
/// 参数较多（11 个）：每个参数都是嵌入文本的一个独立组成部分，无自然
/// 归组（组结构会引入只为一处调用服务的中间类型），保留全参签名。
#[allow(clippy::too_many_arguments)]
pub fn build_embed_text(
    module_path: &[String],
    name: &str,
    kind: NodeKind,
    scope: &[String],
    file_path: &str,
    line_start: usize,
    line_end: usize,
    visibility: Option<&str>,
    signature: &str,
    doc: Option<&str>,
    body: &str,
) -> String {
    let mut out = String::new();
    if !module_path.is_empty() {
        out.push_str(&module_path.join("::"));
        out.push_str("::");
    }
    out.push_str(name);
    out.push_str(&format!(" {kind:?}"));
    if !scope.is_empty() {
        out.push_str(&format!(" scope={}", scope.join("::")));
    }
    out.push_str(&format!(" [{file_path}:{line_start}-{line_end}]"));
    if let Some(v) = visibility.filter(|v| !v.is_empty()) {
        out.push_str(&format!(" ({v})"));
    }
    if !signature.is_empty() {
        out.push('\n');
        out.push_str(&truncate_chars(signature, SIGNATURE_MAX_CHARS));
    }
    if let Some(d) = doc.filter(|d| !d.is_empty()) {
        out.push('\n');
        out.push_str(&truncate_chars(d, DOC_MAX_CHARS));
    }
    out.push('\n');
    out.push_str(body);
    // 预算截断：取头 + 省略号（防 API 拒收；embed.rs 还有最终安全网）
    let total = out.chars().count();
    if total > BLOCK_TEXT_MAX_CHARS {
        let truncated = truncate_chars(&out, BLOCK_TEXT_MAX_CHARS);
        tracing::warn!(
            "块文本超 {BLOCK_TEXT_MAX_CHARS} 字符（{total}），已截断取头: {file_path}:{line_start}-{line_end} ({name})"
        );
        format!("{truncated}…")
    } else {
        out
    }
}

/// 按字符数截断（UTF-8 安全：先取 chars 再收集）
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embed_text_includes_scope_and_signature_and_body() {
        let text = build_embed_text(
            &["src".into(), "auth".into(), "user".into()],
            "login",
            NodeKind::Function,
            &[],
            "src/auth/user.rs",
            10,
            20,
            Some("pub"),
            "pub fn login(name: &str)",
            Some("验证用户登录"),
            "let ok = check(name);\nok",
        );
        assert!(text.starts_with("src::auth::user::login Function [src/auth/user.rs:10-20] (pub)"));
        assert!(text.contains("pub fn login(name: &str)"));
        assert!(text.contains("验证用户登录"));
        assert!(text.contains("let ok = check(name);"));
    }

    #[test]
    fn test_embed_text_scope_chain_when_present() {
        let text = build_embed_text(
            &["src".into()],
            "handle",
            NodeKind::Function,
            &["Router".into(), "Api".into()],
            "src/router.rs",
            1,
            5,
            None,
            "fn handle()",
            None,
            "{}",
        );
        assert!(
            text.contains(" scope=Router::Api"),
            "作用域链应入文本: {text}"
        );
    }

    #[test]
    fn test_embed_text_truncates_oversized_body_keeps_head() {
        let long_body = "x".repeat(BLOCK_TEXT_MAX_CHARS + 100);
        let text = build_embed_text(
            &[],
            "huge",
            NodeKind::Function,
            &[],
            "src/huge.rs",
            1,
            2,
            None,
            "fn huge()",
            None,
            &long_body,
        );
        assert!(text.ends_with('…'), "超预算应加省略号");
        assert!(
            text.chars().count() <= BLOCK_TEXT_MAX_CHARS + 1,
            "截断后不超上限+省略号"
        );
        assert!(
            text.starts_with("huge Function"),
            "头部保留（语义密度最高处）"
        );
    }

    #[test]
    fn test_embed_text_signature_truncated_to_160() {
        let long_sig = "fn f(".to_string() + &"a".repeat(200) + ")";
        let text = build_embed_text(
            &[],
            "f",
            NodeKind::Function,
            &[],
            "src/f.rs",
            1,
            2,
            None,
            &long_sig,
            None,
            "{}",
        );
        // 签名截断到 160 字符：前缀 "fn f(" 5 字符 + 155 个 a
        assert!(text.contains(&"a".repeat(155)), "签名头部保留");
        assert!(!text.contains(&"a".repeat(156)), "超长签名必须截断");
    }
}
