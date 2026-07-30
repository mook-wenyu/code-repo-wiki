import type { Plugin } from "@opencode-ai/plugin";

/**
 * repo-wiki OpenCode 插件
 * 
 * 提供：
 * - 2 个 Agent 工具（wiki_query, generate_wiki）
 * - 自动调用 Rust CLI 核心引擎
 * - 从 .repo-wiki/ 读取现有卡片和 Wiki 数据
 */
export const RepoWikiPlugin: Plugin = async ({ project, client }) => {

    /** 调用 repo-wiki CLI 并返回结构化输出 */
    async function runCli(args: string[]): Promise<{ stdout: string; stderr: string; code: number }> {
        const { execSync } = await import("child_process");
        try {
            const result = execSync(`repo-wiki ${args.join(" ")}`, {
                cwd: project.rootPath,
                encoding: "utf-8",
                maxBuffer: 10 * 1024 * 1024,
            });
            return { stdout: result.toString(), stderr: "", code: 0 };
        } catch (err: any) {
            return {
                stdout: err.stdout?.toString() || "",
                stderr: err.stderr?.toString() || err.message || "",
                code: err.status ?? 1,
            };
        }
    }

    /** 从 .repo-wiki/cards/ 读取 Knowledge Card */
    async function readExistingCards(): Promise<any[]> {
        const { readFileSync, existsSync, readdirSync } = await import("fs");
        const { join } = await import("path");
        const cardsDir = join(project.rootPath, ".repo-wiki", "cards");
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
                name: "wiki_query",
                description: "查询项目 Wiki 知识，返回 Knowledge Card 或 Wiki 页面内容",
                args: {
                    query: { type: "string", description: "搜索关键词或模块名称" },
                },
                execute: async (args) => {
                    const query = (args.query as string) || "";
                    if (!query) {
                        return "请提供搜索关键词";
                    }

                    // 先尝试读取本地已有卡片
                    const cards = await readExistingCards();
                    const matched = cards.filter(c =>
                        c.name.toLowerCase().includes(query.toLowerCase())
                    );

                    if (matched.length > 0) {
                        return matched.map(c => `## ${c.name}\n\n${c.content.slice(0, 2000)}`).join("\n\n---\n\n");
                    }

                    // 无匹配时调用 CLI
                    const result = await runCli(["status", "--config", ".repo-wiki/config.toml"]);
                    if (result.code === 0) {
                        return `Wiki 状态: ${result.stdout}`;
                    }
                    return `查询失败: ${result.stderr}`;
                },
            },
            {
                name: "generate_wiki",
                description: "生成或更新项目 Wiki 文档（全量生成）",
                args: {
                    incremental: {
                        type: "boolean",
                        description: "是否使用增量更新模式（默认 true，只更新变更部分）",
                    },
                },
                execute: async (args) => {
                    const incremental = args.incremental !== false;
                    const cmd = incremental ? "update" : "generate";
                    // 调用 CLI 时通过环境变量传递进度回调
                    const result = await runCli([cmd, "--config", ".repo-wiki/config.toml"]);
                    if (result.code === 0) {
                        const cards = await readExistingCards();
                        return `Wiki 生成完成！\n\n已生成 ${cards.length} 个 Knowledge Card\n\n可以在 .repo-wiki/wiki/ 目录查看生成的 Wiki 文档。`;
                    }
                    return `Wiki 生成失败:\n${result.stderr}`;
                },
            },
        ],
    };
};
