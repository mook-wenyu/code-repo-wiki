//! OpenCode 配置读写模块
//!
//! 管理 code-repo-wiki 插件在 opencode.json 中的注册状态。
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
    /// 全局 opencode.json 路径（`~/.config/opencode/opencode.json`）
    pub config_path: PathBuf,
    /// 项目根目录（仅用于清理 v39 之前的旧版项目级插件产物）
    pub project_root: PathBuf,
}

/// 向插件模板注入当前可执行文件的绝对路径（占位符替换）。
///
/// 模板中 execa 首参占位字面量为 `execa("code-repo-wiki"`；替换为
/// `execa(<exe_json>`（exe_json 为 JSON 字符串字面量，Windows 反斜杠/引号安全）。
/// 占位符未命中（模板改版/字面量变化）时显式报错——绝不静默输出
/// PATH 依赖的插件（v16 已修复的缺陷不得以「替换未命中」形式回归）。
fn inject_exe_path(template: &str, exe_json: &str) -> Result<String> {
    let placeholder = "execa(\"code-repo-wiki\"";
    let replaced = template.replace(placeholder, &format!("execa({exe_json}"));
    if replaced == template {
        anyhow::bail!(
            "插件模板占位符缺失（未找到 {placeholder:?}），请同步更新 plugin-template.ts 与替换逻辑"
        );
    }
    Ok(replaced)
}

impl OpenCodeConfig {
    /// 创建管理器，配置路径固定为用户级全局 opencode.json
    ///
    /// v39 起（官方文档+源码查证）：opencode 用户级配置根为
    /// `~/.config/opencode`（全平台一致，含 Windows——xdg-basedir 无平台
    /// 分支）；插件自动加载目录为配置根下 `plugins/`。因此插件文件与
    /// 配置清理全部落在用户级（一次 install 全仓库 opencode 会话可用），
    /// 不再读写项目根 `.opencode.json`——那是用户自建文件，不属于本工具。
    pub fn new(root: &crate::project::ProjectRoot) -> Result<Self> {
        let project_root = root.path().to_path_buf();
        let global_config = Self::config_dir()?.join("opencode.json");
        Ok(Self {
            config_path: global_config,
            project_root,
        })
    }

    /// 安装 code-repo-wiki 插件
    ///
    /// 插件目录自动加载，无需配置条目；本方法仅**幂等清理**配置中
    /// 历史遗留的无效 `plugins` 键（opencode 1.18.10 解析会报
    /// `Unrecognized key` 错误）。配置不存在时静默创建空对象。
    pub fn install_plugin(&mut self) -> Result<()> {
        let content = match std::fs::read_to_string(&self.config_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
            Err(e) => {
                // v52 T08a：读失败（权限/占用/损坏）时显式中止——静默当空对象
                // 会让后续写回**覆盖含 OAuth 会话的用户配置**（凭据丢失）。
                anyhow::bail!(
                    "读取 opencode.json 失败（已中止写回，避免覆盖现有配置）: {}: {e}",
                    self.config_path.display()
                );
            }
        };
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
            // 已在函数顶部确认顶层为 JSON 对象（非对象已 bail），此处重复的对象
            // 检查是死代码，直接以断言清键（行为与原先完全一致）。
            value
                .as_object_mut()
                .expect("函数顶部已确认顶层为 JSON 对象")
                .remove("plugins");
        }

        let output =
            serde_json::to_string_pretty(&value).with_context(|| "序列化 opencode.json 失败")?;
        // 父目录（如 ~/.config/opencode/）可能不存在（全新环境），写入前创建
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建配置目录失败: {}", parent.display()))?;
        }
        std::fs::write(&self.config_path, &output)
            .with_context(|| format!("写入配置文件失败: {}", self.config_path.display()))?;

        tracing::info!("code-repo-wiki 插件已就绪（目录自动加载，无需配置条目）");
        Ok(())
    }

    /// 从 opencode.json 卸载 code-repo-wiki 插件（清理无效 plugins 键）
    ///
    /// opencode 对插件是目录自动加载，卸载插件的实际动作是删除
    /// `.opencode/plugins/code-repo-wiki.ts` 文件（由用户决定，不在此处执行）；
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

        let had_plugins = value.get("plugins").is_some();
        value
            .as_object_mut()
            .with_context(|| {
                format!(
                    "opencode.json 顶层应为 JSON 对象: {}",
                    self.config_path.display()
                )
            })?
            .remove("plugins");
        // 清理后若配置文件已无任何键（空对象——含从未有过 plugins 键的
        // 历史空壳），直接删除文件：保留 `{}` 只会让用户疑惑，且下次
        // install 会重新创建。非空配置（无 plugins 键）原样保留不动。
        if value.as_object().is_some_and(|o| o.is_empty()) {
            std::fs::remove_file(&self.config_path)
                .with_context(|| format!("删除空配置文件失败: {}", self.config_path.display()))?;
        } else if had_plugins {
            let output = serde_json::to_string_pretty(&value)
                .with_context(|| "序列化 opencode.json 失败")?;
            std::fs::write(&self.config_path, &output)
                .with_context(|| format!("写入配置文件失败: {}", self.config_path.display()))?;
        }

        tracing::info!(
            "code-repo-wiki 插件配置已清理: {}",
            self.config_path.display()
        );
        Ok(())
    }

    /// 检查插件是否已安装（插件文件 `.opencode/plugins/code-repo-wiki.ts` 是否存在）
    ///
    /// 以文件存在性为准：opencode 目录自动加载，配置文件不再承载注册信息。
    /// N10：官方加载器 glob 为 `{plugin,plugins}/*.{ts,js}`——单复数目录
    /// 都要查（此前只查 plugins/，用户手工放在 plugin/ 时误报未安装）。
    ///
    /// v39：插件已移至用户级配置根（`~/.config/opencode/plugins/`），
    /// 安装状态以用户级文件存在性为准（项目级旧产物由迁移逻辑清理，
    /// 不再视为已安装）。
    pub fn is_installed(&self) -> Result<bool> {
        let config_root = self
            .config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("无法定位 OpenCode 配置根目录"))?;
        for dir in ["plugins", "plugin"] {
            let plugin_file = config_root.join(dir).join("code-repo-wiki.ts");
            if plugin_file.exists() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 清理 v39 之前的旧版项目级插件产物
    ///
    /// v33-v38 把插件写入 `{project_root}/.opencode/{plugins,plugin}/`；
    /// v39 起改用户级配置根。旧文件残留会被 opencode 项目级目录继续
    /// 自动加载（且内容含旧 exe 路径），install/uninstall 时幂等清理：
    /// 存在即删除并返回 true（供提示），不存在静默返回 false。
    fn remove_legacy_project_plugin(&self) -> Result<bool> {
        let mut removed = false;
        for dir in ["plugins", "plugin"] {
            let legacy = self
                .project_root
                .join(".opencode")
                .join(dir)
                .join("code-repo-wiki.ts");
            match std::fs::remove_file(&legacy) {
                Ok(()) => {
                    tracing::info!("已清理旧版项目级插件: {}", legacy.display());
                    removed = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(removed)
    }

    /// 将插件模板写入 `{config_root}/plugins/code-repo-wiki.ts`（用户级）
    ///
    /// v39：插件是「用户级内容」——装进 Agent 配置根目录
    /// `~/.config/opencode/plugins/`（官方文档：该目录下 `{plugin,plugins}/*.{ts,js}`
    /// 启动时自动加载），一次 install 全仓库 opencode 会话可用，不再写入
    /// 项目 `.opencode/plugins/`。写入前清理 v39 之前的旧版项目级产物。
    ///
    /// 升级语义（用户拍板「带标记则升级」）：插件文件是 code-repo-wiki
    /// 专属产物（文件名即标记），内容与最新模板（注入当前 exe 绝对路径）
    /// 不同即覆盖升级（旧版本模板/二进制路径变化）；相同则跳过。
    /// 返回是否实际写入。模板经 include_str 内嵌编译（见下方实现注释：
    /// v33 修复自举缺陷——模板源不再依赖仓库内安装产物文件）。
    pub fn install_plugin_file(&mut self) -> Result<bool> {
        if self.remove_legacy_project_plugin()? {
            println!("  ✓ 已清理旧版项目级插件（v39 起插件改用户级安装）");
        }
        let plugin_path = self
            .config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("无法定位 OpenCode 配置根目录"))?
            .join("plugins")
            .join("code-repo-wiki.ts");
        // t02（v16）：PATH 硬依赖根治——把模板中 execa 的二进制名替换为
        // 当前进程的绝对路径。插件经 execa("code-repo-wiki", ...) 调 CLI，二进制
        // 不在 PATH 时（cargo install 目标目录未入 PATH、便携部署等）15 个
        // 工具全部 ENOENT 失效。install 时注入 current_exe() 绝对路径，
        // 插件不再依赖 PATH。只替换 execa 首参（模板中该字面量唯一）；
        // 路径经 JSON 字符串转义（Windows 反斜杠/引号安全）。
        let exe_path = std::env::current_exe()
            .with_context(|| "无法定位当前可执行文件路径（插件无法绑定绝对路径）")?;
        let exe_json = serde_json::to_string(&exe_path.to_string_lossy().to_string())
            .with_context(|| "序列化可执行文件路径失败")?;
        let template = {
            // 模板内嵌编译（include_str）：插件模板只含 execa("code-repo-wiki")
            // 占位（下方注入 current_exe 绝对路径），不含任何编译期路径，
            // 因此发布安装/仓库移动后仍可生成。模板源固定为源码目录内
            // src/config/plugin-template.ts——与安装产物（用户级
            // ~/.config/opencode/plugins/）完全分离，uninstall 删除产物
            // 不影响编译与再次 install
            // （v38 修复：v33 注释声称已修复自举缺陷但 include_str 仍指向
            // 仓库内安装产物路径——真实环境 uninstall 删除产物后编译失败）
            let raw = include_str!("plugin-template.ts");
            inject_exe_path(raw, &exe_json)?
        };

        // v33：内容比对决定升级或跳过（幂等跳过 = 内容完全一致）
        if let Ok(existing) = std::fs::read_to_string(&plugin_path) {
            if existing == template {
                tracing::info!("插件文件已是最新，跳过: {}", plugin_path.display());
                return Ok(false);
            }
            tracing::info!(
                "插件文件内容与模板不一致，升级覆盖: {}",
                plugin_path.display()
            );
        }
        std::fs::create_dir_all(plugin_path.parent().unwrap())
            .with_context(|| format!("创建插件目录失败: {}", plugin_path.display()))?;
        std::fs::write(&plugin_path, template)
            .with_context(|| format!("写入插件文件失败: {}", plugin_path.display()))?;
        tracing::info!("插件文件已写入: {}", plugin_path.display());
        Ok(true)
    }

    /// 删除用户级插件文件（`plugins/` 与 `plugin/` 双目录，与官方自动加载
    /// glob `{plugin,plugins}/*.{ts,js}` 及 [`Self::is_installed`] 对称），
    /// 并清理 v39 之前的旧版项目级产物。
    ///
    /// 文件不存在时静默成功（幂等，与 uninstall_plugin 语义一致）。
    pub fn uninstall_plugin_file(&mut self) -> Result<()> {
        self.remove_legacy_project_plugin()?;
        let config_root = self
            .config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("无法定位 OpenCode 配置根目录"))?;
        for dir in ["plugins", "plugin"] {
            let plugin_path = config_root.join(dir).join("code-repo-wiki.ts");
            match std::fs::remove_file(&plugin_path) {
                Ok(()) => {
                    tracing::info!("插件文件已删除: {}", plugin_path.display());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// 获取 OpenCode 配置的根目录 (~/.config/opencode/)
    ///
    /// v33：USERPROFILE 与 HOME 都缺失时显式报错（与
    /// [`crate::config::global_config_dir`] 的「写错位置比报错更隐蔽」
    /// 语义统一），不再回退 `.` 静默写当前目录。
    pub fn config_dir() -> Result<PathBuf> {
        let userprofile = std::env::var("USERPROFILE").ok();
        let home = std::env::var("HOME").ok();
        Self::config_dir_from(userprofile.as_deref(), home.as_deref())
    }

    /// 纯函数版配置根目录解析（N11/v33 语义）——Windows 下
    /// USERPROFILE 优先于 HOME（两者都存在时 HOME 可能是
    /// Cygwin/残留值）。拆成纯函数以便测试不依赖进程级环境变量
    /// （并行测试对全局 env 的读写是竞态）。
    pub fn config_dir_from(userprofile: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
        let user_home = userprofile.or(home).map(PathBuf::from).ok_or_else(|| {
            anyhow::anyhow!("无法确定用户级配置目录（USERPROFILE 与 HOME 均未设置）")
        })?;
        Ok(user_home.join(".config").join("opencode"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// 在临时目录中创建模拟的 opencode.json（每个测试独立目录，防并行冲突）
    fn setup_temp_config(initial: Option<&str>) -> (PathBuf, PathBuf) {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "code-repo-wiki-opencode-test-{}-{}",
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");
        let path = dir.join("opencode.json");

        if let Some(content) = initial {
            std::fs::write(&path, content).expect("写入临时配置文件失败");
        }

        (dir, path)
    }

    /// 在临时目录创建用户级插件文件（{dir}/plugins/code-repo-wiki.ts——
    /// v39 起插件装 Agent 配置根目录），返回文件路径
    fn setup_plugin_file(dir: &Path) -> PathBuf {
        let plugin_dir = dir.join("plugins");
        std::fs::create_dir_all(&plugin_dir).expect("创建插件目录失败");
        let path = plugin_dir.join("code-repo-wiki.ts");
        std::fs::write(&path, "export const RepoWikiPlugin = () => ({});")
            .expect("写入插件文件失败");
        path
    }

    /// 在临时目录创建 v39 之前的旧版项目级插件文件
    /// （{dir}/.opencode/plugins/code-repo-wiki.ts——旧 install 产物），
    /// 供迁移清理断言使用
    fn setup_legacy_project_plugin(dir: &Path) -> PathBuf {
        let plugin_dir = dir.join(".opencode").join("plugins");
        std::fs::create_dir_all(&plugin_dir).expect("创建插件目录失败");
        let path = plugin_dir.join("code-repo-wiki.ts");
        std::fs::write(&path, "export const RepoWikiPlugin = () => ({});")
            .expect("写入插件文件失败");
        path
    }

    /// install 应幂等清理历史遗留的无效 plugins 键（旧版本错误写入的复数对象数组）
    #[test]
    fn test_install_plugin_removes_invalid_plugins_key() {
        let initial = r#"{"plugins":[{"name":"code-repo-wiki","path":".opencode/plugins/code-repo-wiki.ts","enabled":true}]}"#;
        let (dir, path) = setup_temp_config(Some(initial));
        let mut config = OpenCodeConfig {
            config_path: path.clone(),
            project_root: dir.clone(),
        };

        config.install_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(
            value.get("plugins").is_none(),
            "install 后不应残留无效的 plugins 键"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// install 对干净配置幂等（不写入任何条目）
    #[test]
    fn test_install_plugin_noop_when_clean() {
        let (dir, path) = setup_temp_config(Some(r#"{}"#));
        let mut config = OpenCodeConfig {
            config_path: path.clone(),
            project_root: dir.clone(),
        };

        config.install_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value.get("plugins").is_none());
        assert_eq!(
            value.as_object().unwrap().len(),
            0,
            "干净配置不应被写入内容"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 配置缺失时 install 创建空对象且无无效键
    #[test]
    fn test_install_plugin_creates_config_when_missing() {
        let (dir, path) = setup_temp_config(None);
        let mut config = OpenCodeConfig {
            config_path: path.clone(),
            project_root: dir.clone(),
        };

        config.install_plugin().unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value.get("plugins").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v52 T08a：配置文件读失败（权限/占用/损坏）必须显式中止——
    /// 静默当空对象会让后续写回覆盖含 OAuth 会话的用户配置
    #[test]
    fn test_install_plugin_aborts_on_unreadable_config() {
        // config_path 指向目录：read_to_string 对目录必然 Err（Windows 权限模拟不可靠）
        let dir =
            std::env::temp_dir().join(format!("rw_opencode_unreadable_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = OpenCodeConfig {
            config_path: dir.clone(),
            project_root: dir.clone(),
        };
        let err = cfg.install_plugin().unwrap_err();
        assert!(
            err.to_string().contains("已中止写回"),
            "应显式中止并说明原因: {err}"
        );
        assert!(dir.is_dir(), "目录本身不应被写坏");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// uninstall 清理无效 plugins 键且保留其他合法键
    #[test]
    fn test_uninstall_plugin_removes_invalid_key_preserves_others() {
        let initial = r#"{"plugins":[{"name":"code-repo-wiki","enabled":true}],"theme":"dark"}"#;
        let (dir, path) = setup_temp_config(Some(initial));
        let mut config = OpenCodeConfig {
            config_path: path.clone(),
            project_root: dir.clone(),
        };

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
        let mut config = OpenCodeConfig {
            config_path: path,
            project_root: dir.clone(),
        };

        config.uninstall_plugin().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// is_installed 以插件文件存在性为准：用户级文件存在 → true
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
        // 临时配置根下没有 plugins/code-repo-wiki.ts
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
        let plugin_dir = dir.join("plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("code-repo-wiki.ts"),
            "export const RepoWikiPlugin = () => ({});",
        )
        .unwrap();
        let config = OpenCodeConfig {
            config_path: dir.join("opencode.json"),
            project_root: dir.clone(),
        };
        assert!(config.is_installed().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v39：旧版项目级插件文件（.opencode/plugins/）不算已安装——安装
    /// 状态以用户级配置根为准；旧文件由迁移逻辑清理
    #[test]
    fn test_is_installed_ignores_legacy_project_plugin() {
        let (dir, _) = setup_temp_config(None);
        setup_legacy_project_plugin(&dir);
        let config = OpenCodeConfig {
            config_path: dir.join("opencode.json"),
            project_root: dir.clone(),
        };
        assert!(
            !config.is_installed().unwrap(),
            "旧版项目级插件不应视为已安装"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v39：install 前清理旧版项目级插件产物（迁移到用户级配置根）
    #[test]
    fn test_install_plugin_file_migrates_legacy_project_plugin() {
        let (dir, _) = setup_temp_config(None);
        let legacy = setup_legacy_project_plugin(&dir);
        let mut config = OpenCodeConfig {
            config_path: dir.join("opencode.json"),
            project_root: dir.clone(),
        };

        let wrote = config.install_plugin_file().unwrap();
        assert!(wrote, "迁移时应实际写入用户级插件文件");
        assert!(!legacy.exists(), "旧版项目级插件文件应被清理");

        let user_plugin = dir.join("plugins").join("code-repo-wiki.ts");
        assert!(user_plugin.exists(), "用户级插件文件应写入");
        assert!(config.is_installed().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v39：uninstall 同时清理用户级插件与旧版项目级残留
    #[test]
    fn test_uninstall_plugin_file_removes_user_and_legacy() {
        let (dir, _) = setup_temp_config(None);
        let user_plugin = setup_plugin_file(&dir);
        let legacy = setup_legacy_project_plugin(&dir);
        let mut config = OpenCodeConfig {
            config_path: dir.join("opencode.json"),
            project_root: dir.clone(),
        };

        config.uninstall_plugin_file().unwrap();

        assert!(!user_plugin.exists(), "用户级插件文件应删除");
        assert!(!legacy.exists(), "旧版项目级插件文件应删除");
        assert!(!config.is_installed().unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// N11：config_dir 优先 USERPROFILE（Windows 语义）
    ///
    /// 纯函数调用——不触碰进程级环境变量（并行测试下 env 是全局竞态，
    /// unsafe set_var/remove_var 的窗口会让其他测试读到被移除的 HOME，
    /// ubuntu 无 APPDATA 兜底时必现——v36 修复后同模式）
    #[test]
    fn test_config_dir_prefers_userprofile() {
        // USERPROFILE 优先于 HOME
        let dir = OpenCodeConfig::config_dir_from(Some("C:\\Users\\testuser"), None).unwrap();
        assert_eq!(
            dir,
            PathBuf::from("C:\\Users\\testuser")
                .join(".config")
                .join("opencode"),
            "USERPROFILE 应优先于 HOME"
        );
        // HOME 兜底（USERPROFILE 缺失）
        let dir2 = OpenCodeConfig::config_dir_from(None, Some("/home/t")).unwrap();
        assert_eq!(
            dir2,
            PathBuf::from("/home/t").join(".config").join("opencode")
        );
        // 双缺失 → 显式报错
        assert!(OpenCodeConfig::config_dir_from(None, None).is_err());
    }

    /// v33：config_dir 双缺失（USERPROFILE 与 HOME 均未设置）→ 显式报错
    /// （与 config::global_config_dir 语义统一，不再回退 "." 写当前目录）。
    /// 纯函数调用——不触碰进程级环境变量（并行测试下 env 是全局竞态）。
    #[test]
    fn test_config_dir_errors_without_home() {
        assert!(OpenCodeConfig::config_dir_from(None, None).is_err());
        assert!(OpenCodeConfig::config_dir_from(Some("C:/Users/t"), None).is_ok());
        assert!(OpenCodeConfig::config_dir_from(None, Some("/home/t")).is_ok());
        // USERPROFILE 优先于 HOME
        let p = OpenCodeConfig::config_dir_from(Some("C:/Users/t"), Some("/home/x")).unwrap();
        assert_eq!(p, PathBuf::from("C:/Users/t/.config/opencode"));
    }

    /// N12：顶层非对象 JSON（数组/标量）→ install/uninstall 显式报错
    #[test]
    fn test_non_object_config_errors() {
        for (tag, initial) in [("arr", "[1,2,3]"), ("str", "\"oops\"")] {
            let (dir, path) = setup_temp_config(Some(initial));
            let mut config = OpenCodeConfig {
                config_path: path.clone(),
                project_root: dir.clone(),
            };
            assert!(
                config.install_plugin().is_err(),
                "install 对非对象配置应报错 ({tag})"
            );
            assert!(
                config.uninstall_plugin().is_err(),
                "uninstall 对非对象配置应报错 ({tag})"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// t02（v16）：install_plugin_file 注入当前可执行文件绝对路径——
    /// 插件不再依赖 PATH（exec 目标为注入路径而非 "code-repo-wiki" 字面量）
    /// v39：落点为用户级配置根 {dir}/plugins/（dir=测试注入的临时配置根）
    #[test]
    fn test_install_plugin_file_injects_absolute_exe_path() {
        let (dir, _) = setup_temp_config(None);
        let mut config = OpenCodeConfig {
            config_path: dir.join("opencode.json"),
            project_root: dir.clone(),
        };

        let wrote = config.install_plugin_file().unwrap();
        assert!(wrote, "首次安装应实际写入插件文件");

        let plugin_path = dir.join("plugins").join("code-repo-wiki.ts");
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
            !content.contains("execa(\"code-repo-wiki\""),
            "PATH 字面量版本不应残留"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// inject_exe_path 命中占位符：替换为注入串且占位符不残留
    #[test]
    fn test_inject_exe_path_hits_placeholder() {
        let template = r#"import { execa } from "execa";
export const RepoWikiPlugin = () => {
    execa("code-repo-wiki", ["--version"]);
};"#;
        let exe_json = r#""C:\\tools\\code-repo-wiki.exe""#;
        let result = inject_exe_path(template, exe_json).unwrap();
        assert!(
            result.contains(&format!("execa({exe_json}")),
            "结果应包含注入串: {result}"
        );
        assert!(
            !result.contains("execa(\"code-repo-wiki\""),
            "占位符不应残留: {result}"
        );
    }

    /// inject_exe_path 占位符缺失（模板改版/字面量变化）时显式报错——
    /// 绝不静默输出 PATH 依赖的插件
    #[test]
    fn test_inject_exe_path_missing_placeholder_errors() {
        let template = r#"import { execa } from "execa";
export const RepoWikiPlugin = () => {
    execa("some-other-tool", ["--version"]);
};"#;
        let err = inject_exe_path(template, r#""/usr/local/bin/code-repo-wiki""#).unwrap_err();
        assert!(
            err.to_string().contains("占位符缺失"),
            "错误应说明占位符缺失: {err}"
        );
    }

    /// audit-cfg-01：插件模板 --config 路径字面量断言——root 提供时指向
    /// `{root}/config.toml`（v25 起项目级配置文件名），死设计
    /// `.code-repo-wiki/config.toml`（v25 前产物目录内配置）不得残留
    /// （15 个工具此前全传该路径致全部失效）。
    #[test]
    fn test_plugin_template_config_path_literal() {
        let raw = include_str!("plugin-template.ts");
        // 新契约：--config 只可能指向项目根 config.toml（configArg 拼字面量）
        assert!(
            raw.contains("`${root}/config.toml`"),
            "模板 --config 应指向 {{root}}/config.toml，实际模板:\n{}",
            raw.chars().take(1200).collect::<String>()
        );
        // 旧 configPath 函数（死路径生成器）必须整体移除
        assert!(
            !raw.contains("configPath"),
            "模板不得残留旧 configPath 函数（生成 .code-repo-wiki/config.toml 死路径）"
        );
        // 旧拼接形态 `"--config", configPath(...)` 不得残留
        assert!(
            !raw.contains("--config\", configPath("),
            "模板不得残留旧 --config 拼接形态"
        );
    }
}
