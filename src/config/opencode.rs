//! OpenCode 配置读写模块
//!
//! 管理 repo-wiki 插件在 opencode.json 中的注册状态。
//! 搜索顺序：项目根 .opencode.json → ~/.config/opencode/opencode.json
//!
//! 使用 serde_json::Value 操作 JSON，不依赖 OpenCode 的 schema 类型，
//! 避免与 OpenCode 版本耦合。
//!
//! ## 插件加载机制（opencode 1.18.10，实测验证）
//!
//! - `.opencode/plugins/*.ts` 目录**自动扫描加载**，无需任何 config 条目
//!   （官方加载器 glob `{plugin,plugins}/*.{ts,js}`）
//! - 官方配置仅认单数 `plugin` 字段（字符串数组）；**不存在 `plugins` 复数键**，
//!   多余顶层键会触发配置解析 `Unrecognized key` 错误
//! - 因此本模块不再向配置写入插件条目；install/uninstall 仅负责
//!   **幂等清理历史遗留的无效 `plugins` 键**（旧版本曾错误写入），
//!   is_installed 以插件文件存在性为准

use std::path::PathBuf;

use anyhow::{Context, Result};

/// OpenCode 配置管理器
pub struct OpenCodeConfig {
    /// 全局 opencode.json 路径
    pub config_path: PathBuf,
    /// 项目根目录（插件文件相对此解析；测试中可注入任意临时目录）
    pub project_root: PathBuf,
}

impl OpenCodeConfig {
    /// 创建管理器，自动查找 opencode.json
    ///
    /// 搜索顺序：
    /// 1. 项目根目录 `.opencode.json`
    /// 2. `~/.config/opencode/opencode.json`
    pub fn new() -> Result<Self> {
        // 尝试当前目录下的 .opencode.json
        let cwd = std::env::current_dir().context("获取当前工作目录失败")?;
        let project_config = cwd.join(".opencode.json");
        if project_config.exists() {
            return Ok(Self {
                config_path: project_config,
                project_root: cwd,
            });
        }

        // 回退到全局配置
        let global_config = Self::config_dir().join("opencode.json");
        Ok(Self {
            config_path: global_config,
            project_root: cwd,
        })
    }

    /// 安装 repo-wiki 插件
    ///
    /// 插件目录自动加载，无需配置条目；本方法仅**幂等清理**配置中
    /// 历史遗留的无效 `plugins` 键（opencode 1.18.10 解析会报
    /// `Unrecognized key` 错误）。配置不存在时静默创建空对象。
    pub fn install_plugin(&mut self) -> Result<()> {
        let content = std::fs::read_to_string(&self.config_path)
            .unwrap_or_else(|_| "{}".to_string());
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("解析配置文件失败: {}", self.config_path.display()))?;

        // 移除无效的 plugins 键（无论是否数组，都不是官方字段）
        if value.get_mut("plugins").is_some() {
            tracing::info!(
                "清理 opencode.json 中无效的 plugins 键（官方仅认单数 plugin）: {}",
                self.config_path.display()
            );
            value.as_object_mut().unwrap().remove("plugins");
        }

        let output = serde_json::to_string_pretty(&value)
            .with_context(|| "序列化 opencode.json 失败")?;
        std::fs::write(&self.config_path, &output)
            .with_context(|| format!("写入配置文件失败: {}", self.config_path.display()))?;

        tracing::info!("repo-wiki 插件已就绪（目录自动加载，无需配置条目）");
        Ok(())
    }

    /// 从 opencode.json 卸载 repo-wiki 插件（清理无效 plugins 键）
    ///
    /// opencode 对插件是目录自动加载，卸载插件的实际动作是删除
    /// `.opencode/plugins/repo-wiki.ts` 文件（由用户决定，不在此处执行）；
    /// 本方法仅保证配置不含历史遗留的无效键。
    pub fn uninstall_plugin(&mut self) -> Result<()> {
        if !self.config_path.exists() {
            return Ok(());
        }

        let content = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("读取配置文件失败: {}", self.config_path.display()))?;
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("解析配置文件失败: {}", self.config_path.display()))?;

        if value.get_mut("plugins").is_some() {
            value.as_object_mut().unwrap().remove("plugins");
            let output = serde_json::to_string_pretty(&value)
                .with_context(|| "序列化 opencode.json 失败")?;
            std::fs::write(&self.config_path, &output)
                .with_context(|| format!("写入配置文件失败: {}", self.config_path.display()))?;
        }

        tracing::info!("repo-wiki 插件配置已清理: {}", self.config_path.display());
        Ok(())
    }

    /// 检查插件是否已安装（插件文件 `.opencode/plugins/repo-wiki.ts` 是否存在）
    ///
    /// 以文件存在性为准：opencode 目录自动加载，配置文件不再承载注册信息。
    pub fn is_installed(&self) -> Result<bool> {
        let plugin_file = self
            .project_root
            .join(".opencode")
            .join("plugins")
            .join("repo-wiki.ts");
        Ok(plugin_file.exists())
    }

    /// 获取 OpenCode 配置的根目录 (~/.config/opencode/)
    pub fn config_dir() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config").join("opencode")
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 在临时目录中创建模拟的 opencode.json（每个测试独立目录，防并行冲突）
    fn setup_temp_config(initial: Option<&str>) -> (PathBuf, PathBuf) {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("repo-wiki-opencode-test-{}-{}", std::process::id(), id));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");
        let path = dir.join("opencode.json");

        if let Some(content) = initial {
            std::fs::write(&path, content).expect("写入临时配置文件失败");
        }

        (dir, path)
    }

    /// 在临时目录创建插件文件（.opencode/plugins/repo-wiki.ts），返回 (目录, 文件路径)
    fn setup_plugin_file(dir: &Path) -> PathBuf {
        let plugin_dir = dir.join(".opencode").join("plugins");
        std::fs::create_dir_all(&plugin_dir).expect("创建插件目录失败");
        let path = plugin_dir.join("repo-wiki.ts");
        std::fs::write(&path, "export const RepoWikiPlugin = () => ({});").expect("写入插件文件失败");
        path
    }

    /// install 应幂等清理历史遗留的无效 plugins 键（旧版本错误写入的复数对象数组）
    #[test]
    fn test_install_plugin_removes_invalid_plugins_key() {
        let initial = r#"{"plugins":[{"name":"repo-wiki","path":".opencode/plugins/repo-wiki.ts","enabled":true}]}"#;
        let (dir, path) = setup_temp_config(Some(initial));
        let mut config = OpenCodeConfig { config_path: path.clone(), project_root: dir.clone() };

        config.install_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value.get("plugins").is_none(), "install 后不应残留无效的 plugins 键");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// install 对干净配置幂等（不写入任何条目）
    #[test]
    fn test_install_plugin_noop_when_clean() {
        let (dir, path) = setup_temp_config(Some(r#"{}"#));
        let mut config = OpenCodeConfig { config_path: path.clone(), project_root: dir.clone() };

        config.install_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value.get("plugins").is_none());
        assert_eq!(value.as_object().unwrap().len(), 0, "干净配置不应被写入内容");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 配置缺失时 install 创建空对象且无无效键
    #[test]
    fn test_install_plugin_creates_config_when_missing() {
        let (dir, path) = setup_temp_config(None);
        let mut config = OpenCodeConfig { config_path: path.clone(), project_root: dir.clone() };

        config.install_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value.get("plugins").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// uninstall 清理无效 plugins 键且保留其他合法键
    #[test]
    fn test_uninstall_plugin_removes_invalid_key_preserves_others() {
        let initial = r#"{"plugins":[{"name":"repo-wiki","enabled":true}],"theme":"dark"}"#;
        let (dir, path) = setup_temp_config(Some(initial));
        let mut config = OpenCodeConfig { config_path: path.clone(), project_root: dir.clone() };

        config.uninstall_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value.get("plugins").is_none(), "卸载后不应残留 plugins 键");
        assert_eq!(value["theme"], "dark", "其他合法键应保留");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 配置文件缺失时 uninstall 静默成功（幂等）
    #[test]
    fn test_uninstall_plugin_noop_when_file_missing() {
        let (dir, path) = setup_temp_config(None);
        let mut config = OpenCodeConfig { config_path: path, project_root: dir.clone() };

        config.uninstall_plugin().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// is_installed 以插件文件存在性为准：文件存在 → true
    #[test]
    fn test_is_installed_when_plugin_file_present() {
        let (dir, _) = setup_temp_config(None);
        setup_plugin_file(&dir);
        let config = OpenCodeConfig {
            config_path: dir.join("opencode.json"),
            project_root: dir.clone(),
        };
        assert!(config.is_installed().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// is_installed 在无插件文件的项目返回 false
    #[test]
    fn test_is_installed_when_plugin_file_missing() {
        let (dir, _) = setup_temp_config(None);
        // 临时目录没有 .opencode/plugins/repo-wiki.ts
        let config = OpenCodeConfig {
            config_path: dir.join("opencode.json"),
            project_root: dir.clone(),
        };
        assert!(!config.is_installed().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
