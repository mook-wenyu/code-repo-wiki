use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::incremental::state::GenerationState;

/// 将产物目录（wiki/{lang}/、cards/{lang}/）的工作区内容同步到指纹库
///
/// 官方语义：Git 目录中直接编辑 .md 后，手动触发"同步"以 Git 内容为准
/// 合入 Wiki（团队 pull 共享知识的场景）。对每个 .md 产物：
/// - 指纹不存在 → 记录新指纹（视为新文件）
/// - 指纹不匹配 → 工作区内容为准，更新指纹
/// - 受保护页面（protected_docs 含该路径）→ 跳过，保留人工版
///
/// 本质上 = 重新加载工作区 .md 内容到指纹库，不触发任何 LLM 生成。
pub fn sync_from_git(output_dir: &Path) -> Result<()> {
    let state_dir = output_dir.join(".state");
    // 无既有状态时从空状态开始：所有产物视为新文件，全部记录指纹
    let mut state = GenerationState::load(&state_dir).unwrap_or_else(|_| GenerationState {
        last_commit_hash: None,
        file_fingerprints: HashMap::new(),
        module_fingerprints: HashMap::new(),
        doc_fingerprints: HashMap::new(),
        protected_docs: Vec::new(),
        generated_at: chrono::Utc::now().to_rfc3339(),
    });

    let mut updated = 0usize;
    let mut skipped = 0usize;
    for root in [output_dir.join("wiki"), output_dir.join("cards")] {
        for path in collect_md_files(&root) {
            let path_str = path.to_string_lossy().to_string();
            if state.protected_docs.iter().any(|p| p == &path_str) {
                tracing::warn!("跳过受保护页面（保留人工版）: {}", path_str);
                skipped += 1;
                continue;
            }
            let fp = GenerationState::compute_file_fingerprint(&path)?;
            if state.doc_fingerprints.get(&path_str) != Some(&fp) {
                state.doc_fingerprints.insert(path_str, fp);
                updated += 1;
                tracing::info!("同步指纹（工作区内容为准）: {}", path.display());
            }
        }
    }

    state.save(&state_dir)?;
    tracing::info!("同步完成: 指纹更新 {} 个, 跳过受保护 {} 个", updated, skipped);
    Ok(())
}

/// 递归收集目录下所有 .md 文件（目录不存在时返回空列表）
fn collect_md_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            out.extend(collect_md_files(&p));
        } else if p.extension().is_some_and(|e| e == "md") {
            out.push(p);
        }
    }
    out
}

/// repo-wiki 安装: 配置 Agent 插件 + git hooks + 默认配置
pub fn install(agent: &str) -> Result<()> {
    let project_root = std::env::current_dir()?;

    // 1. 配置 OpenCode 插件 (如果 agent 是 opencode)
    if agent == "opencode" {
        let mut oc = crate::config::opencode::OpenCodeConfig::new()
            .context("读取 OpenCode 配置失败")?;
        oc.install_plugin()?;
        println!("✓ OpenCode 插件已安装");
    }

    // 2. 创建默认 .repo-wiki/config.toml (如果不存在)
    let config_path = project_root.join(".repo-wiki").join("config.toml");
    if !config_path.exists() {
        std::fs::create_dir_all(config_path.parent().unwrap())?;
        let default_config = include_str!("../default-config.toml");
        std::fs::write(&config_path, default_config)?;
        println!("✓ 默认配置已创建: .repo-wiki/config.toml");
    }

    // 3. 安装 git hooks
    let hooks_dir = project_root.join(".git").join("hooks");
    let hook_content = "#!/bin/sh\n# repo-wiki: auto-update wiki on commit\ncd \"$(git rev-parse --show-toplevel)\"\nrepo-wiki update --quiet 2>/dev/null || true\n";
    if hooks_dir.exists() {
        let post_commit = hooks_dir.join("post-commit");
        if !post_commit.exists() {
            std::fs::write(&post_commit, hook_content)?;
            #[cfg(unix)]
            std::fs::set_permissions(&post_commit, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
            println!("✓ git post-commit hook 已安装");
        }

        let post_merge = hooks_dir.join("post-merge");
        if !post_merge.exists() {
            std::fs::write(&post_merge, hook_content)?;
            #[cfg(unix)]
            std::fs::set_permissions(&post_merge, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
            println!("✓ git post-merge hook 已安装");
        }
    }

    println!("✓ repo-wiki 安装完成");
    Ok(())
}

/// repo-wiki 卸载: 移除 Agent 插件 + git hooks + 可选数据
pub fn uninstall(force: bool) -> Result<()> {
    let project_root = std::env::current_dir()?;

    if !force {
        println!("警告: 卸载将移除 repo-wiki 集成配置。");
        println!("数据目录 .repo-wiki/ 不会被删除（使用 --force 跳过确认）。");
        anyhow::bail!("请添加 --force 参数确认卸载");
    }

    // 1. 移除 OpenCode 插件
    let mut oc = crate::config::opencode::OpenCodeConfig::new()
        .context("读取 OpenCode 配置失败")?;
    oc.uninstall_plugin()?;
    println!("✓ OpenCode 插件已移除");

    // 2. 移除 git hooks
    let hooks_dir = project_root.join(".git").join("hooks");
    for hook_name in &["post-commit", "post-merge"] {
        let hook_path = hooks_dir.join(hook_name);
        if hook_path.exists() {
            let content = std::fs::read_to_string(&hook_path).unwrap_or_default();
            if content.contains("repo-wiki") {
                std::fs::remove_file(&hook_path)?;
                println!("✓ git {} hook 已移除", hook_name);
            }
        }
    }

    println!("✓ repo-wiki 卸载完成 (数据保留: .repo-wiki/)");
    Ok(())
}
