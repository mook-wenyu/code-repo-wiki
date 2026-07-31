---
description: 管理知识卡片（生成、修改、补充、重写）
---

# 知识卡片管理

对 repo-wiki 知识卡片执行操作。参数（$ARGUMENTS）格式：
`<动作> <模块名> <指令文本> [参考文件]`

动作与工具映射：
- `generate <模块名>`：调用 `card_generate` 工具
- `modify <模块名> <指令>`：调用 `card_modify` 工具
- `supplement <模块名> <指令>`：调用 `card_supplement` 工具
- `rewrite <模块名> <指令>`：调用 `card_rewrite` 工具

参考文件规则（重要）：
- 指令中 @ 引用的文件（如 `@docs/design.md`）应提取其**路径**放入工具调用的
  `reference` 数组（reference 是路径列表，不是文件内容）
- 用户显式给出的路径同样放入 `reference` 数组
- 模块名缺省或指令为空时，提示用户正确用法，不调用工具
