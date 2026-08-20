# code-repo-wiki DSH Bundle

DeepSeek Harness (dsh) 的 code-repo-wiki MCP 集成包。

## 功能说明

本 bundle 为 DeepSeek Harness 会话注册 code-repo-wiki MCP 服务器，提供以下代码智能工具：

| 工具 | 用途 |
|---|---|
| `wiki_search` | 按关键词检索代码实体（定位函数/结构体/类定义或引用，hybrid 模式含调用链补全） |
| `wiki_ast_search` | 精确符号定义查找（全量 AST 扫描，返回文件 + 行号 + 签名） |
| `wiki_status` | Wiki 状态报告（是否就绪、语义索引降级、lint 问题） |
| `wiki_read_page` | 读取模块页/架构/概览/API 页面正文 |
| `wiki_read_card` | 读取知识卡片（模块结构化摘要） |
| `wiki_get_dependencies` | 查询模块依赖关系（被谁依赖 / 依赖谁） |

## 安装方法

### 方法一：从 npm 安装（推荐）

```bash
dsh plugin add code-repo-wiki-dsh-bundle
```

### 方法二：从本地路径安装

```bash
dsh plugin add ./path/to/dsh-bundle
# 或绝对路径
dsh plugin add D:\RustProjects\repo-wiki\dsh-bundle
```

### 方法三：手动配置

将 `cordis.patch.yml` 的内容合并到你 dsh profile 的 `cordis.patch.yml` 中：

```yaml
- insert:
    - id: code-repo-wiki
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        serverName: code-repo-wiki
        transport: stdio
        command: "code-repo-wiki"
        args: [mcp]
        cwd: !!js process.cwd()
```

## 前置条件

1. **code-repo-wiki 二进制文件必须在 PATH 中**
   - 安装方式：`cargo install code-repo-wiki`
   - 验证：`code-repo-wiki --version`

2. **目标仓库已生成 Wiki**
   - 在目标仓库中运行：`code-repo-wiki generate`
   - 验证：`code-repo-wiki status`

## 配置选项

| 配置项 | 默认值 | 说明 |
|---|---|---|
| `serverName` | `code-repo-wiki` | MCP 服务器名称 |
| `transport` | `stdio` | 传输方式 |
| `command` | `code-repo-wiki` | 可执行文件路径 |
| `args` | `[mcp]` | 启动参数 |
| `cwd` | 当前工作目录 | 工作目录（自动检测） |

## 使用示例

安装后，在 dsh 会话中可直接使用 MCP 工具：

```
# 搜索函数定义
使用 wiki_search 搜索 "load_config"

# 精确查找符号
使用 wiki_ast_search 查找 "Config" 结构体

# 检查 Wiki 状态
使用 wiki_status

# 读取模块文档
使用 wiki_read_page 读取 "src_config" 模块
```

## 故障排除

### 问题：MCP 服务器连接失败

**症状**：dsh 会话中无法使用 wiki_* 工具

**解决步骤**：

1. 确认 code-repo-wiki 已安装：
   ```bash
   which code-repo-wiki  # Linux/macOS
   where code-repo-wiki  # Windows
   ```

2. 确认目标仓库已生成 Wiki：
   ```bash
   cd /path/to/your/repo
   code-repo-wiki status
   ```

3. 检查 dsh 插件列表：
   ```bash
   dsh plugin list
   ```

4. 重新安装插件：
   ```bash
   dsh plugin remove code-repo-wiki-dsh-bundle
   dsh plugin add code-repo-wiki-dsh-bundle
   ```

### 问题：工具返回空结果

**症状**：wiki_search 或 wiki_ast_search 返回空

**解决**：确保目标仓库已运行 `code-repo-wiki generate`，且 `.code-repo-wiki/` 目录存在。

### 问题：权限错误

**症状**：MCP 服务器启动失败

**解决**：检查 code-repo-wiki 二进制文件权限：
```bash
chmod +x $(which code-repo-wiki)
```

## 开发说明

本 bundle 是纯配置层，不包含运行时逻辑。实际的 MCP 连接由 `@deepseek-ai/dsh-mcp-client` 插件处理。

### 文件结构

```
dsh-bundle/
├── package.json        # npm 包配置
├── cordis.patch.yml    # dsh 插件注册配置
├── index.js            # bundle 入口（仅日志）
└── README.md           # 本文档
```

## 许可证

Apache-2.0
