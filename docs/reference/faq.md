# FAQ

## 没有 API key 能跑吗？

能。无 key 时 LLM 降级为本地模拟内容、语义搜索降级为纯文本，全流程不中断。`status` 显式显示「LLM: 已降级（mock 模拟）」/「语义索引已降级」。

## 产物在哪里？

`.code-repo-wiki/`：`wiki/{lang}/` 文档页（模块页 + api.md + architecture.md + overview.md）、`cards/` 知识卡片、`llms.txt` / `llms-full.txt` Agent 索引。

## 手动改过文档会被覆盖吗？

不会。人工修改过的页面自动加入保护集（SHA256 指纹），后续更新跳过；`generate --force` 清空保护集。人工修改会反向同步到卡片（`pending_manual_edits`）。

## 文档过时了？

`code-repo-wiki lint` 检查断链/过时/引用错位/坏 mermaid；`doctor` 检测二进制与产物版本漂移。日常 commit 已自动增量更新（装了 hook 的话）。

## 大仓库跑得动吗？

单项目上限 10 万个源文件；单次变更超 1 万行自动回退全量生成；16 路并发 LLM 调用。实测（mock LLM，v30）：cal.com（5048 文件/5.9 万实体）全量约 6.2 分钟。详见[限制项](limitations.md)。

## 会泄露我的代码/密钥吗？

只把模块内实体清单/文件路径发给 LLM（不发全文件）；API key 只从环境变量或用户级配置读取，项目级配置写 `api_key_env` 变量名而非明文。

## 非 git 仓库能用吗？

能。增量更新基于内容指纹（SHA256）而非 git 依赖，非 git 仓库同样支持（git hooks 集成除外——非 git 仓库跳过 hooks 安装）。

## 手动删除了产物目录会怎样？

下次 `generate` / `update` 自动重建（指纹库随之重建）。`install` 的 hooks 不依赖产物存在。

## watch 和 hook 都开着会重复更新吗？

watch 与 hook 各自独立触发增量；单实例运行锁（`.state/run.lock`）保证并发调用时后到者显式报错而非互相踩写（见[限制项](limitations.md)）。
