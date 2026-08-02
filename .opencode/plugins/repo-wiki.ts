import type { Plugin, PluginInput } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { execa } from "execa";

/**
 * repo-wiki OpenCode 插件
 *
 * 提供：
 * - 16 个 Agent 工具：5 个查询工具（ast_search/wiki_search/wiki_query/wiki_generate/module_info）
 *   + 4 个知识卡片工具（card_generate/card_modify/card_supplement/card_rewrite）
 *   + 7 个 Wiki 管理工具（wiki_update/wiki_sync/wiki_status/wiki_export/wiki_note/wiki_lint/wiki_init）
 * - 自动调用 Rust CLI 核心引擎（execa）
 * - 从 .repo-wiki/ 读取现有卡片和 Wiki 数据
 *
 * 形状约束（opencode 1.18.10，务必保持）：
 * - 本模块必须**命名导出函数**（插件加载器要求模块导出函数或含 server() 的对象，
 *   返回数组形状会被加载器抛 TypeError 并静默吞掉，插件将完全不生效）
 * - 返回值必须是官方 Hooks 形状：自定义工具放 `tool` 对象映射
 *   （`{ [key: string]: ToolDefinition }`），**不存在 `tools`/`commands` 数组键**
 * - 斜杠命令不走插件：由 .opencode/commands/*.md 命令文件注册（见 1.4）
 *
 * 历史教训：旧实现返回 { tools: [], commands: [] } 数组形状，不符合 Hooks，
 * 导致插件从未被加载（tsc 因未标注 Plugin 类型而静默通过）。
 */

export const RepoWikiPlugin: Plugin = async ({ directory }: PluginInput) => {
    /** 调用 repo-wiki CLI 并返回结构化输出（stderr/非零退出不抛错，交由调用方处理） */
    async function runCli(args: string[]): Promise<{ stdout: string; stderr: string; code: number }> {
        try {
            const { stdout, stderr, exitCode } = await execa("repo-wiki", args, {
                cwd: directory,
                maxBuffer: 10 * 1024 * 1024,
                reject: false,
            });
            return { stdout: stdout ?? "", stderr: stderr ?? "", code: exitCode ?? 0 };
        } catch (err: any) {
            return {
                stdout: err.stdout?.toString() || "",
                stderr: err.stderr?.toString() || err.message || "",
                code: err.exitCode ?? 1,
            };
        }
    }

    /** --config 值：root 是项目根目录（拼 root/.repo-wiki/config.toml），缺省用 cwd 相对路径 */
    function configPath(root?: string): string {
        return root ? `${root}/.repo-wiki/config.toml` : ".repo-wiki/config.toml";
    }

    /** 从 .repo-wiki/cards/{lang}/ 读取 Knowledge Card（目录不存在时返回空列表） */
    async function readExistingCards(root?: string): Promise<Array<{ name: string; content: string }>> {
        const { readFileSync, existsSync, readdirSync } = await import("fs");
        const { join } = await import("path");
        const cardsDir = join(root ?? directory, ".repo-wiki", "cards");
        if (!existsSync(cardsDir)) return [];

        try {
            // 实际写盘结构为 cards/{lang}/{module}.md（output::card_page_path 含 lang 层），
            // 故按语言子目录递归一层收集；同模块多语言并存时保留首个（排序保证确定性）
            const langs = readdirSync(cardsDir)
                .filter((d) => {
                    try {
                        return readdirSync(join(cardsDir, d)).some((f) => f.endsWith(".md"));
                    } catch {
                        return false;
                    }
                })
                .sort();
            const seen = new Set<string>();
            const cards: Array<{ name: string; content: string }> = [];
            for (const lang of langs) {
                for (const file of readdirSync(join(cardsDir, lang)).filter(f => f.endsWith(".md")).sort()) {
                    const name = file.replace(".md", "");
                    if (seen.has(name)) continue;
                    seen.add(name);
                    cards.push({
                        name,
                        content: readFileSync(join(cardsDir, lang, file), "utf-8"),
                    });
                }
            }
            return cards;
        } catch {
            return [];
        }
    }

    /** 执行 `repo-wiki search` 并格式化为 Markdown 命中列表 */
    async function searchEntities(query: string, topK: number, engine: string | undefined, root?: string): Promise<string> {
        const cliArgs = [
            "search", "-q", JSON.stringify(query),
            "-k", String(topK),
            "--json",
            "--config", configPath(root),
        ];
        if (engine) cliArgs.push("--engine", engine);
        const result = await runCli(cliArgs);
        if (result.code !== 0 || !result.stdout.trim()) {
            return `搜索失败: ${result.stderr || "索引不存在，请先运行 generate"}`;
        }
        try {
            const hits = JSON.parse(result.stdout);
            if (hits.length === 0) return "未找到匹配结果";
            return hits.map((h: any, i: number) =>
                `${i + 1}. **${h.name}** (${h.kind}) - ${h.file || "-"}\n` +
                `   签名: ${h.signature || "-"}\n` +
                `   分数: ${h.score?.toFixed(2)}`
            ).join("\n\n");
        } catch {
            return result.stdout;
        }
    }

    /**
     * 知识卡片工具工厂：modify/supplement/rewrite 三个动作结构一致，
     * 仅 CLI 子命令与描述不同，用工厂消除重复（DRY）。
     *
     * reference 数组在 CLI 层展开为重复的 --reference flag
     * （main.rs 的 reference 参数为 Vec<PathBuf> 且无 value_delimiter，
     * 天然支持重复 flag；card.rs read_references 校验存在性，
     * 文件不存在时 CLI 显式报错）。
     */
    function cardTool(action: "modify" | "supplement" | "rewrite", description: string) {
        return tool({
            description,
            args: {
                module: tool.schema.string().describe("模块名（如 crate::config）"),
                instruction: tool.schema.string().describe("修改指令文本"),
                reference: tool.schema.array(tool.schema.string()).optional()
                    .describe("参考文件路径列表（@ 引用的文件或显式路径）"),
                root: tool.schema.string().optional()
                    .describe("项目根目录（默认当前工作目录；提供时从 root/.repo-wiki/ 读写产物）"),
            },
            execute: async (args) => {
                const cliArgs = [
                    "card", action, args.module,
                    "--instruction", args.instruction,
                    "--config", configPath(args.root),
                ];
                if (args.reference?.length) {
                    for (const r of args.reference) cliArgs.push("--reference", r);
                }
                const result = await runCli(cliArgs);
                return result.code === 0
                    ? (result.stdout || `卡片 ${args.module} ${action} 完成`)
                    : `卡片 ${args.module} ${action} 失败: ${result.stderr}`;
            },
        });
    }

    /** Wiki 管理工具工厂：无参子命令转发（update/sync/status/export），extraArgs 追加固定参数 */
    function wikiCmdTool(name: string, description: string, extraArgs: string[] = []) {
        return tool({
            description,
            args: {
                root: tool.schema.string().optional()
                    .describe("项目根目录（默认当前工作目录；提供时从 root/.repo-wiki/ 读写产物）"),
            },
            execute: async (args) => {
                const result = await runCli([name, "--config", configPath(args.root), ...extraArgs]);
                return result.code === 0
                    ? (result.stdout || `repo-wiki ${name} 完成`)
                    : `repo-wiki ${name} 失败: ${result.stderr}`;
            },
        });
    }

    return {
        tool: {
            // ---- 查询工具 ----
            ast_search: tool({
                description: "AST 精确符号查找：扫描源文件定位符号定义（文件+行号+签名，不依赖搜索索引）",
                args: {
                    symbol: tool.schema.string().describe("要查找的符号名（函数/结构体/trait/类等）"),
                    language: tool.schema.string().optional().describe("源语言（rust/python/go/...，省略时按扩展名推断）"),
                    root: tool.schema.string().optional()
                        .describe("项目根目录（默认当前工作目录；提供时从 root/.repo-wiki/ 读写产物）"),
                },
                execute: async (args) => {
                    if (!args.symbol) return "请提供要查找的符号名";
                    const cliArgs = ["ast-search", args.symbol, "--config", configPath(args.root)];
                    if (args.language) cliArgs.push("--language", args.language);
                    const result = await runCli(cliArgs);
                    return result.code === 0
                        ? (result.stdout || `未找到符号 "${args.symbol}" 的定义`)
                        : `查找失败: ${result.stderr}`;
                },
            }),
            wiki_search: tool({
                description: "搜索代码实体（函数、结构体、类等），基于 BM25 全文检索返回匹配结果",
                args: {
                    query: tool.schema.string().describe("搜索关键词"),
                    top_k: tool.schema.number().optional().describe("返回结果数量（默认 10）"),
                    engine: tool.schema.string().optional()
                        .describe("搜索引擎: text/semantic/hybrid（默认取配置文件 default_engine）"),
                    root: tool.schema.string().optional()
                        .describe("项目根目录（默认当前工作目录；提供时从 root/.repo-wiki/ 读写产物）"),
                },
                execute: async (args) => {
                    if (!args.query) return "请提供搜索关键词";
                    return searchEntities(args.query, args.top_k ?? 10, args.engine, args.root);
                },
            }),

            wiki_query: tool({
                description: "查询项目 Wiki 知识，返回 Knowledge Card 或 Wiki 页面内容",
                args: {
                    query: tool.schema.string().describe("搜索关键词或模块名称"),
                    root: tool.schema.string().optional()
                        .describe("项目根目录（默认当前工作目录；提供时从 root/.repo-wiki/ 读写产物）"),
                },
                execute: async (args) => {
                    if (!args.query) return "请提供搜索关键词";
                    const cards = await readExistingCards(args.root);
                    const matched = cards.filter((c) =>
                        c.name.toLowerCase().includes(args.query.toLowerCase())
                    );
                    if (matched.length > 0) {
                        return matched
                            .map((c) => `## ${c.name}\n\n${c.content.slice(0, 2000)}`)
                            .join("\n\n---\n\n");
                    }
                    // 卡片未命中：明确提示后仍回退到搜索索引（保底可用，不静默吞掉查询）
                    const hits = await searchEntities(args.query, 10, undefined, args.root);
                    return `未找到匹配卡片，尝试搜索:\n\n${hits}`;
                },
            }),

            wiki_generate: tool({
                description: "全量生成或更新项目 Wiki 文档（所有模块）",
                args: {
                    output: tool.schema.string().optional().describe("输出目录（默认 .repo-wiki）"),
                    force: tool.schema.boolean().optional()
                        .describe("清空人工修改保护集，强制覆盖所有文档"),
                    root: tool.schema.string().optional()
                        .describe("项目根目录（默认当前工作目录；提供时从 root/.repo-wiki/ 读写产物）"),
                },
                execute: async (args) => {
                    const cliArgs = ["generate", "--config", configPath(args.root)];
                    if (args.output) cliArgs.push("-o", args.output);
                    if (args.force) cliArgs.push("--force");
                    const result = await runCli(cliArgs);
                    return result.code === 0
                        ? (result.stdout || "Wiki 全量生成完成")
                        : `生成失败: ${result.stderr}`;
                },
            }),

            module_info: tool({
                description: "获取项目中某个模块的结构化信息",
                args: {
                    module: tool.schema.string().describe("模块路径"),
                    root: tool.schema.string().optional()
                        .describe("项目根目录（默认当前工作目录；提供时从 root/.repo-wiki/ 读写产物）"),
                },
                execute: async (args) => {
                    const cards = await readExistingCards(args.root);
                    const matched = cards.filter((c) => c.name?.includes(args.module));
                    if (matched.length === 0) return `未找到模块 "${args.module}" 的信息`;
                    return matched.map((c) => `## ${c.name}\n\n${c.content}`).join("\n\n---\n\n");
                },
            }),

            // ---- 知识卡片工具（CLI card 子命令转发） ----
            card_generate: tool({
                description: "为单个模块生成知识卡片",
                args: {
                    module: tool.schema.string().describe("模块名（如 crate::config）"),
                    root: tool.schema.string().optional()
                        .describe("项目根目录（默认当前工作目录；提供时从 root/.repo-wiki/ 读写产物）"),
                },
                execute: async (args) => {
                    const result = await runCli([
                        "card", "generate", args.module,
                        "--config", configPath(args.root),
                    ]);
                    return result.code === 0
                        ? (result.stdout || `卡片 ${args.module} 生成完成`)
                        : `生成失败: ${result.stderr}`;
                },
            }),
            card_modify: cardTool("modify", "按指令修改已有卡片（可附参考文件）"),
            card_supplement: cardTool("supplement", "在已有卡片上追加内容（可附参考文件）"),
            card_rewrite: cardTool("rewrite", "忽略现有内容全量重写卡片（可附参考文件）"),

            // ---- Wiki 管理工具（CLI 子命令转发） ----
            wiki_update: wikiCmdTool("update", "增量更新 Wiki（代码变更 → 仅重建受影响页）"),
            // sync：以 Git 工作区内容为准同步指纹库，不触发 LLM 重生成（commands.rs sync_from_git）
            wiki_sync: wikiCmdTool("sync", "以 Git 工作区内容为准同步 Wiki（不触发 LLM 重生成）"),
            wiki_status: wikiCmdTool("status", "查看 Wiki 状态"),
            // export：跳过生成，直接消费导出快照（快照缺失时 CLI 显式报错引导）
            wiki_export: wikiCmdTool("export", "导出 Wiki（从导出快照导出，不重跑生成）", ["--skip-generate"]),
            // note：追加一条知识沉淀记录到 _log.md（Karpathy log 模式，人工可读可 grep）
            wiki_note: tool({
                description: "追加一条知识沉淀记录到 Wiki _log.md（人工可读可 grep 的会话知识日志）",
                args: {
                    text: tool.schema.string().describe("记录内容"),
                    root: tool.schema.string().optional()
                        .describe("项目根目录（默认当前工作目录；提供时从 root/.repo-wiki/ 读写产物）"),
                },
                execute: async (args) => {
                    if (!args.text || !args.text.trim()) return "请提供记录内容";
                    const result = await runCli([
                        "note", args.text.trim(),
                        "--config", configPath(args.root),
                    ]);
                    return result.code === 0
                        ? (result.stdout || "知识记录已追加")
                        : `记录失败: ${result.stderr}`;
                },
            }),
            // lint：检查产物健康（孤儿页/断链/过时），供 CI 与人工巡检使用
            wiki_lint: tool({
                description: "检查 Wiki 产物健康：孤儿页/断链/过时文档（发现问题时退出码非 0）",
                args: {
                    root: tool.schema.string().optional()
                        .describe("项目根目录（默认当前工作目录；提供时从 root/.repo-wiki/ 读写产物）"),
                },
                execute: async (args) => {
                    const result = await runCli([
                        "lint", "--config", configPath(args.root),
                    ]);
                    return result.code === 0
                        ? (result.stdout || "lint: 通过，无孤儿页/断链/过时问题")
                        : `lint 发现问题:\n${result.stdout || result.stderr}`;
                },
            }),
            // init：引导缺失 config 场景（生成 schema 对齐的默认配置，供后续 generate/search 使用）
            wiki_init: tool({
                description: "初始化 .repo-wiki/config.toml 默认配置文件（缺失 config 时的引导入口）",
                args: {
                    root: tool.schema.string().optional()
                        .describe("项目根目录（默认当前工作目录；提供时在 root/.repo-wiki/config.toml 处初始化）"),
                },
                execute: async (args) => {
                    // init 的子命令参数是配置文件路径（positional，无 --config）；
                    // 指定 root 时在 root/.repo-wiki/config.toml 处初始化，否则用 CLI 默认路径
                    const cliArgs = args.root ? ["init", `${args.root}/.repo-wiki/config.toml`] : ["init"];
                    const result = await runCli(cliArgs);
                    return result.code === 0
                        ? (result.stdout || "默认配置文件已创建")
                        : `初始化失败: ${result.stderr}`;
                },
            }),
        },
    };
};
