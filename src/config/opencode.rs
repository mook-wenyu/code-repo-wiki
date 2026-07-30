//! OpenCode 配置读写模块
//!
//! 管理 repo-wiki 插件在 opencode.json 中的注册状态。
//! 搜索顺序：项目根 .opencode.json → ~/.config/opencode/opencode.json
//!
//! 使用 serde_json::Value 操作 JSON，不依赖 OpenCode 的 schema 类型，
//! 避免与 OpenCode 版本耦合。

use std::path::PathBuf;

use anyhow::{Context, Result};

/// OpenCode 配置管理器
pub struct OpenCodeConfig {
    /// 全局 opencode.json 路径
    pub config_path: PathBuf,
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
            });
        }

        // 回退到全局配置
        let global_config = Self::config_dir().join("opencode.json");
        Ok(Self {
            config_path: global_config,
        })
    }

    /// 安装 repo-wiki 插件到 opencode.json
    ///
    /// 如果 plugins 字段不存在则创建，如果已有 name == "repo-wiki" 的条目则跳过。
    pub fn install_plugin(&mut self) -> Result<()> {
        let content = std::fs::read_to_string(&self.config_path)
            .unwrap_or_else(|_| "{}".to_string());
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("解析配置文件失败: {}", self.config_path.display()))?;

        let plugins = value
            .get_mut("plugins")
            .and_then(|p| p.as_array_mut());

        match plugins {
            Some(arr) => {
                // 检查是否已安装
                if arr.iter().any(|p| p.get("name") == Some(&serde_json::Value::String("repo-wiki".to_string()))) {
                    tracing::info!("repo-wiki 插件已安装，跳过");
                    return Ok(());
                }
                arr.push(plugin_entry());
            }
            None => {
                // 创建 plugins 数组
                value["plugins"] = serde_json::json!([plugin_entry()]);
            }
        }

        let output = serde_json::to_string_pretty(&value)
            .with_context(|| "序列化 opencode.json 失败")?;
        std::fs::write(&self.config_path, &output)
            .with_context(|| format!("写入配置文件失败: {}", self.config_path.display()))?;

        tracing::info!("repo-wiki 插件已安装到: {}", self.config_path.display());
        Ok(())
    }

    /// 从 opencode.json 卸载 repo-wiki 插件
    pub fn uninstall_plugin(&mut self) -> Result<()> {
        if !self.config_path.exists() {
            return Ok(());
        }

        let content = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("读取配置文件失败: {}", self.config_path.display()))?;
        let mut value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("解析配置文件失败: {}", self.config_path.display()))?;

        if let Some(plugins) = value.get_mut("plugins").and_then(|p| p.as_array_mut()) {
            plugins.retain(|p| p.get("name") != Some(&serde_json::Value::String("repo-wiki".to_string())));
        }

        let output = serde_json::to_string_pretty(&value)
            .with_context(|| "序列化 opencode.json 失败")?;
        std::fs::write(&self.config_path, &output)
            .with_context(|| format!("写入配置文件失败: {}", self.config_path.display()))?;

        tracing::info!("repo-wiki 插件已从 {} 卸载", self.config_path.display());
        Ok(())
    }

    /// 检查插件是否已安装
    pub fn is_installed(&self) -> Result<bool> {
        if !self.config_path.exists() {
            return Ok(false);
        }

        let content = std::fs::read_to_string(&self.config_path)
            .with_context(|| format!("读取配置文件失败: {}", self.config_path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("解析配置文件失败: {}", self.config_path.display()))?;

        let installed = value
            .get("plugins")
            .and_then(|p| p.as_array())
            .map(|arr| {
                arr.iter()
                    .any(|p| p.get("name") == Some(&serde_json::Value::String("repo-wiki".to_string())))
            })
            .unwrap_or(false);

        Ok(installed)
    }

    /// 获取 OpenCode 配置的根目录 (~/.config/opencode/)
    pub fn config_dir() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config").join("opencode")
    }
}

/// 创建 repo-wiki 插件条目 JSON
fn plugin_entry() -> serde_json::Value {
    serde_json::json!({
        "name": "repo-wiki",
        "path": ".opencode/plugins/repo-wiki.ts",
        "enabled": true
    })
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn test_install_plugin_adds_entry() {
        let (dir, path) = setup_temp_config(Some(r#"{}"#));
        let mut config = OpenCodeConfig { config_path: path.clone() };

        config.install_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        let plugins = value["plugins"].as_array().expect("plugins 应该是一个数组");
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0]["name"], "repo-wiki");
        assert_eq!(plugins[0]["enabled"], true);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_install_plugin_idempotent() {
        let (dir, path) = setup_temp_config(Some(r#"{"plugins":[]}"#));
        let mut config = OpenCodeConfig { config_path: path.clone() };

        config.install_plugin().unwrap();
        config.install_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        let plugins = value["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1, "重复安装不应增加条目");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_uninstall_plugin_removes_entry() {
        let initial = r#"{"plugins":[{"name":"repo-wiki","path":".opencode/plugins/repo-wiki.ts","enabled":true}]}"#;
        let (dir, path) = setup_temp_config(Some(initial));
        let mut config = OpenCodeConfig { config_path: path.clone() };

        config.uninstall_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        let plugins = value["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_uninstall_plugin_preserves_other_plugins() {
        let initial = r#"{"plugins":[{"name":"other","enabled":true},{"name":"repo-wiki","enabled":true}]}"#;
        let (dir, path) = setup_temp_config(Some(initial));
        let mut config = OpenCodeConfig { config_path: path.clone() };

        config.uninstall_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        let plugins = value["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0]["name"], "other");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_installed_when_present() {
        let initial = r#"{"plugins":[{"name":"repo-wiki","enabled":true}]}"#;
        let (dir, path) = setup_temp_config(Some(initial));
        let config = OpenCodeConfig { config_path: path };

        assert!(config.is_installed().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_installed_when_absent() {
        let initial = r#"{"plugins":[{"name":"other","enabled":true}]}"#;
        let (dir, path) = setup_temp_config(Some(initial));
        let config = OpenCodeConfig { config_path: path };

        assert!(!config.is_installed().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_installed_when_file_missing() {
        let (dir, path) = setup_temp_config(None);
        let config = OpenCodeConfig { config_path: path };

        assert!(!config.is_installed().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_uninstall_plugin_noop_when_file_missing() {
        let (dir, path) = setup_temp_config(None);
        let mut config = OpenCodeConfig { config_path: path };

        config.uninstall_plugin().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
