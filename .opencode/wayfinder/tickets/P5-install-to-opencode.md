## 问题

repo-wiki 以什么方式注册为 OpenCode 可调用的搜索工具？

### 待定方案

A. **CLI subcommand (JSON 输出)**：`install-to-opencode` 生成 JSON 配置片段，描述 `repo-wiki search` 为 OpenCode MCP tool。
   - 优点：零额外依赖，CLI 已有
   - 缺点：每次搜索启动新的 Rust 进程（~200ms 冷启动）

B. **轻量 HTTP server**：`repo-wiki serve` 启动 HTTP 端口，OpenCode MCP tool 配置为 HTTP endpoint。
   - 优点：常驻进程，搜索更快
   - 缺点：需要 HTTP 依赖（可能已有：reqwest）

C. **OpenCode Rust plugin**：直接用 Rust 编译为 OpenCode 插件。
   - 优点：最低延迟
   - 缺点：耦合 opencode 插件协议

### 影响范围

- `src/main.rs` — install-to-opencode 或 serve 子命令
- `.opencode/` — 配置输出

### 验证标准

运行 `install-to-opencode` 后 OpenCode 可调用 `search_code` 工具返回搜索结果。
