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
    /// 1. 项目根 `root` 下 `.opencode.json`
    /// 2. `~/.config/opencode/opencode.json`
    ///
    /// v33 起接收注入的 `root`（--root 语义修复）：插件文件/配置查找
    /// 全部相对项目根解析，不再依赖进程 cwd（v32 审查 HIGH：跨 cwd 运行
    /// 会把插件装进 cwd 仓库，只有 hooks/.mcp.json 落对目标仓库）。
    pub fn new(root: &crate::project::ProjectRoot) -> Result<Self> {
        let project_root = root.path().to_path_buf();
        let project_config = project_root.join(".opencode.json");
        if project_config.exists() {
            return Ok(Self {
                config_path: project_config,
                project_root,
            });
        }

        // 回退到全局配置
        let global_config = Self::config_dir()?.join("opencode.json");
        Ok(Self {
            config_path: global_config,
            project_root,
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
        // N12：顶层必须是 JSON 对象——数组/标量/字符串配置本身就是
        // 损坏（opencode 顶层只有对象合法），此前仅在 plugins 键存在时
        // 检查，数组 JSON 无键会静默通过并写回原样（错误配置被保留）
        if !value.is_object() {
            anyhow::bail!(
                "opencode.json 顶层应为 JSON 对象: {}",
                self.config_path.display()
            );
        }

        // 移除无效的 plugins 键（无论是否数组，都不是官方字段）
        if value.get_mut("plugins").is_some() {
            tracing::info!(
                "清理 opencode.json 中无效的 plugins 键（官方仅认单数 plugin）: {}",
                self.config_path.display()
            );
            // 顶层必须是对象（数组/标量 JSON 无键可清，属配置错误，显式报错而非兜底）
            value
                .as_object_mut()
                .with_context(|| format!("opencode.json 顶层应为 JSON 对象: {}", self.config_path.display()))?
                .remove("plugins");
        }

        let output = serde_json::to_string_pretty(&value)
            .with_context(|| "序列化 opencode.json 失败")?;
        // 父目录（如 ~/.config/opencode/）可能不存在（全新环境），写入前创建
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }
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
        // N12：顶层非对象（数组/标量）直接报错——与 install_plugin 同规则
        if !value.is_object() {
            anyhow::bail!(
                "opencode.json 顶层应为 JSON 对象: {}",
                self.config_path.display()
            );
        }

        if value.get_mut("plugins").is_some() {
            value
                .as_object_mut()
                .with_context(|| format!("opencode.json 顶层应为 JSON 对象: {}", self.config_path.display()))?
                .remove("plugins");
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
    /// N10：官方加载器 glob 为 `{plugin,plugins}/*.{ts,js}`——单复数目录
    /// 都要查（此前只查 plugins/，用户手工放在 plugin/ 时误报未安装）。
    pub fn is_installed(&self) -> Result<bool> {
        for dir in ["plugins", "plugin"] {
            let plugin_file = self
                .project_root
                .join(".opencode")
                .join(dir)
                .join("repo-wiki.ts");
            if plugin_file.exists() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 将插件模板写入 `{project_root}/.opencode/plugins/repo-wiki.ts`
    ///
    /// v33 升级语义（用户拍板「带标记则升级」）：插件文件是 repo-wiki
    /// 专属产物（文件名即标记），内容与最新模板（注入当前 exe 绝对路径）
    /// 不同即覆盖升级（旧版本模板/二进制路径变化）；相同则跳过。
    /// 返回是否实际写入。模板经 include_str 内嵌编译（见下方实现注释：
    /// v33 修复自举缺陷——模板源不再依赖仓库内安装产物文件）。
    pub fn install_plugin_file(&mut self) -> Result<bool> {
        let plugin_path = self
            .project_root
            .join(".opencode")
            .join("plugins")
            .join("repo-wiki.ts");
        // t02（v16）：PATH 硬依赖根治——把模板中 execa 的二进制名替换为
        // 当前进程的绝对路径。插件经 execa("repo-wiki", ...) 调 CLI，二进制
        // 不在 PATH 时（cargo install 目标目录未入 PATH、便携部署等）16 个
        // 工具全部 ENOENT 失效。install 时注入 current_exe() 绝对路径，
        // 插件不再依赖 PATH。只替换 execa 首参（模板中该字面量唯一）；
        // 路径经 JSON 字符串转义（Windows 反斜杠/引号安全）。
        let exe_path = std::env::current_exe()
            .with_context(|| "无法定位当前可执行文件路径（插件无法绑定绝对路径）")?;
        let exe_json =
            serde_json::to_string(&exe_path.to_string_lossy().to_string())
                .with_context(|| "序列化可执行文件路径失败")?;
        let template = {
            // 模板内嵌编译（include_str）：插件模板只含 execa("repo-wiki")
            // 占位（下方注入 current_exe 绝对路径），不含任何编译期路径，
            // 因此发布安装/仓库移动/uninstall 删除安装产物后仍可生成。
            // （v33 修复：旧实现运行时读仓库内 .opencode/plugins/repo-wiki.ts
            // 作为模板源——uninstall 删除该安装产物后模板源同时丢失，
            // 再次 install 直接失败；模板与安装目标同路径是自举缺陷）
            let raw = include_str!("../../.opencode/plugins/repo-wiki.ts");
            raw.replace(
                "execa(\"repo-wiki\"",
                &format!("execa({exe_json}"),
            )
        };

        // v33：内容比对决定升级或跳过（幂等跳过 = 内容完全一致）
        if let Ok(existing) = std::fs::read_to_string(&plugin_path) {
            if existing == template {
                tracing::info!("插件文件已是最新，跳过: {}", plugin_path.display());
                return Ok(false);
            }
            tracing::info!("插件文件内容与模板不一致，升级覆盖: {}", plugin_path.display());
        }
        std::fs::create_dir_all(plugin_path.parent().unwrap())
            .with_context(|| format!("创建插件目录失败: {}", plugin_path.display()))?;
        std::fs::write(&plugin_path, template)
            .with_context(|| format!("写入插件文件失败: {}", plugin_path.display()))?;
        tracing::info!("插件文件已写入: {}", plugin_path.display());
        Ok(true)
    }

    /// 删除插件文件 `.opencode/plugins/repo-wiki.ts`
    ///
    /// 文件不存在时静默成功（幂等，与 uninstall_plugin 语义一致）。
    pub fn uninstall_plugin_file(&mut self) -> Result<()> {
        let plugin_path = self
            .project_root
            .join(".opencode")
            .join("plugins")
            .join("repo-wiki.ts");
        match std::fs::remove_file(&plugin_path) {
            Ok(()) => {
                tracing::info!("插件文件已删除: {}", plugin_path.display());
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// 获取 OpenCode 配置的根目录 (~/.config/opencode/)
    ///
    /// v33：USERPROFILE 与 HOME 都缺失时显式报错（与
    /// [`crate::config::global_config_dir`] 的「写错位置比报错更隐蔽」
    /// 语义统一），不再回退 `.` 静默写当前目录。
    pub fn config_dir() -> Result<PathBuf> {
        // N11：Windows 语义——USERPROFILE 优先于 HOME（
        // 部分 Windows 环境两者都存在时 HOME 可能是 Cygwin/残留值）
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map(PathBuf::from)
            .map_err(|_| anyhow::anyhow!("无法确定用户级配置目录（USERPROFILE 与 HOME 均未设置）"))?;
        Ok(home.join(".config").join("opencode"))
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

    /// N10：插件文件在单数 plugin/ 目录时同样判定已安装（官方加载器 glob {plugin,plugins}）
    #[test]
    fn test_is_installed_singular_plugin_dir() {
        let (dir, _) = setup_temp_config(None);
        let plugin_dir = dir.join(".opencode").join("plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("repo-wiki.ts"), "export const RepoWikiPlugin = () => ({});").unwrap();
        let config = OpenCodeConfig {
            config_path: dir.join("opencode.json"),
            project_root: dir.clone(),
        };
        assert!(config.is_installed().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// N11：config_dir 优先 USERPROFILE（Windows 语义）
    #[test]
    fn test_config_dir_prefers_userprofile() {
        // Rust 2024：env::set_var/remove_var 为 unsafe（多线程环境写环境变量
        // 与 getenv 竞态），测试内短暂修改后立即恢复
        unsafe {
            std::env::set_var("USERPROFILE", "C:\\Users\\testuser");
            std::env::remove_var("HOME");
        }
        let dir = OpenCodeConfig::config_dir().unwrap();
        assert_eq!(
            dir,
            PathBuf::from("C:\\Users\\testuser").join(".config").join("opencode"),
            "USERPROFILE 应优先于 HOME"
        );
        // 恢复环境变量（并行测试隔离：其他测试可能依赖 HOME）
        unsafe {
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("HOME");
        }
    }

    /// v33：config_dir 双缺失（USERPROFILE 与 HOME 均未设置）→ 显式报错
    /// （与 config::global_config_dir 语义统一，不再回退 "." 写当前目录）
    #[test]
    fn test_config_dir_errors_without_home() {
        unsafe {
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("HOME");
        }
        assert!(OpenCodeConfig::config_dir().is_err());
    }

    /// N12：顶层非对象 JSON（数组/标量）→ install/uninstall 显式报错
    #[test]
    fn test_non_object_config_errors() {
        for (tag, initial) in [("arr", "[1,2,3]"), ("str", "\"oops\"")] {
            let (dir, path) = setup_temp_config(Some(initial));
            let mut config = OpenCodeConfig { config_path: path.clone(), project_root: dir.clone() };
            assert!(config.install_plugin().is_err(), "install 对非对象配置应报错 ({tag})");
            assert!(config.uninstall_plugin().is_err(), "uninstall 对非对象配置应报错 ({tag})");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// t02（v16）：install_plugin_file 注入当前可执行文件绝对路径——
    /// 插件不再依赖 PATH（exec 目标为注入路径而非 "repo-wiki" 字面量）
    #[test]
    fn test_install_plugin_file_injects_absolute_exe_path() {
        let (dir, _) = setup_temp_config(None);
        let mut config = OpenCodeConfig { config_path: dir.join("opencode.json"), project_root: dir.clone() };

        let wrote = config.install_plugin_file().unwrap();
        assert!(wrote, "首次安装应实际写入插件文件");

        let plugin_path = dir.join(".opencode").join("plugins").join("repo-wiki.ts");
        let content = std::fs::read_to_string(&plugin_path).unwrap();

        // 注入的路径 = 测试进程可执行文件（current_exe 语义），JSON 转义后嵌入
        let exe_path = std::env::current_exe().unwrap();
        let exe_json = serde_json::to_string(&exe_path.to_string_lossy().to_string()).unwrap();
        assert!(
            content.contains(&format!("execa({exe_json}")),
            "插件应绑定注入的绝对路径（JSON 转义）, 实际: {}",
            // char 安全切片：字节索引可能落在多字节 UTF-8 中间（panic）
            content.chars().take(400).collect::<String>()
        );
        assert!(
            !content.contains("execa(\"repo-wiki\""),
            "PATH 字面量版本不应残留"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
