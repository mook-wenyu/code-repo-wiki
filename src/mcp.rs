//! MCP (Model Context Protocol) server（P1-3）
//!
//! 通过 stdio 暴露 code-repo-wiki 能力给任意 MCP 客户端（Claude Code、Cline、
//! 自研 Agent 等）：代码搜索、AST 符号查找、Wiki 页面/卡片读取、状态查询。
//! 实现基于官方 Rust MCP SDK（rmcp 3.x，crates.io 维护）。
//!
//! 设计：server 无状态（每次工具调用现场加载配置与索引），与 CLI 共享
//! 同一 lib 入口（execute_search / execute_ast_search），不复制业务逻辑。
//! 项目根由 `--root` 参数指定（缺省解析 cwd 的 config.toml 项目级配置）。
//!
//! 工具返回类型（audit-out-08 → 已修复）：工具返回 `CallToolResult`，业务错误
//! 走 `CallToolResult::error(...)` 置 `isError=true`，成功与合法空结果走
//! `CallToolResult::success(...)`（isError=false）。AI 客户端据此程序化区分
//! 「工具成功但结果是错误消息」与真正的成功（此前错误以文本返回、isError 未
//! 置位，客户端无法区分）。错误文案仍为可读文本（含具体失败原因），与 CLI
//! 文本模式错误措辞一致。参数反序列化失败由 rmcp 路由层自动置 isError=true
//! （into_tool_argument_error，见 mcp.rs 工具注释）。

use std::path::{Path, PathBuf};

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::service::{QuitReason, ServiceExt};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};

use crate::project::ProjectRoot;

/// MCP server：工具路由 + 配置/根注入
#[derive(Debug, Clone)]
pub struct RepoWikiMcp {
    /// 工具路由（tool_handler 宏访问）
    #[expect(dead_code, reason = "tool_handler 宏访问此路由字段")]
    tool_router: ToolRouter<Self>,
    /// 配置文件路径（config.toml 项目级、全局或 --config 指定；None=默认链）
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

/// 工具成功结果：isError=false（文本内容）
///
/// MCP `CallToolResult::success` 将 `is_error` 置为 `Some(false)`（序列化为
/// `"isError": false`），与错误分支的 `true` 可程序化区分。成功路径含「合法
/// 空结果」（如搜索无命中、符号未找到、status 报告未生成）——这些是工具
/// 正常执行产生的输出，不属于工具执行错误。
fn tool_success(out: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(out)])
}

/// 工具业务错误：置 isError=true（MCP 规范「工具执行错误」）
///
/// MCP 区分两类失败：协议错误（JSON-RPC error，客户端渲染为不透明错误）与
/// 工具执行错误（result 内 `isError: true`，content 为调用方可读的说明）。
/// 业务错误（配置加载失败、非法参数、资源不存在、执行失败）属于后者——
/// AI 客户端据此把「工具成功但结果是错误消息」与真正的成功区分开。
/// 参数反序列化失败则由 rmcp 路由层（into_tool_argument_error）自动置
/// isError=true，无需本函数处理。
fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

// ============ 工具定义（#[tool_router] 块内，方法可访问 self 配置） ============

/// Search request parameters
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SearchRequest {
    /// Search keywords, e.g. "config load"
    query: String,
    /// Number of results to return (default: config search.default_top_k; clamped to 1..=50)
    top_k: Option<usize>,
    /// Search engine: text / semantic / hybrid (default: config default_engine)
    engine: Option<String>,
}

/// AST symbol lookup request parameters
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AstSearchRequest {
    /// Symbol name to look up (function / struct / trait / class, etc.)
    symbol: String,
    /// Source language (rust/python/go/...); auto-inferred from file extension if omitted
    language: Option<String>,
}

/// Read wiki page request parameters
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadPageRequest {
    /// Page file name (without .md), e.g. src_config, architecture, overview, api
    page: String,
    /// Language directory (default: config wiki.language)
    lang: Option<String>,
}

/// Read knowledge card request parameters
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ReadCardRequest {
    /// Card name (module name), e.g. src_config, crate_net
    card: String,
    /// Language directory (default: config wiki.language)
    lang: Option<String>,
}

/// Get module dependencies request parameters
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetDependenciesRequest {
    /// Module name, e.g. src::analysis, crate::net
    module: String,
}

#[tool_router(router = tool_router)]
impl RepoWikiMcp {
    /// Search code entities: keyword search over the pre-built code index
    #[tool(
        name = "wiki_search",
        title = "Search code index",
        description = "Purpose: keyword search over the pre-built code index (text / semantic / hybrid engines). Returns ranked hits with signature, file:lines, score, and callers/callees (hybrid engine only).\nWhen to use: to locate where a function, struct, class, or trait is defined or referenced by fuzzy keyword.\nWhen NOT to use: for exact symbol definition lookup, use wiki_ast_search instead.\nParameters & return example: {\"query\": \"config load\", \"top_k\": 10, \"engine\": \"hybrid\"} -> \"1. `fn load_config()` — src/config.rs:12-40 | score=0.8 | callers=[main] | callees=[]\". Note: requires `code-repo-wiki generate` to have built the index first.",
        annotations(read_only_hint = true)
    )]
    async fn search(
        &self,
        Parameters(SearchRequest {
            query,
            top_k,
            engine,
        }): Parameters<SearchRequest>,
    ) -> CallToolResult {
        // 配置完整性检查：搜索前确认配置可加载（错误早暴露）；v22 起
        // 引擎/条数默认值硬编码，配置内容不再被本函数使用。config 另用于
        // 读取语义降级标记（v0.6 FR-501，见下方结果尾部提示）
        let config = match crate::load_config_rooted(self.config_path.as_deref(), &self.root) {
            Ok(c) => c,
            Err(e) => return tool_error(format!("配置加载失败: {e}")),
        };
        let engine_type = match engine.as_deref() {
            Some("text") => crate::config::schema::SearchEngineType::Text,
            Some("semantic") => crate::config::schema::SearchEngineType::Semantic,
            Some("hybrid") => crate::config::schema::SearchEngineType::Hybrid,
            Some(other) => {
                return tool_error(format!(
                    "不支持的搜索引擎: {other}（可选: text/semantic/hybrid）"
                ));
            }
            None => crate::config::schema::SEARCH_DEFAULT_ENGINE,
        };
        let top_k = clamp_top_k(top_k.unwrap_or(crate::config::schema::SEARCH_DEFAULT_TOP_K));
        match crate::execute_search(
            self.config_path.as_deref(),
            &self.root,
            &query,
            top_k,
            &engine_type,
        ) {
            Ok(hits) => {
                // 先构造结果主体（空/非空），再统一追加降级提示——
                // 降级提示必须无条件出现：降级场景下 hybrid 引擎静默
                // 降级为纯 text（lib.rs:1468-1472），可能返回空结果，
                // 若只挂在非空分支，用户只会看到"未找到匹配结果"而
                // 永远不知索引已降级（reviewer 14.2 REJECTED 必须项，
                // 与 CLI 文本模式 main.rs:928-947 无条件提示对齐）
                let mut out = if hits.is_empty() {
                    // 尾部换行与下方提示行分隔（与命中分支的每行 \n 一致，
                    // 保证"提示: …"独占一行——测试精确断言 lines().last()）
                    "未找到匹配结果\n".to_string()
                } else {
                    let mut out = format!("找到 {} 个结果:\n", hits.len());
                    for (i, hit) in hits.iter().enumerate() {
                        let file = hit.node.file_path.as_deref().unwrap_or("-");
                        let loc = match hit.node.line_range {
                            Some((s, e)) => format!("{file}:{s}-{e}"),
                            None => file.to_string(),
                        };
                        let sig = hit.node.signature.as_deref().unwrap_or(&hit.node.name);
                        // C-004：命中行文本增强——附加 score 与调用关系。score 为
                        // SearchHit.score（hybrid 为 RRF 融合分，text/semantic 为原始
                        // 相关分，恒有值）；callers/callees 仅 hybrid 引擎由 SearchAgent
                        // 调用链补全填充，text/semantic 恒空（仍输出 [] 保留结构，
                        // 让 Agent 明确知道该字段存在）。
                        let callers = hit.callers.join(", ");
                        let callees = hit.callees.join(", ");
                        out.push_str(&format!(
                            "{}. `{sig}` — {loc} | score={:.3} | callers=[{callers}] | callees=[{callees}]\n",
                            i + 1,
                            hit.score
                        ));
                    }
                    out
                };
                // v0.6 FR-501：语义降级显式提示（cli-vs-mcp-07 修复）——
                // 降级此前仅进 tracing 日志，MCP 调用方不可见；读生成期
                // 降级标记（.search/semantic_degraded），有标记即追加原因
                if let Some(reason) = crate::semantic_degraded_reason(&config) {
                    out.push_str(&format!(
                        "提示: 语义索引已降级（原因: {}）\n",
                        reason.trim()
                    ));
                }
                tool_success(out)
            }
            // 索引缺失/执行失败为工具执行错误（isError=true）；"未找到匹配
            // 结果"是合法空结果，不在此分支，保持 isError=false
            Err(e) => tool_error(format!("搜索失败: {e}")),
        }
    }

    /// Exact symbol definition lookup via full AST scan of the source tree
    #[tool(
        name = "wiki_ast_search",
        title = "Look up symbol definition",
        description = "Purpose: exact symbol definition lookup via a full AST scan of the source tree. Returns definitions with signature and file:line location.\nWhen to use: when you need the precise definition file, line, and signature of a function/struct/trait/class.\nWhen NOT to use: for fuzzy keyword queries, use wiki_search instead.\nWARNING: scans the entire source tree; cost scales with repository size.\nParameters & return example: {\"symbol\": \"load_config\", \"language\": \"rust\"} -> \"1. fn load_config() -> Result<Config> — src/config.rs:20-45\".",
        annotations(read_only_hint = true)
    )]
    async fn ast_search(
        &self,
        Parameters(AstSearchRequest { symbol, language }): Parameters<AstSearchRequest>,
    ) -> CallToolResult {
        // C-009：全量扫描成本提示——execute_ast_search 扫描整个源码树，
        // 耗时随仓库规模增长；计时并在结果尾部附提示，提醒调用方谨慎使用
        let start = std::time::Instant::now();
        match crate::execute_ast_search(
            self.config_path.as_deref(),
            &self.root,
            &symbol,
            language.as_deref(),
        ) {
            // "未找到符号"是合法空结果（查询执行成功、无命中），非工具错误
            Ok(hits) if hits.is_empty() => tool_success(format!("未找到符号 \"{symbol}\" 的定义")),
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
                // 扫描耗时提示（C-009）：全量扫描成本随仓库规模增长，显式告知调用方
                out.push_str(&format!("（扫描耗时 {}ms）\n", start.elapsed().as_millis()));
                tool_success(out)
            }
            Err(e) => tool_error(format!("符号查找失败: {e}")),
        }
    }

    /// Read a generated wiki page's markdown content
    #[tool(
        name = "wiki_read_page",
        title = "Read wiki page",
        description = "Purpose: read the markdown content of a generated wiki page (module page / architecture overview / project overview / API).\nWhen to use: to retrieve module or API documentation generated by `code-repo-wiki generate`.\nWhen NOT to use: to read a knowledge card, use wiki_read_card instead. The page must already exist (run `code-repo-wiki generate` first), otherwise the tool reports an error.\nParameters & return example: {\"page\": \"architecture\", \"lang\": \"zh\"} -> \"/abs/path/wiki/zh/architecture.md\\n\\n(page markdown content)\". Returns the file path plus the markdown content.",
        annotations(read_only_hint = true)
    )]
    async fn read_wiki_page(
        &self,
        Parameters(ReadPageRequest { page, lang }): Parameters<ReadPageRequest>,
    ) -> CallToolResult {
        let config = match crate::load_config_rooted(self.config_path.as_deref(), &self.root) {
            Ok(c) => c,
            Err(e) => return tool_error(format!("配置加载失败: {e}")),
        };
        let lang = lang.unwrap_or_else(|| config.wiki.language.clone());
        // 语言目录净化（S1，工具暴露给任意 Agent）：lang 直接 join 进产物
        // 路径，未净化时 `../..` 可穿越到 output_dir 之外读取任意 .md 文件
        //（曾实测复现）。与 page 同规则但更严：语言目录名只允许
        // [A-Za-z0-9_-] 单段（zh、en、zh-CN 等），拒绝一切路径分隔符与
        // 绝对路径形态——校验失败明确报错，不读盘。
        if let Err(e) = validate_lang_segment(&lang) {
            return tool_error(e);
        }
        // 参数净化（工具暴露给任意 Agent）：拒绝路径穿越与绝对路径，
        // 只允许单段文件名（页面名），防读取 output_dir 之外任意文件
        if page.contains('/')
            || page.contains('\\')
            || page.contains("..")
            || Path::new(&page).is_absolute()
        {
            return tool_error(format!("非法的页面名: {page}（只允许单段文件名）"));
        }
        let path = config
            .output_dir()
            .join("wiki")
            .join(&lang)
            .join(format!("{page}.md"));
        match std::fs::read_to_string(&path) {
            Ok(content) => tool_success(format!("{}\n\n{content}", path.display())),
            // 页面不存在/不可读：工具执行错误（isError=true），引导运行 generate
            Err(e) => tool_error(format!(
                "页面不存在或不可读（可先运行 code-repo-wiki generate 生成）: {}: {e}",
                path.display()
            )),
        }
    }

    /// Read a generated knowledge card's markdown content
    #[tool(
        name = "wiki_read_card",
        title = "Read knowledge card",
        description = "Purpose: read the markdown content of a generated knowledge card (structured module summary for AI agents).\nWhen to use: to retrieve the details of a module card, or a project card (Spec rules card / TechStack stack card) by module name \"project::spec\" / \"project::tech-stack\".\nWhen NOT to use: to read a wiki page (module/API docs), use wiki_read_page instead. The card must already exist (run `code-repo-wiki generate` first), otherwise the tool reports an error.\nParameters & return example: {\"card\": \"src_config\", \"lang\": \"zh\"} -> \"/abs/path/cards/zh/src_config.md\\n\\n(card markdown content)\"; {\"card\": \"project::tech-stack\"} -> \"/abs/path/cards/zh/project/tech-stack.md\". Returns the file path plus the markdown content.",
        annotations(read_only_hint = true)
    )]
    async fn read_card(
        &self,
        Parameters(ReadCardRequest { card, lang }): Parameters<ReadCardRequest>,
    ) -> CallToolResult {
        let config = match crate::load_config_rooted(self.config_path.as_deref(), &self.root) {
            Ok(c) => c,
            Err(e) => return tool_error(format!("配置加载失败: {e}")),
        };
        let lang = lang.unwrap_or_else(|| config.wiki.language.clone());
        // 语言目录净化（S1）：同 read_wiki_page，lang 直接 join 进路径，
        // 未净化可穿越读取 output_dir 之外任意 .md（实测复现）。
        if let Err(e) = validate_lang_segment(&lang) {
            return tool_error(e);
        }
        // 同 read_wiki_page：净化路径穿越
        if card.contains('/')
            || card.contains('\\')
            || card.contains("..")
            || Path::new(&card).is_absolute()
        {
            return tool_error(format!("非法的卡片名: {card}（只允许单段文件名）"));
        }
        // 显式项目卡契约：MCP 的 card 参数取模块名形态 "project::spec" /
        // "project::tech-stack"（与模块卡同以 module_name 定位的约定）时映射到
        // 项目卡写盘路径（card_write_path 的 project/{spec|tech-stack}.md 子目录）；
        // 其余按模块卡根级命名 cards/{lang}/{card}.md 解析。
        let path = match card.as_str() {
            "project::spec" => crate::output::project_card_page_path(
                config.output_dir(),
                &lang,
                crate::model::CardKind::Spec,
            ),
            "project::tech-stack" => crate::output::project_card_page_path(
                config.output_dir(),
                &lang,
                crate::model::CardKind::TechStack,
            ),
            _ => config
                .output_dir()
                .join("cards")
                .join(&lang)
                .join(format!("{card}.md")),
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => tool_success(format!("{}\n\n{content}", path.display())),
            // 卡片不存在/不可读：工具执行错误（isError=true），引导运行 generate
            Err(e) => tool_error(format!(
                "卡片不存在或不可读（可先运行 code-repo-wiki generate 生成）: {}: {e}",
                path.display()
            )),
        }
    }

    /// Report generated artifact health (page/card counts, degraded index, lint issues)
    #[tool(
        name = "wiki_status",
        title = "Report wiki generation status",
        description = "Purpose: report generated artifact health — page/card counts, semantic index degradation, and lint issues (orphan pages / broken links / stale / references).\nWhen to use: to check whether `code-repo-wiki generate` has run and whether regeneration is needed.\nWhen NOT to use: to read actual page or card content, use wiki_read_page / wiki_read_card instead.\nParameters & return example: {} -> \"Wiki ready: 3 pages, 2 cards\\nsemantic index: normal\\nlint: ok (no issues)\". Returns counts plus degradation hints.",
        annotations(read_only_hint = true)
    )]
    async fn status(&self) -> CallToolResult {
        let config = match crate::load_config_rooted(self.config_path.as_deref(), &self.root) {
            Ok(c) => c,
            Err(e) => return tool_error(format!("配置加载失败: {e}")),
        };
        // v0.6（cli-vs-mcp-03 修复）：直接用注入的 self.root（--root 参数），
        // 不再 from_cwd() 重建——跨 cwd 调用时 from_cwd 解析到启动目录，
        // 与 --root 不一致会 lint 扫错目录、误报"未生成"
        let root = &self.root;
        let report = crate::commands::status_report(&config, root);
        // "Wiki 未生成"是 status 工具的正常输出（报告当前未生成状态），
        // 非工具执行错误——工具成功执行并如实报告，isError=false
        if !report.ready {
            return tool_success(
                "Wiki 未生成（运行 code-repo-wiki generate 生成后可用）".to_string(),
            );
        }
        let mut out = format!(
            "Wiki 就绪: {} 张页面, {} 张卡片\n",
            report.wiki_pages, report.cards
        );
        // v0.6 FR-501：status 报告显式提示语义降级（读降级标记；
        // 与 CLI status main.rs:674-678 行为一致）
        match crate::semantic_degraded_reason(&config) {
            Some(reason) => out.push_str(&format!("语义索引: 已降级（原因: {}）\n", reason.trim())),
            None => out.push_str("语义索引: 正常\n"),
        }
        if report.issues.is_empty() {
            out.push_str("lint: 通过（无孤儿页/断链/过时/引用/覆盖问题）");
        } else {
            out.push_str(&format!("lint: 发现 {} 个问题:\n", report.issues.len()));
            for issue in &report.issues {
                out.push_str(&format!(
                    "- [{}] {}: {}\n",
                    issue.kind, issue.path, issue.message
                ));
            }
        }
        tool_success(out)
    }

    /// Return a module's dependencies and dependents, aggregated from the knowledge graph
    #[tool(
        name = "wiki_get_dependencies",
        title = "Get module dependencies",
        description = "Purpose: return a module's dependencies (modules it imports/calls) and dependents (modules that import/call it), aggregated from the knowledge graph (Imports + Calls edges, deduplicated). Same data source as architecture-map.md.\nWhen to use: to answer 'who depends on X' or 'what does X depend on' directly, without reading full module pages.\nWhen NOT to use: to read full module documentation, use wiki_read_page / wiki_read_card instead.\nWARNING: scans the entire source tree to rebuild the graph; cost scales with repository size.\nParameters & return example: {\"module\": \"src::analysis\"} -> \"模块 src::analysis\\n依赖: src::output\\n被依赖: src::generate\". Note: requires `code-repo-wiki generate` to have run (module names come from the graph's module clustering).",
        annotations(read_only_hint = true)
    )]
    async fn get_dependencies(
        &self,
        Parameters(GetDependenciesRequest { module }): Parameters<GetDependenciesRequest>,
    ) -> CallToolResult {
        // 配置完整性校验（错误早暴露）；本工具数据源=知识图谱，无需 config 内容
        if let Err(e) = crate::load_config_rooted(self.config_path.as_deref(), &self.root) {
            return tool_error(format!("配置加载失败: {e}"));
        }
        // 扫描 + 重建图（与 execute_search hybrid 调用链补全同一路径：
        // MCP server 无状态，每次现场重建；成本随仓库规模增长，description 已注明）
        let scan = match crate::ingest::scan_and_parse_at(&self.root) {
            Ok(s) => s,
            Err(e) => return tool_error(format!("代码扫描失败: {e}")),
        };
        let graph = match crate::analysis::build_graph(&scan.insights) {
            Ok(g) => g,
            Err(e) => return tool_error(format!("知识图谱构建失败: {e}")),
        };
        // 与架构地图同一聚合函数（DRY）：模块级依赖 = imports/calls 边聚合
        let deps = crate::analysis::architecture_map::module_dependencies(&graph);
        // 模块不存在 → 合法空结果（查询执行成功、无命中），isError=false
        let Some(d) = deps.iter().find(|d| d.name == module) else {
            return tool_success(format!(
                "未找到模块 \"{module}\"（可先运行 code-repo-wiki generate）"
            ));
        };
        let deps_str = if d.dependencies.is_empty() {
            "无".to_string()
        } else {
            d.dependencies.join(", ")
        };
        let deps_by_str = if d.dependents.is_empty() {
            "无".to_string()
        } else {
            d.dependents.join(", ")
        };
        tool_success(format!(
            "模块 {}\n依赖: {}\n被依赖: {}",
            d.name, deps_str, deps_by_str
        ))
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
/// { "command": "code-repo-wiki", "args": ["mcp", "--root", "."] }
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
