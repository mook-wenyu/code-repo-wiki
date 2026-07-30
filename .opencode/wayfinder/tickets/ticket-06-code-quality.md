# Ticket: 代码质量修复

## Question

批处理 6 个独立 P1/P2 问题，均为 1 行改动或简单修改，互不依赖：

| # | 严重度 | 文件 | 行 | 问题 | 修复 |
|---|--------|------|-----|------|------|
| 1 | P1.8 | `impact.rs` | 29 | `fp.contains(cfp)` 子串匹配误报 | 改为 `fp == cfp` 精确匹配 |
| 2 | P1.11 | `crossref.rs` | 55 | `r.target_path.contains(target)` 子串误报 | 改为精确路径匹配 |
| 3 | P1.9 | `html.rs` | 195 | `Options::all()` 启用非预期扩展 | 显式 `Options::ENABLE_TABLES \| ENABLE_FENCED_CODE_BLOCKS` |
| 4 | P1.5 | `mermaid.rs` | 66,107 | `render_entity_graph`/`render_cycle_diagram` 死代码 | 删除或加 `#[allow(dead_code)]` |
| 5 | P2.14 | `ast_chunker.rs` | 全部 | 161 行死代码 | 加 `#[allow(dead_code)]` |
| 6 | P2.6 | `state.rs` | 2 | `use std::io::Read` 未使用导入 | 删除导入 |
| 7 | P1.7 | `agent.rs` + `lib.rs` | 全文件 | SearchAgent 未接入 `execute_search()` | `execute_search` 新增 `--agent` 模式，委托给 `SearchAgent` |

**估算**: 总共 ~30 行修改，完全独立，可并行实施。

## Tickets this blocks

无

## Assets

`docs/full-audit-report.md:98-107` — P1 问题清单
