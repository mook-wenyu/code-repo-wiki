# 技术研究结果

> 研究日期：2026-07-30

---

## 1. sqlite-vec

### 版本号
- **最新稳定版**：`0.1.9`（crates.io, 发布于 2026-07-05）
- **最新版**：`0.1.10-alpha.4`
- **总下载量**：213 万+
- **仓库**：https://github.com/asg017/sqlite-vec
- **主页**：https://alexgarcia.xyz/sqlite-vec/
- 来源：https://crates.io/crates/sqlite-vec

### Windows 编译支持
> "Written in pure C, no dependencies, runs anywhere SQLite runs (Linux/MacOS/Windows, in the browser with WASM, Raspberry Pis, etc.)"

sqlite-vec 用纯 C 编写，无外部依赖。Rust crate 通过 `cc` crate 编译并静态链接 C 源码，Windows 上需要 MSVC 工具链（与 rusqlite `bundled` 模式要求一致）。确认 Windows 是官方支持的平台。

### API 接口

**创建 vec0 表：**
```sql
create virtual table vec_examples using vec0(
  sample_embedding float[8]
);
```
支持的向量类型：`float[N]`、`int8[N]`、`bit[N]`。可指定距离度量：`distance_metric=cosine`（默认 L2）。

**插入向量（JSON 或二进制格式）：**
```sql
insert into vec_examples(rowid, sample_embedding)
  values (1, '[-0.200, 0.250, 0.341, -0.211, 0.645, 0.935, -0.316, -0.924]');
```

**KNN 查询（k 近邻）：**
```sql
select rowid, distance
from vec_examples
where sample_embedding match '[0.890, 0.544, 0.825, 0.961, 0.358, 0.0196, 0.521, 0.175]'
  and k = 10
order by distance;
```
SQLite 3.41+ 也可用 `LIMIT 10` 代替 `k = 10`。

**元数据列、分区键、辅助列：**
```sql
create virtual table vec_chunks using vec0(
  document_id integer primary key,
  contents_embedding float[768],
  user_id integer partition key,   -- 分区键（WHERE 中 = 约束可预过滤）
  label text,                       -- 元数据列（WHERE 中支持 =, <, >, BETWEEN）
  +contents text                    -- 辅助列（+ 前缀，不能出现在 WHERE 但可在 SELECT 中返回）
);
```

### 与 FTS5 共存
sqlite-vec 通过 `sqlite3_auto_extension()` 注册到 SQLite 连接。由于它仅注册 SQL 函数和虚拟表，且 FTS5 是 SQLite 内建模块，两者在同一连接上完全兼容——只需都注册后，在同一个 SQLite 连接上同时使用 `match`（FTS5）和 `match`（vec0）即可。

### Cargo.toml 示例
```toml
[dependencies]
sqlite-vec = "0.1.9"
rusqlite = { version = "0.32", features = ["bundled"] }
zerocopy = { version = "0.8", features = ["zerocopy-derive"] }  # 推荐
```

Rust 注册代码：
```rust
use sqlite_vec::sqlite3_vec_init;
use rusqlite::{ffi::sqlite3_auto_extension, Connection};

fn main() -> rusqlite::Result<()> {
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite3_vec_init as *const (),
        )));
    }
    let db = Connection::open_in_memory()?;
    // 现在 sqlite-vec 函数和 vec0 虚拟表都可用
    db.execute_batch("create virtual table v using vec0(embedding float[4])")?;
    Ok(())
}
```

来源：https://alexgarcia.xyz/sqlite-vec/rust.html

---

## 2. FTS5 BM25 + sqlite-vec RRF 混合搜索

### RRF 标准 k 值
标准 k = **60**（来自 Cormack et al. 2009 原论文，被 Elasticsearch、Azure AI Search、Apache Doris 等广泛采纳）。

### RRF 公式
```
RRF_score(doc) = Σ 1 / (k + rank_i(doc))
```
其中 `rank_i(doc)` 是文档在列表 i 中的排名（从 1 开始），`k = 60`。

### FTS5 的 bm25() 分数
FTS5 的 `bm25()` 函数返回 BM25 分数，范围无上限（通常 0~25 间，取决于语料库）。用法：
```sql
-- FTS5 查询
SELECT rowid, rank
FROM docs_fts
WHERE docs_fts MATCH ?
ORDER BY rank;
```
在 FTS5 中，`rank` 是一个特殊列名，对应 bm25 分数。也可显式调用 `bm25(fts_table, 0, 1, ...)` 来指定每个列的权重。

### 归一化方案
RRF 的**核心理念是不需要归一化**——它只依赖排名而非原始分数。这是 RRF 比加权平均更健壮的原因：
- BM25 分数范围：无上限（0~25+）
- Cosine 距离范围：0~2（cosine）或 0~∞（L2）
- 直接平均会使数值范围大的方法主导结果

RRF 方案完全消除了对分数归一化的需求，因此推荐直接用 RRF 合并排名。

### SQL 实现模式
```sql
WITH bm25 AS (
  SELECT rowid, ROW_NUMBER() OVER (ORDER BY rank) AS r
  FROM docs_fts
  WHERE docs_fts MATCH ?
  LIMIT 50
),
vec AS (
  SELECT rowid, ROW_NUMBER() OVER (ORDER BY distance) AS r
  FROM vec_docs
  WHERE embedding MATCH ?
  LIMIT 50
)
SELECT
  COALESCE(b.rowid, v.rowid) AS rowid,
  COALESCE(1.0 / (60.0 + b.r), 0.0) +
  COALESCE(1.0 / (60.0 + v.r), 0.0) AS rrf_score
FROM bm25 b
FULL OUTER JOIN vec v ON b.rowid = v.rowid
ORDER BY rrf_score DESC
LIMIT 10;
```

关键参数：
- `k = 60`：标准值，低值（20-40）放大 top 排名影响，高值（80+）扁平化
- `LIMIT 50`：每个检索器的候选池大小，NRR 只看列表顶部
- 可加权：`1.5 * COALESCE(1.0/(60+bm25.r), 0)`（仅在确有必要时）
- 性能：每个检索器各一次索引扫描 + JOIN，p95 约 24ms（单检索器约 12-18ms）

来源：
- https://ceaksan.com/en/hybrid-search-fts5-vector-rrf
- https://learn.microsoft.com/en-us/azure/search/hybrid-search-ranking
- https://doris.apache.org/docs/dev/key-features/reciprocal-rank-fusion
- https://www.elastic.co/guide/en/elasticsearch/reference/8.19/rrf.html
- https://hoangtuan.me/blog/hybrid-search-fts-vector-postgres

---

## 3. OpenCode MCP Plugin 配置

### V2 版本 opencode.json 结构
```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "timeout": {
      "startup": 45000,
      "catalog": 30000,
      "execution": 600000
    },
    "servers": {
      "my-server": {
        "type": "local",
        "command": ["npx", "-y", "example-mcp-server"],
        "cwd": ".",
        "environment": {
          "API_KEY": "{env:MY_API_KEY}"
        },
        "disabled": false,
        "codemode": true,
        "timeout": {
          "catalog": 60000
        }
      }
    }
  },
  "plugins": [
    "opencode-example-plugin",
    {
      "package": "./plugins/local.ts",
      "options": { "enabled": true }
    }
  ]
}
```

### 注册 MCP 命令
- 本地服务器：`type: "local"`，`command` 是执行命令数组
- 远程服务器：`type: "remote"`，`url` 是 Streamable HTTP 端点
- CLI 快速添加：`opencode2 mcp add my-server -- npx -y example-mcp-server`
- 环境变量引用：`{env:VAR_NAME}`
- 禁用：`disabled: true`
- 工具名规范化：`server_name_tool_name`

### OpenCode Plugin（V2 beta）

**加载方式：**
```json
{
  "plugins": [
    "./plugins/local.ts",
    { "package": "@my-org/custom-plugin", "options": { "key": "val" } }
  ]
}
```
- 自动扫描 `.opencode/plugins/` 和 `~/.config/opencode/plugins/`
- `.ts`/`.js` 文件直接加载（支持 deno）
- 子目录以 npm 包方式解析

**插件结构：**
```typescript
import { Plugin } from "@opencode-ai/plugin";

export default Plugin.define({
  id: "my.plugin",
  setup: async (ctx) => {
    // ctx.options — 用户配置的 options
    // ctx.tool.transform — 注册工具
    // ctx.session.hook — 拦截请求
    // ctx.tool.hook — 拦截工具执行
    // ctx.catalog.transform — 修改模型/提供商
  },
});
```

**添加工具：**
```typescript
await ctx.tool.transform((tools) => {
  tools.add(
    "greeting",
    {
      description: "Create a greeting",
      input: { /* JSON Schema */ },
      execute: async ({ name }) => {
        return { output: { greeting: text }, content: text };
      },
    },
    { namespace: "my-plugin", codemode: true }
  );
});
```

**发送进度反馈：**
当前 V2 plugin API 文档中未直接暴露 `progress` 或 `notification` 方法。可用的钩子是 `ctx.tool.hook("execute.before", cb)` 和 `ctx.tool.hook("execute.after", cb)`。对于长时间运行的操作，建议通过 `ctx.session` 的 `interrupt`、`synthetic` 方法或分割步骤来间接实现进度反馈（MCP 协议本身的 `$/progress` 通知需要 MCP 服务器实现，但目前 opencode V2 plugin 文档未明示该功能）。

**配置目录发现规则：**
- 全局：`~/.config/opencode/opencode.json(c)`
- 项目：`<project>/opencode.json(c)` 或 `<project>/.opencode/opencode.json(c)`
- 从工作目录向上搜索合并，`.opencode/` 优先级高于直接 `opencode.json`

来源：
- https://opencode.ai/v2/docs/config
- https://opencode.ai/v2/docs/build/plugins
- https://opencode.ai/v2/docs/mcp-servers

---

## 4. tokio Runtime in CLI Tools

### OnceLock<Runtime> 是否推荐
**是，对于 CLI 工具是推荐模式**。当：
- 有同步入口点（`fn main()`），但部分操作需要异步
- 需要多个函数共享同一个 tokio runtime
- 不想在每个同步函数中传递 `&Handle`

标准模式：
```rust
use std::sync::OnceLock;
use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn get_runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        Runtime::new().expect("Failed to create tokio runtime")
    })
}

fn sync_fn_needs_async() {
    get_runtime().block_on(async {
        // 异步代码
    });
}
```

### lazy_static vs OnceLock（Rust 1.80+）
| 特性 | lazy_static | OnceLock（std） | LazyLock（std） |
|------|-------------|-----------------|-----------------|
| 需要 crate | 是 | 否（std） | 否（std） |
| 宏语法 | `lazy_static! { static ref X: T = ...; }` | 无宏，纯类型 | 无宏，纯类型 |
| 初始化时机 | 首次 deref | `get_or_init()` 调用时 | 首次 deref |
| 异步初始化 | 不直接支持 | 需同步 block_on | 不直接支持 |
| 灵活性 | 固定初始化函数 | 可延迟到运行时决定 | 固定初始化函数 |
| 推荐度 | 旧项目兼容 | **推荐**（新项目首选） | 无参数初始化时首选 |

**Rust 1.80+ 已稳定** `LazyLock` 和 `OnceLock`，建议新项目使用 std 类型而非 `lazy_static` 或 `once_cell` crate。`LazyLock` 适合简单场景（初始化函数在声明时可指定），`OnceLock` 适合需要在运行时决定初始化参数或可能失败的场景。

### 一个 Runtime 足够的场景
**是的，绝大多数 CLI 工具一个 Runtime 就够。**
- tokio 的 `Runtime` 内部自带线程池（默认 worker 数 = CPU 核数）
- `block_on` 可以在同步代码中驱动异步任务
- 多个并发任务由同一个 runtime 的 worker 线程处理
- `Runtime::new()` 创建 multi-thread runtime

**不需要多个 Runtime 的迹象：**
- 所有异步操作在同一个线程池中性能足够
- 没有需要隔离的长时间运行 task
- 不需要不同的 `tokio::runtime::Builder` 配置（如不同线程数）

**需要多个 Runtime 的罕见场景：**
- 需要隔离不同优先级的任务
- 并行运行多个阻塞性 runtime 且不希望互相影响
- 嵌入式环境需要严格控制资源

来源：
- https://doc.rust-lang.org/stable/std/sync/struct.OnceLock.html
- https://docs.rs/tokio/latest/tokio/sync/struct.OnceCell.html
- https://blog.logrocket.com/how-use-lazy-initialization-pattern-rust-1-80/
- https://users.rust-lang.org/t/async-initializer-in-lazylock/115705

---

## 5. tree-sitter Query Patterns

### 查询语法基础
tree-sitter 查询使用 S-表达式匹配语法树节点：

```
(node_type
  field_name: (child_type) @capture_name)
```

特殊符号：
- `_` — 通配符（匹配任何节点，包括匿名节点）
- `(_)` — 命名节点通配符
- `?` — 可选标记
- `*` / `+` — 量词（零次或多次 / 一次或多次）
- `.` — 锚点（约束为直接兄弟节点）
- `[...]` — 匹配多个候选项之一
- `(#eq? @cap "value")` — 谓词匹配

### 跨语言查询函数定义

**Python (function_definition):**
```
(function_definition
  name: (identifier) @name) @definition.function
```

**Rust (function_item):**
```
(function_item
  name: (identifier) @name) @definition.function
```

**Go (function_declaration):**
```
(function_declaration
  name: (identifier) @name) @definition.function
```

**JavaScript/TypeScript (function_declaration / method_definition / arrow_function):**
```
[
  (function_declaration
    name: (identifier) @name)
  (method_definition
    name: (property_identifier) @name)
  (arrow_function)  ; 匿名箭头函数
] @definition.function

; 变量赋值的函数
(assignment_expression
  left: (identifier) @name
  right: [(arrow_function) (function)]) @definition.function
```

**统一查询策略：** 定义 `@definition.function` 和 `@definition.method` 标签，然后用 `#any-of?` 或分语言配置。

### 查询调用表达式

**通用 call_expression 模式：**
```
(call_expression
  function: (identifier) @function.name
  arguments: (arguments) @function.args) @reference.call
```

**方法调用：**
```
(call_expression
  function: (member_expression
    property: (property_identifier) @method.name)) @reference.call
```

**过滤特定函数名：**
```
(
  (call_expression
    function: (identifier) @function.name)
  (#eq? @function.name "deserialize")
)
```

**可选参数捕获：**
```
(call_expression
  function: (identifier) @function.name
  arguments: (arguments (string_literal)? @string.arg))
```

### 跨语言最佳实践

1. **使用 field name**：指定 field 名（如 `function:`、`name:`、`body:`）让查询精确，避免模糊匹配
2. **通配符降噪**：不需要精确子节点路径时用 `(_)` 跳过中间节点
3. **分语言粒度**：不同语言的语法树结构差异大（如 Rust 用 `function_item`，Python 用 `function_definition`），建议按语言配置不同查询
4. **捕获角色规范**：遵循 tree-sitter 标签约定：
   - `@definition.function` / `@definition.method` / `@definition.class`
   - `@reference.call` / `@reference.class` / `@reference.implementation`
5. **谓词过滤**：`#eq?`、`#match?`、`#any-of?` 用于内容级过滤
6. **错误处理**：查询不匹配不会报错——只是不返回结果，因此可以安全地叠加多个语言查询

来源：
- https://tree-sitter.github.io/tree-sitter/using-parsers/queries/1-syntax.html
- https://tree-sitter.github.io/tree-sitter/using-parsers/queries/2-operators.html
- https://tree-sitter.github.io/tree-sitter/4-code-navigation.html
- https://parsiya.net/blog/knee-deep-tree-sitter-queries/
