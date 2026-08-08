# lint 检查项

`code-repo-wiki lint` 对磁盘上的产物做静态健康检查（对齐 LLM Wiki 最佳实践：Karpathy 的 lint 健康检查、Econowiz 的孤儿页 lint）。退出码：`0` 干净 / `1` 有问题 / `2` 配置或环境错误（CI 可用）。

| kind | 含义 | 发射点 |
|---|---|---|
| `orphan` | 孤儿页：没有任何其他页面链接指向的模块页（无人可达 = 可能过期/重复） | `src/output/lint.rs` |
| `broken` | 断链：页面内链接指向不存在的产物文件 | 同上 |
| `stale` | 过时：页面时间戳早于源文件修改时间（源码已变文档未更新） | 同上 |
| `bad-citation` | 正文 `path:line` 引用指向不存在的文件或行号越界（引用契约的静态复核，3 个发射点） | 同上 |
| `bad-citation-overlap` | 行号对但内容错：引用行区间与实体表行区间不重叠 | 同上 |
| `bad-vctx` | 正文 `[[vctx:path#L-a-L-b@hash8]]` 手工标记 5 步哈希只读校验失败（vericontext 协议，5 个发射点） | 同上 |
| `entity-coverage` | 页面声称的实体不在 api.md 权威清单（LLM 编造的第二道闸） | 同上 |
| `stale-entity` | api.md 权威清单的实体在当前源码中不存在（文档引用了已删除/重命名的符号） | 同上 |
| `bad-mermaid` | 产物中的 mermaid fence 无法被 merman 解析（历史产物/人工编辑/增量遗留） | 同上 |

## 已知噪声

`entity-coverage` 会把**模块名引用**（api.md 的 `##` 节标题）判为不在实体清单——合成页（architecture/overview）按模块名引用属已知模式，不是 LLM 编造；人工复核时按此排除即可。

检查对象是磁盘产物（真实用户看到的东西），而非内存中的文档对象。
