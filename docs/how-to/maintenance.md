# 运维指南：维护

## 测试残留清理

历史测试可能遗留无意义的临时目录（`%USERPROFILE%\.code-repo-wiki\key-test-*` 与 `%TEMP%\repo_wiki*` / `code_repo_wiki*`，≈0B 占用、无敏感数据）。需要清理时：

```powershell
powershell -File scripts/cleanup-test-residue.ps1          # 先预览
powershell -File scripts/cleanup-test-residue.ps1 -Apply   # 确认后删除
```

## 发布新版本

**前置（首次发布必做；后续发布仅版本号变更）**：

- `Cargo.toml` 元数据完备——`description` + `license`（SPDX 表达式，如 `Apache-2.0`）为
  crates.io **硬性必填**（缺失即 400 拒绝发布）；`repository` / `readme` / `keywords`
  （≤5 个，ASCII ≤20 字符）/ `categories`（≤5 个，slug 必须精确匹配
  [crates.io category_slugs](https://crates.io/category_slugs)，未知 slug 即 400）为推荐项；
  `license-file` 与 `license` 不要同时填（cargo 警告）
- 发布前核对：`cargo package --list`（打包文件集——git 仓库遵循 .gitignore，`target/` 恒排除、
  `Cargo.lock` 恒包含）+ `cargo publish --dry-run`（元数据校验）
- crates.io 账号已验证邮箱；**新 crate 首次必须手动 `cargo publish`**
  （Trusted Publishing 不支持首次发布，且需先在 crates.io 侧启用）

**流程**：SemVer 更新 `Cargo.toml` → `cargo test` / `clippy` / `machete` 全绿 →
`cargo publish --dry-run` → 首次：手动 `cargo publish`；后续：推送 `git tag vX.Y.Z`
触发 release 工作流（`.github/workflows/release.yml`——crates.io 发布 + GitHub Releases
二进制矩阵，linux/macos/windows 四目标）→ 干净环境 `cargo install code-repo-wiki` +
`doctor` 六查。

## 维护者日常

- **搜索**：`code-repo-wiki search --query "k" --engine hybrid --top-k 10`；语义索引无 Key 自动降级
- **AI Agent 入口**：`llms.txt`（站点地图）+ `llms-full.txt`（含实体签名的内联索引），随生成确定性重写
- **评测**：`code-repo-wiki bench --root <repo> [--judge]` 自动评测；`bench --repodoc` 五维聚合报告；`bench-manifest` 清单批量跑分（详见[CLI 参考](../reference/cli.md)）
- **CI**：GitHub Actions 五 job——check（fmt + clippy `-D warnings` + `cargo doc` 门禁）、test（ubuntu + windows + macos 三平台矩阵）、lint-workflow（actionlint 校验 workflow 文件）、lint-docs（markdownlint-cli2 + lychee 死链检查）、lint-artifacts（PR 常驻产物门禁：mock provider 生成 sample-repo 产物后对磁盘产物 lint，零 key 零触网，Error 级阻断/Warning 级仅展示）；`rust-toolchain.toml` 钉定 stable + clippy
