import type { Plugin, PluginInput } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { execa } from "execa";

/**
 * repo-wiki OpenCode 插件
 *
 * 提供：
 * - 9 个 Agent 工具：4 个查询工具（wiki_search/wiki_query/wiki_generate/module_info）
 *   + 4 个知识卡片工具（card_generate/card_modify/card_supplement/card_rewrite）
 *   + 4 个 Wiki 管理工具（wiki_update/wiki_sync/wiki_status/wiki_export）
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

    /** 从 .repo-wiki/cards/ 读取 Knowledge Card（目录不存在时返回空列表） */
    async function readExistingCards(): Promise<Array<{ name: string; content: string }>> {
        const { readFileSync, existsSync, readdirSync } = await import("fs");
        const { join } = await import("path");
        const cardsDir = join(directory, ".repo-wiki", "cards");
        if (!existsSync(cardsDir)) return [];

        try {
            const files = readdirSync(cardsDir).filter(f => f.endsWith(".md"));
            return files.map((file) => ({
                name: file.replace(".md", ""),
                content: readFileSync(join(cardsDir, file), "utf-8"),
            }));
        } catch {
            return [];
        }
    }

    /** 执行 `repo-wiki search` 并格式化为 Markdown 命中列表 */
    async function searchEntities(query: string, topK: number): Promise<string> {
        const result = await runCli([
            "search", "-q", JSON.stringify(query),
            "-k", String(topK),
            "--json",
            "--config", ".repo-wiki/config.toml",
        ]);
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
     * reference 数组在 CLI 层拼为逗号分隔的 --reference 多文件参数
     * （main.rs:124 value_delimiter=','；card.rs:175 read_references 校验存在性，
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
            },
            execute: async (args) => {
                const cliArgs = [
                    "card", action, args.module,
                    "--instruction", args.instruction,
                    "--config", ".repo-wiki/config.toml",
                ];
                if (args.reference?.length) {
                    cliArgs.push("--reference", args.reference.join(","));
                }
                const result = await runCli(cliArgs);
                return result.code === 0
                    ? (result.stdout || `卡片 ${args.module} ${action} 完成`)
                    : `卡片 ${args.module} ${action} 失败: ${result.stderr}`;
            },
        });
    }

    /** Wiki 管理工具工厂：无参数子命令转发（update/sync/status/export） */
    function wikiCmdTool(name: string, description: string) {
        return tool({
            description,
            args: {},
            execute: async () => {
                const result = await runCli([name]);
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
                },
                execute: async (args) => {
                    if (!args.symbol) return "请提供要查找的符号名";
                    const cliArgs = ["ast-search", args.symbol, "--config", ".repo-wiki/config.toml"];
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
                },
                execute: async (args) => {
                    if (!args.query) return "请提供搜索关键词";
                    return searchEntities(args.query, args.top_k ?? 10);
                },
            }),

            wiki_query: tool({
                description: "查询项目 Wiki 知识，返回 Knowledge Card 或 Wiki 页面内容",
                args: {
                    query: tool.schema.string().describe("搜索关键词或模块名称"),
                },
                execute: async (args) => {
                    if (!args.query) return "请提供搜索关键词";
                    const cards = await readExistingCards();
                    const matched = cards.filter((c) =>
                        c.name.toLowerCase().includes(args.query.toLowerCase())
                    );
                    if (matched.length > 0) {
                        return matched
                            .map((c) => `## ${c.name}\n\n${c.content.slice(0, 2000)}`)
                            .join("\n\n---\n\n");
                    }
                    // 卡片未命中时回退到搜索索引
                    return searchEntities(args.query, 10);
                },
            }),

            wiki_generate: tool({
                description: "全量生成或更新项目 Wiki 文档（所有模块）",
                args: {
                    output: tool.schema.string().optional().describe("输出目录（默认 .repo-wiki）"),
                },
                execute: async (args) => {
                    const cliArgs = ["generate", "--config", ".repo-wiki/config.toml"];
                    if (args.output) cliArgs.push("-o", args.output);
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
                },
                execute: async (args) => {
                    const cards = await readExistingCards();
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
                },
                execute: async (args) => {
                    const result = await runCli([
                        "card", "generate", args.module,
                        "--config", ".repo-wiki/config.toml",
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
            wiki_export: wikiCmdTool("export", "导出 Wiki"),
            // note：追加一条知识沉淀记录到 _log.md（Karpathy log 模式，人工可读可 grep）
            wiki_note: tool({
                description: "追加一条知识沉淀记录到 Wiki _log.md（人工可读可 grep 的会话知识日志）",
                args: {
                    text: tool.schema.string().describe("记录内容"),
                },
                execute: async (args) => {
                    if (!args.text || !args.text.trim()) return "请提供记录内容";
                    const result = await runCli([
                        "note", args.text.trim(),
                        "--config", ".repo-wiki/config.toml",
                    ]);
                    return result.code === 0
                        ? (result.stdout || "知识记录已追加")
                        : `记录失败: ${result.stderr}`;
                },
            }),
            // lint：检查产物健康（孤儿页/断链/过时），供 CI 与人工巡检使用
            wiki_lint: tool({
                description: "检查 Wiki 产物健康：孤儿页/断链/过时文档（发现问题时退出码非 0）",
                args: {},
                execute: async () => {
                    const result = await runCli([
                        "lint", "--config", ".repo-wiki/config.toml",
                    ]);
                    return result.code === 0
                        ? (result.stdout || "lint: 通过，无孤儿页/断链/过时问题")
                        : `lint 发现问题:\n${result.stdout || result.stderr}`;
                },
            }),
        },
    };
};
