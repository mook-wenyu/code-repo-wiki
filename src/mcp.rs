//! MCP (Model Context Protocol) server（P1-3）
//!
//! 通过 stdio 暴露 repo-wiki 能力给任意 MCP 客户端（Claude Code、Cline、
//! 自研 Agent 等）：代码搜索、AST 符号查找、Wiki 页面/卡片读取、状态查询。
//! 实现基于官方 Rust MCP SDK（rmcp 3.x，crates.io 维护）。
//!
//! 设计：server 无状态（每次工具调用现场加载配置与索引），与 CLI 共享
//! 同一 lib 入口（execute_search / execute_ast_search），不复制业务逻辑。
//! 项目根由 `--root` 参数指定（缺省解析 cwd 的 .repo-wiki.toml 项目级配置）。

use std::path::{Path, PathBuf};

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::service::{QuitReason, ServiceExt};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};

use crate::project::ProjectRoot;

/// MCP server：工具路由 + 配置/根注入
#[derive(Debug, Clone)]
pub struct RepoWikiMcp {
    /// 工具路由（tool_handler 宏访问）
    #[expect(dead_code, reason = "tool_handler 宏访问此路由字段")]
    tool_router: ToolRouter<Self>,
    /// 配置文件路径（.repo-wiki.toml 项目级、全局或 --config 指定）
    config_path: Option<PathBuf>,
    /// 项目根（代码扫描/git 定位基准）
    root: ProjectRoot,
}

impl RepoWikiMcp {
    /// 创建 server
    pub fn new(config_path: Option<PathBuf>, root: ProjectRoot) -> Self {
        Self {
            tool_router: Self::tool_router(),
            config_path,
            root,
        }
    }
}

// ============ 工具定义（#[tool_router] 块内，方法可访问 self 配置） ============

/// 搜索请求参数
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchRequest {
    /// 搜索关键词，如 "config 加载"
    query: String,
    /// 返回结果数量（默认取配置 search.default_top_k）
    top_k: Option<usize>,
    /// 搜索引擎: text / semantic / hybrid（默认取配置文件 default_engine）
    engine: Option<String>,
}

/// AST 查找请求参数
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AstSearchRequest {
    /// 要查找的符号名（函数/结构体/trait/类等）
    symbol: String,
    /// 源语言（rust/python/go/...）；省略时按文件扩展名自动推断
    language: Option<String>,
}

/// 读页面请求参数
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadPageRequest {
    /// 页面文件名（不含 .md），如 src_config、architecture、overview、api
    page: String,
    /// 语言目录（默认取配置 wiki.language）
    lang: Option<String>,
}

/// 读卡片请求参数
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadCardRequest {
    /// 卡片名（模块名），如 src_config、crate_net
    card: String,
    /// 语言目录（默认取配置 wiki.language）
    lang: Option<String>,
}

#[tool_router(router = tool_router)]
impl RepoWikiMcp {
    /// 搜索代码实体：按关键词返回匹配的函数/结构体/类及文件位置（text/semantic/hybrid 引擎）
    #[tool(description = "搜索代码实体：按关键词返回匹配的函数/结构体/类及文件位置（text/semantic/hybrid 引擎，与 CLI repo-wiki search 等价）")]
    async fn search(&self, Parameters(SearchRequest { query, top_k, engine }): Parameters<SearchRequest>) -> String {
        // 配置完整性检查：搜索前确认配置可加载（错误早暴露）；v22 起
        // 引擎/条数默认值硬编码，配置内容不再被本函数使用
        let _config = match crate::config::resolve_mcp_config(self.config_path.as_deref(), &self.root) {
            Ok(c) => c,
            Err(e) => return format!("配置加载失败: {e}"),
        };
        let engine_type = match engine.as_deref() {
            Some("text") => crate::config::schema::SearchEngineType::Text,
            Some("semantic") => crate::config::schema::SearchEngineType::Semantic,
            Some("hybrid") => crate::config::schema::SearchEngineType::Hybrid,
            Some(other) => return format!("不支持的搜索引擎: {other}（可选: text/semantic/hybrid）"),
            None => crate::config::schema::SEARCH_DEFAULT_ENGINE,
        };
        let top_k = clamp_top_k(top_k.unwrap_or(crate::config::schema::SEARCH_DEFAULT_TOP_K));
        match crate::execute_search(self.config_path.as_deref(), &self.root, &query, top_k, &engine_type) {
            Ok(hits) if hits.is_empty() => "未找到匹配结果".to_string(),
            Ok(hits) => {
                let mut out = format!("找到 {} 个结果:\n", hits.len());
                for (i, hit) in hits.iter().enumerate() {
                    let file = hit.node.file_path.as_deref().unwrap_or("-");
                    let loc = match hit.node.line_range {
                        Some((s, e)) => format!("{file}:{s}-{e}"),
                        None => file.to_string(),
                    };
                    let sig = hit.node.signature.as_deref().unwrap_or(&hit.node.name);
                    out.push_str(&format!("{}. `{sig}` — {loc}\n", i + 1));
                }
                out
            }
            Err(e) => format!("搜索失败: {e}"),
        }
    }

    /// AST 精确符号查找：扫描源文件定位 函数/结构体/类 定义的 文件+行号+签名（不依赖搜索索引）
    #[tool(description = "AST 精确符号查找：扫描源文件定位函数/结构体/类定义的 文件+行号+签名（与 CLI repo-wiki ast-search 等价）")]
    async fn ast_search(&self, Parameters(AstSearchRequest { symbol, language }): Parameters<AstSearchRequest>) -> String {
        match crate::execute_ast_search(self.config_path.as_deref(), &self.root, &symbol, language.as_deref()) {
            Ok(hits) if hits.is_empty() => format!("未找到符号 \"{symbol}\" 的定义"),
            Ok(hits) => {
                let mut out = format!("找到 {} 个定义:\n", hits.len());
                for (i, hit) in hits.iter().enumerate() {
                    let sig = hit.node.signature.as_deref().unwrap_or(&hit.node.name);
                    let loc = match (&hit.node.file_path, hit.node.line_range) {
                        (Some(f), Some((s, e))) => format!("{f}:{s}-{e}"),
                        (Some(f), None) => f.clone(),
                        _ => "(unknown)".to_string(),
                    };
                    out.push_str(&format!("{}. {sig} — {loc}\n", i + 1));
                }
                out
            }
            Err(e) => format!("符号查找失败: {e}"),
        }
    }

    /// 读取已生成的 Wiki 页面内容（模块页/架构概览/项目概览/api）
    #[tool(description = "读取已生成的 Wiki 页面内容（wiki/{lang}/{page}.md，如 src_config、architecture、overview、api）")]
    async fn read_wiki_page(&self, Parameters(ReadPageRequest { page, lang }): Parameters<ReadPageRequest>) -> String {
        let config = match crate::config::resolve_mcp_config(self.config_path.as_deref(), &self.root) {
            Ok(c) => c,
            Err(e) => return format!("配置加载失败: {e}"),
        };
        let lang = lang.unwrap_or_else(|| config.wiki.language.clone());
        // 语言目录净化（S1，工具暴露给任意 Agent）：lang 直接 join 进产物
        // 路径，未净化时 `../..` 可穿越到 output_dir 之外读取任意 .md 文件
        //（曾实测复现）。与 page 同规则但更严：语言目录名只允许
        // [A-Za-z0-9_-] 单段（zh、en、zh-CN 等），拒绝一切路径分隔符与
        // 绝对路径形态——校验失败明确报错，不读盘。
        if let Err(e) = validate_lang_segment(&lang) {
            return e;
        }
        // 参数净化（工具暴露给任意 Agent）：拒绝路径穿越与绝对路径，
        // 只允许单段文件名（页面名），防读取 output_dir 之外任意文件
        if page.contains('/') || page.contains('\\') || page.contains("..") || Path::new(&page).is_absolute() {
            return format!("非法的页面名: {page}（只允许单段文件名）");
        }
        let path = std::path::Path::new(&config.output.dir)
            .join("wiki")
            .join(&lang)
            .join(format!("{page}.md"));
        match std::fs::read_to_string(&path) {
            Ok(content) => format!("{}\n\n{content}", path.display()),
            Err(e) => format!(
                "页面不存在或不可读（可先运行 repo-wiki generate 生成）: {}: {e}",
                path.display()
            ),
        }
    }

    /// 读取已生成的 Knowledge Card（AI 代理的结构化模块摘要）
    #[tool(description = "读取已生成的 Knowledge Card 内容（cards/{lang}/{card}.md）")]
    async fn read_card(&self, Parameters(ReadCardRequest { card, lang }): Parameters<ReadCardRequest>) -> String {
        let config = match crate::config::resolve_mcp_config(self.config_path.as_deref(), &self.root) {
            Ok(c) => c,
            Err(e) => return format!("配置加载失败: {e}"),
        };
        let lang = lang.unwrap_or_else(|| config.wiki.language.clone());
        // 语言目录净化（S1）：同 read_wiki_page，lang 直接 join 进路径，
        // 未净化可穿越读取 output_dir 之外任意 .md（实测复现）。
        if let Err(e) = validate_lang_segment(&lang) {
            return e;
        }
        // 同 read_wiki_page：净化路径穿越
        if card.contains('/') || card.contains('\\') || card.contains("..") || Path::new(&card).is_absolute() {
            return format!("非法的卡片名: {card}（只允许单段文件名）");
        }
        let path = std::path::Path::new(&config.output.dir)
            .join("cards")
            .join(&lang)
            .join(format!("{card}.md"));
        match std::fs::read_to_string(&path) {
            Ok(content) => format!("{}\n\n{content}", path.display()),
            Err(e) => format!(
                "卡片不存在或不可读（可先运行 repo-wiki generate 生成）: {}: {e}",
                path.display()
            ),
        }
    }

    /// 查看 Wiki 生成状态：页面/卡片数量与 lint 健康检查结果
    #[tool(description = "查看 Wiki 生成状态：页面/卡片数量与产物健康检查（孤儿页/断链/过时/引用）")]
    async fn status(&self) -> String {
        let config = match crate::config::resolve_mcp_config(self.config_path.as_deref(), &self.root) {
            Ok(c) => c,
            Err(e) => return format!("配置加载失败: {e}"),
        };
        // MCP server 由项目内启动，root = 当前工作目录；
        // 源码根须相对 root 解析（见 commands::source_roots_from_include_rooted），
        // 否则跨 cwd 调用时 lint 会扫到错误目录
        let root = match crate::project::ProjectRoot::from_cwd() {
            Ok(r) => r,
            Err(e) => return format!("无法确定当前工作目录: {e}"),
        };
        let report = crate::commands::status_report(&config, &root);
        if !report.ready {
            return "Wiki 未生成（运行 repo-wiki generate 生成后可用）".to_string();
        }
        let mut out = format!("Wiki 就绪: {} 张页面, {} 张卡片\n", report.wiki_pages, report.cards);
        if report.issues.is_empty() {
            out.push_str("lint: 通过（无孤儿页/断链/过时/引用/覆盖问题）");
        } else {
            out.push_str(&format!("lint: 发现 {} 个问题:\n", report.issues.len()));
            for issue in &report.issues {
                out.push_str(&format!("- [{}] {}: {}\n", issue.kind, issue.path, issue.message));
            }
        }
        out
    }
}

#[tool_handler]
impl ServerHandler for RepoWikiMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

/// 启动 stdio MCP server（阻塞直到 stdin 关闭）
///
/// 客户端配置示例（opencode.json / claude_desktop_config.json）：
/// ```json
/// { "command": "repo-wiki", "args": ["mcp", "--root", "."] }
/// ```
pub async fn serve_stdio(config_path: Option<&Path>, root: ProjectRoot) -> Result<QuitReason> {
    let server = RepoWikiMcp::new(config_path.map(|p| p.to_path_buf()), root);
    let service = server.serve(rmcp::transport::stdio()).await?;
    Ok(service.waiting().await?)
}

/// 将 top_k 收敛到 1..=50
///
/// 为什么 clamp：MCP 工具响应是整条字符串直接回给 Agent 的，top_k 无上限时
/// 一次 search 调用可以把全部命中（几千行签名+文件路径）塞进单条响应，
/// 直接撑爆 Agent 上下文窗口。CLI 的 -k 无此限制（用户显式指定、自担后果），
/// MCP 是面向任意客户端的公共接口，必须从工具侧兜底。1 下限避免
/// top_k=0 时"返回 0 个结果"的无意义调用。
fn clamp_top_k(top_k: usize) -> usize {
    top_k.clamp(1, 50)
}

/// 语言目录名校验（S1：MCP lang 参数净化）
///
/// 只允许单段名（[A-Za-z0-9_-]，如 zh/en/zh-CN/zh_cn），拒绝一切其他字符——
/// 路径分隔符（/ \）、点段（..）、空格、盘符、绝对路径形态全部落入拒绝集。
/// 校验失败返回含"非法语言名"的错误串（工具直接回给 Agent），不读盘。
fn validate_lang_segment(lang: &str) -> Result<(), String> {
    if lang.is_empty() {
        return Err("非法语言名: （只允许 [A-Za-z0-9_-] 单段名）".to_string());
    }
    if lang
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(())
    } else {
        Err(format!("非法语言名: {lang}（只允许 [A-Za-z0-9_-] 单段名）"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// top_k 边界收敛：0/超大值收敛到 1/50，区间内原样保留
    #[test]
    fn test_clamp_top_k() {
        assert_eq!(clamp_top_k(0), 1);
        assert_eq!(clamp_top_k(1), 1);
        assert_eq!(clamp_top_k(50), 50);
        assert_eq!(clamp_top_k(9999), 50);
        assert_eq!(clamp_top_k(10), 10);
    }

    /// S1 语言目录名校验：合法单段名通过，穿越/分隔符/空串全部拒绝
    #[test]
    fn test_validate_lang_segment() {
        // 合法：语言目录名形态
        for ok in ["zh", "en", "zh-CN", "zh_cn", "EN", "pt-BR"] {
            assert!(validate_lang_segment(ok).is_ok(), "{ok} 应通过校验");
        }
        // 非法：路径穿越与分隔符形态（lang 直接 join 进产物路径的攻击面）
        for bad in [
            "", "..", "../..", "/", "//a", "a/b", "a\\b", "C:/x", "/abs", "zh..", "zh zh",
        ] {
            let err = validate_lang_segment(bad).unwrap_err();
            assert!(
                err.contains("非法语言名"),
                "{bad:?} 应被拒绝且报错可读: {err}"
            );
        }
    }
}
