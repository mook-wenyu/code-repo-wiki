pub mod opencode;
pub mod plan;
pub mod schema;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::project::ProjectRoot;

/// 从文件加载配置，缺失字段用默认值填充
pub fn load_config(path: &Path) -> Result<schema::WikiConfig> {
    if !path.exists() {
        // t05（v21）：显式 --config 缺失时给出一键引导——裸报"文件不存在"
        // 会让外部 Agent 无从下手；init 命令是创建默认配置的官方入口。
        anyhow::bail!(
            "配置文件不存在: {}（可运行 `repo-wiki init` 创建默认配置）",
            path.display()
        );
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
    let config: schema::WikiConfig = toml::from_str(&content)
        .with_context(|| format!("解析配置文件失败: {}", path.display()))?;
    validate_config(&config)?;
    Ok(config)
}

/// 创建默认配置文件（写入 install 模板 default-config.toml，非 schema 默认值序列化）。
/// 模板含注释与生产默认值（如 DeepSeek base_url），serde 序列化会丢失这些信息。
pub fn create_default_config(path: &Path) -> Result<schema::WikiConfig> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, include_str!("../../default-config.toml"))?;
    load_config(path)
}

/// 全局（用户级）配置目录的纯路径组装（可测试，不读环境变量）
///
/// 平台语义（用户拍板，v13 E 组）：
/// - Windows：`%APPDATA%/repo-wiki`（Roaming AppData 是 Windows 用户级
///   应用数据的标准位置，随用户漫游）
/// - 其他平台：`$HOME/repo-wiki`（无 XDG 前缀，用户指定）
/// - APPDATA 缺失（非常见环境）时退化 `$HOME/repo-wiki`；
///   APPDATA 与 HOME 都缺失时返回 Err——无法确定用户级目录时显式报错，
///   不静默写当前目录（写错位置比报错更隐蔽）。
pub fn global_config_dir_from(appdata: Option<&Path>, home: Option<&Path>) -> Result<PathBuf> {
    match appdata {
        Some(p) if !p.as_os_str().is_empty() => Ok(p.join("repo-wiki")),
        _ => home
            .filter(|h| !h.as_os_str().is_empty())
            .ok_or_else(|| anyhow::anyhow!("无法确定用户级配置目录（APPDATA 与 HOME 均未设置）"))
            .map(|h| h.join("repo-wiki")),
    }
}

/// 全局（用户级）配置目录（读环境变量，委托纯函数）
///
/// Windows 语义（N11 先例，opencode.rs config_dir）：USERPROFILE 优先于
/// HOME——Windows 构建工具（Git Bash/Cygwin/MSYS）常把 HOME 指向临时值，
/// USERPROFILE 才是用户真实主目录。非 Windows 平台用 HOME。
pub fn global_config_dir() -> Result<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from);
    let appdata = std::env::var("APPDATA").ok().map(PathBuf::from);
    global_config_dir_from(appdata.as_deref(), home.as_deref())
}

/// 默认配置文件解析：项目级 → 全局 → 创建全局（用户拍板，v13 E 组）
///
/// 搜索链（无 `--config` 显式指定时）：
/// 1. `{项目根}/.repo-wiki/config.toml` 存在 → 用它（项目级配置优先，
///    随 Git 提交共享，多项目隔离）；
/// 2. 全局 `{用户级目录}/config.toml` 存在 → 用它（用户默认偏好）；
/// 3. 都不存在 → 创建全局目录 + 写入默认配置模板，返回全局路径
///    （引导式就绪：无配置时自动落位，不需要用户先手动 init）。
///
/// global_dir 由调用方注入（测试传临时目录），生产入口传
/// [`global_config_dir`] 的结果。
pub fn resolve_default_config_path_with(root: &ProjectRoot, global_dir: &Path) -> Result<PathBuf> {
    let project_config = root.path().join(".repo-wiki").join("config.toml");
    if project_config.exists() {
        return Ok(project_config);
    }
    let global_config = global_dir.join("config.toml");
    if global_config.exists() {
        return Ok(global_config);
    }
    std::fs::create_dir_all(global_dir)
        .with_context(|| format!("创建全局配置目录失败: {}", global_dir.display()))?;
    create_default_config(&global_config)?;
    Ok(global_config)
}

/// 默认配置文件解析（生产入口，全局目录按环境变量解析）
pub fn resolve_default_config_path(root: &ProjectRoot) -> Result<PathBuf> {
    resolve_default_config_path_with(root, &global_config_dir()?)
}

/// `--config` 参数解析：显式指定原样使用（不存在时由 load_config 报错）；
/// 缺省走默认配置链（见 [`resolve_default_config_path`]）。
pub fn resolve_config_path(config: Option<&Path>, root: &ProjectRoot) -> Result<PathBuf> {
    match config {
        Some(p) => Ok(p.to_path_buf()),
        None => resolve_default_config_path(root),
    }
}

/// 校验配置合法性
fn validate_config(config: &schema::WikiConfig) -> Result<()> {
    if config.output.dir.is_empty() {
        anyhow::bail!("output.dir 不能为空");
    }
    if config.scope.include.is_empty() {
        anyhow::bail!("scope.include 至少需要一个模式");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_roundtrip() {
        let config = schema::WikiConfig::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: schema::WikiConfig = toml::from_str(&toml_str).unwrap();
        // v17 t05：schema 默认值统一到模板阵营（DeepSeek）
        assert_eq!(parsed.llm.model, "deepseek-v4-flash");
        assert_eq!(parsed.llm.api_key_env, "DEEPSEEK_API_KEY");
        assert_eq!(parsed.wiki.language, "zh");
        assert_eq!(parsed.output.dir, ".repo-wiki");
    }

    #[test]
    fn test_validate_empty_include() {
        let mut config = schema::WikiConfig::default();
        config.scope.include = vec![];
        assert!(validate_config(&config).is_err());
    }
}


    // ============ E 组：全局配置链 ============

    /// 全局目录路径组装：APPDATA 提供时拼 %APPDATA%/repo-wiki
    #[test]
    fn test_global_config_dir_from_appdata() {
        let dir = global_config_dir_from(Some(Path::new("C:/Users/wenyu/AppData/Roaming")), Some(Path::new("C:/Users/wenyu")))
            .unwrap();
        assert_eq!(dir, PathBuf::from("C:/Users/wenyu/AppData/Roaming/repo-wiki"));
    }

    /// 全局目录路径组装：APPDATA 缺失（非 Windows）时退化 $HOME/repo-wiki
    #[test]
    fn test_global_config_dir_from_home_fallback() {
        let dir = global_config_dir_from(None, Some(Path::new("/home/wenyu"))).unwrap();
        assert_eq!(dir, PathBuf::from("/home/wenyu/repo-wiki"));
    }

    /// APPDATA 与 HOME 都缺失：显式报错（不静默写当前目录）
    #[test]
    fn test_global_config_dir_from_missing_both_errors() {
        assert!(global_config_dir_from(None, None).is_err());
        assert!(global_config_dir_from(None, Some(Path::new(""))).is_err());
    }

    /// E 组搜索链：项目级配置存在 → 返回项目级（项目级优先）
    #[test]
    fn test_resolve_prefers_project_config() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_e_project_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".repo-wiki")).unwrap();
        std::fs::write(dir.join(".repo-wiki").join("config.toml"), "dummy").unwrap();
        let global_dir = dir.join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(global_dir.join("config.toml"), "dummy-global").unwrap();

        let resolved = resolve_default_config_path_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
        assert_eq!(resolved, dir.join(".repo-wiki").join("config.toml"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// E 组搜索链：项目级缺失、全局存在 → 返回全局
    #[test]
    fn test_resolve_falls_back_to_global() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_e_global_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let global_dir = dir.join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(global_dir.join("config.toml"), "dummy-global").unwrap();

        let resolved = resolve_default_config_path_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
        assert_eq!(resolved, global_dir.join("config.toml"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// E 组搜索链：项目级与全局都缺失 → 创建全局目录 + 默认配置，返回全局路径
    #[test]
    fn test_resolve_creates_global_config_when_missing() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_e_create_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let global_dir = dir.join("global");

        let resolved = resolve_default_config_path_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
        assert_eq!(resolved, global_dir.join("config.toml"));
        assert!(global_dir.join("config.toml").exists(), "缺失时应创建全局默认配置");
        // 创建的配置必须可加载（模板完整）
        assert!(load_config(&resolved).is_ok());

        // 幂等：再次解析仍返回同一路径，不重复创建
        let resolved2 = resolve_default_config_path_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
        assert_eq!(resolved2, resolved);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// resolve_config_path：显式指定原样返回（不触发创建）
    #[test]
    fn test_resolve_config_path_explicit_wins() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_e_explicit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let explicit = dir.join("custom.toml");
        let resolved = resolve_config_path(Some(&explicit), &ProjectRoot::new(dir.clone())).unwrap();
        assert_eq!(resolved, explicit);
        // 显式指定不创建全局目录/文件
        assert!(!dir.join("global").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
