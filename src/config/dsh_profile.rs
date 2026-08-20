//! dsh profile 自动合入模块（W4）
//!
//! 扫描 `$DSH_HOME/profiles/` 下所有 profile，检测每个 profile 的
//! `cordis.patch.yml` 是否已包含 code-repo-wiki 条目。未包含时自动
//! 合入（追加管理块），已包含时跳过（幂等）。
//!
//! dsh 的配置层叠机制（官方文档）：
//! 1. bundle patches（profile 的 `dsh.profile.bundles` 列表）
//! 2. profile 的 `cordis.patch.yml`
//! 3. home 级 `$DSH_HOME/cordis.patch.yml`
//! 4. `--patch` 覆盖层
//!
//! 本模块操作第 2 层（profile 级 cordis.patch.yml），使 code-repo-wiki
//! MCP server 在 profile 启动时自动加载。
//!
//! 设计决策：
//! - 只操作 profile 级 patch，不触碰 home 级（home 级是机器级偏好，
//!   profile 级是用户级配置，更安全）
//! - 使用与 DshMcp 相同的管理标记（DSH_BLOCK_START/DSH_BLOCK_END），
//!   保证 install/uninstall 对称清理
//! - 幂等：已存在管理块或裸行时跳过

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::mcp::{DSH_BLOCK_END, DSH_BLOCK_START, DshMcp};

/// dsh home 目录检测结果
pub struct DshHome {
    /// dsh home 路径（`$DSH_HOME` 或 `~/.dsh`）
    pub path: PathBuf,
    /// profiles 目录是否存在
    pub profiles_exist: bool,
}

/// 检测 dsh home 目录
///
/// 优先级：`$DSH_HOME` 环境变量 → `~/.dsh`（USERPROFILE/HOME）
/// 返回 None 表示未找到 dsh home
#[allow(clippy::collapsible_if)]
pub fn detect_dsh_home() -> Option<DshHome> {
    // 优先 $DSH_HOME 环境变量
    if let Ok(home) = std::env::var("DSH_HOME") {
        if !home.is_empty() {
            let path = PathBuf::from(home);
            if path.exists() {
                return Some(DshHome {
                    profiles_exist: path.join("profiles").exists(),
                    path,
                });
            }
        }
    }
    // 回退 ~/.dsh（USERPROFILE 优先于 HOME，与 mcp.rs user_home 同语义）
    let base = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from)?;
    let path = base.join(".dsh");
    if path.exists() {
        Some(DshHome {
            profiles_exist: path.join("profiles").exists(),
            path,
        })
    } else {
        None
    }
}

/// 扫描所有 profile 目录，返回 profile 名称列表
///
/// 扫描 `$DSH_HOME/profiles/` 下的子目录，每个子目录视为一个 profile。
/// 返回 (profile_name, profile_dir) 对。
pub fn list_profiles(dsh_home: &Path) -> Vec<(String, PathBuf)> {
    let profiles_dir = dsh_home.join("profiles");
    let mut profiles = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&profiles_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // profile 目录必须含 package.json（dsh profile 标识）
            if path.is_dir()
                && path.join("package.json").exists()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                profiles.push((name.to_string(), path));
            }
        }
    }
    profiles
}

/// 检查 profile 的 cordis.patch.yml 是否已包含 code-repo-wiki 条目
///
/// 两种形态都算已安装：
/// 1. 管理块（含 DSH_BLOCK_START/DSH_BLOCK_END 标记）
/// 2. 裸行（用户手工合入的 `- id: code-repo-wiki`）
pub fn profile_has_wiki(profile_dir: &Path) -> bool {
    let patch_path = profile_dir.join("cordis.patch.yml");
    if !patch_path.exists() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(&patch_path) else {
        return false;
    };
    // 检查管理块标记
    if content.contains(DSH_BLOCK_START) && content.contains(DSH_BLOCK_END) {
        return true;
    }
    // 检查裸行
    content
        .lines()
        .any(|l| l.trim_start() == "- id: code-repo-wiki")
}

/// 将 code-repo-wiki MCP 注册合入单个 profile 的 cordis.patch.yml
///
/// 使用 DshMcp 的 install 方法写入（复用管理块渲染和幂等逻辑）。
/// 返回是否实际变更。
pub fn merge_into_profile(profile_dir: &Path, exe: &str) -> Result<bool> {
    let patch_path = profile_dir.join("cordis.patch.yml");
    let dsh = DshMcp { path: patch_path };
    dsh.install(exe).with_context(|| {
        format!(
            "合入 profile {} 的 cordis.patch.yml 失败",
            profile_dir.display()
        )
    })
}

/// 从单个 profile 的 cordis.patch.yml 移除 code-repo-wiki 条目
///
/// 返回是否实际移除。
pub fn remove_from_profile(profile_dir: &Path) -> Result<bool> {
    let patch_path = profile_dir.join("cordis.patch.yml");
    let dsh = DshMcp { path: patch_path };
    dsh.remove().with_context(|| {
        format!(
            "从 profile {} 移除 code-repo-wiki 失败",
            profile_dir.display()
        )
    })
}

/// 扫描并合入所有 profile（install 流程调用）
///
/// 返回 (合入数量, 已存在数量, 总 profile 数)
pub fn merge_all_profiles(exe: &str) -> Result<(usize, usize, usize)> {
    let dsh_home = match detect_dsh_home() {
        Some(h) if h.profiles_exist => h,
        _ => return Ok((0, 0, 0)),
    };
    let profiles = list_profiles(&dsh_home.path);
    let total = profiles.len();
    let mut merged = 0;
    let mut already = 0;
    for (name, dir) in &profiles {
        if profile_has_wiki(dir) {
            tracing::info!("dsh profile '{}' 已包含 code-repo-wiki，跳过", name);
            already += 1;
        } else {
            match merge_into_profile(dir, exe) {
                Ok(true) => {
                    println!("✓ dsh profile '{}' 已合入 code-repo-wiki MCP", name);
                    merged += 1;
                }
                Ok(false) => {
                    tracing::info!("dsh profile '{}' 合入无变更", name);
                    already += 1;
                }
                Err(e) => {
                    tracing::warn!("dsh profile '{}' 合入失败: {}", name, e);
                }
            }
        }
    }
    Ok((merged, already, total))
}

/// 从所有 profile 移除 code-repo-wiki（uninstall 流程调用）
///
/// 返回移除数量
pub fn remove_all_profiles() -> Result<usize> {
    let dsh_home = match detect_dsh_home() {
        Some(h) if h.profiles_exist => h,
        _ => return Ok(0),
    };
    let profiles = list_profiles(&dsh_home.path);
    let mut removed = 0;
    for (name, dir) in &profiles {
        if profile_has_wiki(dir) {
            match remove_from_profile(dir) {
                Ok(true) => {
                    println!("✓ dsh profile '{}' 已移除 code-repo-wiki", name);
                    removed += 1;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("dsh profile '{}' 移除失败: {}", name, e);
                }
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 独立临时目录（防并行测试冲突）
    fn temp_dir(tag: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "code-repo-wiki-dsh-profile-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 检测不存在的 dsh home → 返回 None（当 DSH_HOME 和 ~/.dsh 都不存在时）
    #[test]
    fn test_detect_dsh_home_not_found() {
        // 保存原始值
        let old_dsh_home = std::env::var("DSH_HOME").ok();
        let old_home = std::env::var("HOME").ok();
        let old_userprofile = std::env::var("USERPROFILE").ok();

        // 设置不存在的路径
        unsafe {
            std::env::set_var("DSH_HOME", "/nonexistent/path/dsh-home-xyz");
            std::env::set_var("HOME", "/nonexistent/path/home-xyz");
            std::env::set_var("USERPROFILE", "/nonexistent/path/userprofile-xyz");
        }

        let result = detect_dsh_home();
        assert!(result.is_none(), "不存在的 DSH_HOME 应返回 None");

        // 恢复环境变量
        unsafe {
            match old_dsh_home {
                Some(v) => {
                    std::env::set_var("DSH_HOME", v);
                }
                None => {
                    std::env::remove_var("DSH_HOME");
                }
            }
            match old_home {
                Some(v) => {
                    std::env::set_var("HOME", v);
                }
                None => {
                    std::env::remove_var("HOME");
                }
            }
            match old_userprofile {
                Some(v) => {
                    std::env::set_var("USERPROFILE", v);
                }
                None => {
                    std::env::remove_var("USERPROFILE");
                }
            }
        }
    }

    /// 列出空 profiles 目录 → 返回空列表
    #[test]
    fn test_list_profiles_empty() {
        let dir = temp_dir("profiles-empty");
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        let profiles = list_profiles(&dir);
        assert!(profiles.is_empty(), "空 profiles 目录应返回空列表");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 列出 profiles → 只返回含 package.json 的子目录
    #[test]
    fn test_list_profiles_filters_by_package_json() {
        let dir = temp_dir("profiles-filter");
        let profiles_dir = dir.join("profiles");
        std::fs::create_dir_all(profiles_dir.join("web")).unwrap();
        std::fs::write(profiles_dir.join("web/package.json"), "{}").unwrap();
        std::fs::create_dir_all(profiles_dir.join("no-package")).unwrap();
        std::fs::create_dir_all(profiles_dir.join("headless")).unwrap();
        std::fs::write(profiles_dir.join("headless/package.json"), "{}").unwrap();

        let profiles = list_profiles(&dir);
        assert_eq!(profiles.len(), 2, "应只返回含 package.json 的目录");
        let names: Vec<&str> = profiles.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"web"));
        assert!(names.contains(&"headless"));
        assert!(!names.contains(&"no-package"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// profile 无 cordis.patch.yml → 未安装
    #[test]
    fn test_profile_has_wiki_no_patch_file() {
        let dir = temp_dir("has-wiki-no-patch");
        assert!(!profile_has_wiki(&dir), "无 patch 文件应返回 false");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// profile 含管理块 → 已安装
    #[test]
    fn test_profile_has_wiki_with_block() {
        let dir = temp_dir("has-wiki-block");
        std::fs::write(
            dir.join("cordis.patch.yml"),
            format!(
                "{}\n- insert:\n    - id: code-repo-wiki\n{}\n",
                DSH_BLOCK_START, DSH_BLOCK_END
            ),
        )
        .unwrap();
        assert!(profile_has_wiki(&dir), "含管理块应返回 true");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// profile 含裸行 → 已安装
    #[test]
    fn test_profile_has_wiki_with_bare_row() {
        let dir = temp_dir("has-wiki-bare");
        std::fs::write(
            dir.join("cordis.patch.yml"),
            "- insert:\n    - id: code-repo-wiki\n      name: test\n",
        )
        .unwrap();
        assert!(profile_has_wiki(&dir), "含裸行应返回 true");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无 dsh home → merge_all_profiles 返回 (0,0,0)
    #[test]
    fn test_merge_all_profiles_no_dsh_home() {
        // 保存原始值
        let old_dsh_home = std::env::var("DSH_HOME").ok();
        let old_home = std::env::var("HOME").ok();
        let old_userprofile = std::env::var("USERPROFILE").ok();

        // 设置不存在的路径
        unsafe {
            std::env::set_var("DSH_HOME", "/nonexistent/path/dsh-home-xyz");
            std::env::set_var("HOME", "/nonexistent/path/home-xyz");
            std::env::set_var("USERPROFILE", "/nonexistent/path/userprofile-xyz");
        }

        let (merged, already, total) = merge_all_profiles("/usr/bin/test").unwrap();
        assert_eq!(merged, 0);
        assert_eq!(already, 0);
        assert_eq!(total, 0);

        // 恢复环境变量
        unsafe {
            match old_dsh_home {
                Some(v) => {
                    std::env::set_var("DSH_HOME", v);
                }
                None => {
                    std::env::remove_var("DSH_HOME");
                }
            }
            match old_home {
                Some(v) => {
                    std::env::set_var("HOME", v);
                }
                None => {
                    std::env::remove_var("HOME");
                }
            }
            match old_userprofile {
                Some(v) => {
                    std::env::set_var("USERPROFILE", v);
                }
                None => {
                    std::env::remove_var("USERPROFILE");
                }
            }
        }
    }
}
