import type { PluginInput } from "@opencode-ai/plugin";
import { execa } from "execa";

/**
 * repo-wiki OpenCode 插件
 *
 * 提供：
 * - 4 个 Agent 工具（wiki_search, wiki_query, wiki_generate, module_info）
 * - /wiki Slash 命令（generate, update, status, export）
 * - 自动调用 Rust CLI 核心引擎
 * - 从 .repo-wiki/ 读取现有卡片和 Wiki 数据
 */
export const RepoWikiPlugin = async ({ project, client, directory }: PluginInput) => {

    /** 调用 repo-wiki CLI 并返回结构化输出 */
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

    /** 从 .repo-wiki/cards/ 读取 Knowledge Card */
    async function readExistingCards(): Promise<any[]> {
        const { readFileSync, existsSync, readdirSync } = await import("fs");
        const { join } = await import("path");
        const cardsDir = join(directory, ".repo-wiki", "cards");
        if (!existsSync(cardsDir)) return [];

        try {
            const files = readdirSync(cardsDir).filter(f => f.endsWith(".md"));
            const cards: any[] = [];
            for (const file of files) {
                const content = readFileSync(join(cardsDir, file), "utf-8");
                cards.push({ name: file.replace(".md", ""), content });
            }
            return cards;
        } catch {
            return [];
        }
    }

    return {
        tools: [
            {
                name: "wiki_search",
                description: "搜索代码实体（函数、结构体、类等），基于 BM25 全文检索返回匹配结果",
                args: {
                    query: { type: "string", description: "搜索关键词" },
                    top_k: { type: "number", description: "返回结果数量（默认 10）" },
                },
                execute: async (args: any) => {
                    const query = (args.query as string) || "";
                    if (!query) return "请提供搜索关键词";
                    const topK = (args.top_k as number) || 10;

                    const result = await runCli([
                        "search", "-q", JSON.stringify(query),
                        "-k", String(topK),
                        "--json",
                        "--config", ".repo-wiki/config.toml",
                    ]);

                    if (result.code === 0 && result.stdout.trim()) {
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
                    return `搜索失败: ${result.stderr || "索引不存在，请先运行 generate"}`;
                },
            },
            {
                name: "wiki_query",
                description: "查询项目 Wiki 知识，返回 Knowledge Card 或 Wiki 页面内容",
                args: {
                    query: { type: "string", description: "搜索关键词或模块名称" },
                },
                execute: async (args: any) => {
                    const query = (args.query as string) || "";
                    if (!query) return "请提供搜索关键词";

                    const cards = await readExistingCards();
                    const matched = cards.filter(c =>
                        c.name.toLowerCase().includes(query.toLowerCase())
                    );

                    if (matched.length > 0) {
                        return matched.map(c => `## ${c.name}\n\n${c.content.slice(0, 2000)}`).join("\n\n---\n\n");
                    }

                    const result = await runCli([
                        "search", "-q", JSON.stringify(query),
                        "--json", "--config", ".repo-wiki/config.toml",
                    ]);
                    if (result.code === 0 && result.stdout.trim()) {
                        return `搜索结果:\n${result.stdout.slice(0, 3000)}`;
                    }
                    return `未找到与 "${query}" 相关的知识`;
                },
            },
            {
                name: "wiki_generate",
                description: "生成或更新项目 Wiki 文档",
                args: {
                    output: { type: "string", description: "输出目录（默认 .repo-wiki）" },
                },
                execute: async (args: any) => {
                    const output = (args.output as string) || "";
                    const cliArgs = ["generate", "--config", ".repo-wiki/config.toml", "--progress-json"];
                    if (output) cliArgs.push("-o", output);
                    const child = execa("repo-wiki", cliArgs, { cwd: directory });
                    child.stdout?.on("data", (chunk: Buffer) => {
                        for (const line of chunk.toString().split("\n")) {
                            if (!line.startsWith('{"stage":')) continue;
                            try {
                                const evt = JSON.parse(line);
                                (client as any)?.sendProgress?.({ stage: evt.stage, progress: evt.progress });
                            } catch {
                                // 半行/坏行忽略：展示层容错
                            }
                        }
                    });
                    const { stdout, stderr } = await child;
                    return stdout || `生成完成。${stderr ? "警告: " + stderr : ""}`;
                },
            },
            {
                name: "module_info",
                description: "获取项目中某个模块的结构化信息",
                args: { module: { type: "string", description: "模块路径" } },
                execute: async (args: any) => {
                    const modulePath = String(args.module || "");
                    const cards = await readExistingCards();
                    const matched = cards.filter(c => c.name?.includes(modulePath));
                    if (matched.length === 0) return `未找到模块 "${modulePath}" 的信息`;
                    return matched.map(c => `## ${c.name}\n\n${c.content}`).join("\n\n---\n\n");
                },
            },
        ],
        commands: [{
            name: "wiki",
            description: "Wiki 生成与管理命令",
            subcommands: [
                {
                    name: "generate",
                    description: "全量生成项目 Wiki",
                    execute: async () => {
                        const result = await runCli(["generate"]);
                        return result.code === 0 ? "Wiki 全量生成完成" : "生成失败: " + result.stderr;
                    },
                },
                {
                    name: "update",
                    description: "增量更新 Wiki",
                    execute: async () => {
                        const result = await runCli(["update"]);
                        return result.code === 0 ? "Wiki 增量更新完成" : "更新失败: " + result.stderr;
                    },
                },
                {
                    name: "status",
                    description: "查看 Wiki 状态",
                    execute: async () => {
                        const result = await runCli(["status"]);
                        return result.code === 0 ? result.stdout : "查看状态失败: " + result.stderr;
                    },
                },
                {
                    name: "export",
                    description: "导出 Wiki",
                    execute: async () => {
                        const result = await runCli(["export"]);
                        return result.code === 0 ? result.stdout || "导出完成" : "导出失败: " + result.stderr;
                    },
                },
            ],
        },
        {
            name: "knowledge",
            description: "知识卡片管理",
            subcommands: [
                {
                    name: "generate",
                    description: "为单个模块生成知识卡片",
                    execute: async (args: string) => {
                        const [module] = (args || "").trim().split(/\s+/);
                        if (!module) return "用法: /knowledge generate <模块名>";
                        const result = await runCli(["card", "generate", module, "--config", ".repo-wiki/config.toml"]);
                        return result.code === 0
                            ? (result.stdout || `卡片 ${module} 生成完成`)
                            : "生成失败: " + result.stderr;
                    },
                },
                {
                    name: "modify",
                    description: "按指令修改已有卡片",
                    execute: async (args: string) => {
                        const [module, ...rest] = (args || "").trim().split(/\s+/);
                        if (!module || rest.length === 0) return "用法: /knowledge modify <模块名> <指令文本>";
                        const instruction = rest.join(" ");
                        const result = await runCli(["card", "modify", module, "--instruction", instruction, "--config", ".repo-wiki/config.toml"]);
                        return result.code === 0
                            ? (result.stdout || `卡片 ${module} 修改完成`)
                            : "修改失败: " + result.stderr;
                    },
                },
                {
                    name: "supplement",
                    description: "在已有卡片上追加内容",
                    execute: async (args: string) => {
                        const [module, ...rest] = (args || "").trim().split(/\s+/);
                        if (!module || rest.length === 0) return "用法: /knowledge supplement <模块名> <指令文本>";
                        const instruction = rest.join(" ");
                        const result = await runCli(["card", "supplement", module, "--instruction", instruction, "--config", ".repo-wiki/config.toml"]);
                        return result.code === 0
                            ? (result.stdout || `卡片 ${module} 补充完成`)
                            : "补充失败: " + result.stderr;
                    },
                },
                {
                    name: "rewrite",
                    description: "忽略现有内容全量重写卡片",
                    execute: async (args: string) => {
                        const [module, ...rest] = (args || "").trim().split(/\s+/);
                        if (!module || rest.length === 0) return "用法: /knowledge rewrite <模块名> <指令文本>";
                        const instruction = rest.join(" ");
                        const result = await runCli(["card", "rewrite", module, "--instruction", instruction, "--config", ".repo-wiki/config.toml"]);
                        return result.code === 0
                            ? (result.stdout || `卡片 ${module} 重写完成`)
                            : "重写失败: " + result.stderr;
                    },
                },
            ],
        }],
    };
};
