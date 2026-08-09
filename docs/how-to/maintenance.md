# 运维指南：维护

## 测试残留清理

历史测试可能遗留无意义的临时目录（`%USERPROFILE%\.code-repo-wiki\key-test-*` 与 `%TEMP%\repo_wiki*` / `code_repo_wiki*`，≈0B 占用、无敏感数据）。需要清理时：

```powershell
powershell -File scripts/cleanup-test-residue.ps1          # 先预览
powershell -File scripts/cleanup-test-residue.ps1 -Apply   # 确认后删除
```

## 发布新版本

SemVer 更新 `Cargo.toml` → `cargo test` / `clippy` / `machete` 全绿 → `cargo publish` + `git tag vX` → 干净环境 `cargo install code-repo-wiki` + `doctor` 六查。

## 维护者日常

- **搜索**：`code-repo-wiki search --query "k" --engine hybrid --top-k 10`；语义索引无 Key 自动降级
- **AI Agent 入口**：`llms.txt`（站点地图）+ `llms-full.txt`（含实体签名的内联索引），随生成确定性重写
- **评测**：`code-repo-wiki bench --root <repo> [--judge]` 自动评测；`bench --repodoc` 五维聚合报告；`bench-manifest` 清单批量跑分（详见[CLI 参考](../reference/cli.md)）
- **CI**：GitHub Actions 三 job——check（clippy `-D warnings` + `cargo doc` 门禁）、test（ubuntu + windows 矩阵）、lint-workflow（actionlint 校验 workflow 文件）；`rust-toolchain.toml` 钉定 stable + clippy
