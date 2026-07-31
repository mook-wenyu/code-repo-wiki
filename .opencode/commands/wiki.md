---
description: 生成、更新或同步项目 Wiki
---

# Wiki 管理

对 repo-wiki 项目 Wiki 执行管理操作。根据参数（$ARGUMENTS）选择动作并调用对应工具：

- 全量生成：调用 `wiki_generate` 工具（`output` 可选）
- 增量更新（默认）：调用 `wiki_update` 工具
- Git 同步：调用 `wiki_sync` 工具（以 Git 工作区内容为准合入指纹库，不触发 LLM 重生成）
- 查看状态：调用 `wiki_status` 工具
- 导出：调用 `wiki_export` 工具

规则：
- $ARGUMENTS 为空时默认执行 `wiki_update`
- $ARGUMENTS 含 "generate" 时执行 `wiki_generate`，含 "sync" 时执行 `wiki_sync`，
  含 "status" 时执行 `wiki_status`，含 "export" 时执行 `wiki_export`
- 执行后向用户简要报告命令结果
