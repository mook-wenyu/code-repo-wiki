pub mod opencode;
pub mod plan;
pub mod schema;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::project::ProjectRoot;

/// 项目级独立配置文件名（v24 拍板：与产物目录 `.repo-wiki/` 物理分离，
/// 配置属非产物内容；自动创建只发生在用户级目录）
pub const PROJECT_CONFIG_FILE: &str = ".repo-wiki.toml";

/// 项目级配置中禁止携带的敏感/机器属性键（Codex DENYLIST 模式，
/// v24 用户拍板）：凭据、提供商、模型归属用户级配置或 `--config`
/// 显式指定——防止随仓库传播造成凭据重定向、模型锁死
pub const PROJECT_CONFIG_DENY_KEYS: &[(&str, &str)] = &[
    ("llm", "provider"),
    ("llm", "model"),
    ("llm", "base_url"),
    ("llm", "api_key_env"),
    ("embed", "model"),
    ("embed", "base_url"),
    ("embed", "api_key_env"),
];

/// 敏感键净化后的注入默认值（schema LlmSection 三字段必填无 serde 默认，
/// 与 schema Default 阵营对齐：OpenAI 协议 + DeepSeek 模板）——
/// 「敏感键不生效」而非「配置缺失报错」
const SANITIZE_DEFAULT_INJECT: &[(&str, &str, &str)] = &[
    ("llm", "provider", "openai"),
    ("llm", "model", "deepseek-v4-flash"),
    ("llm", "api_key_env", "DEEPSEEK_API_KEY"),
    // embed.model/api_key_env 同为必填（无 serde 默认）：默认模板自身含
    // 这些键，净化后需回填，否则 init 写出的 .repo-wiki.toml 无法再加载
    ("embed", "model", "text-embedding-3-small"),
    ("embed", "api_key_env", "OPENAI_API_KEY"),
];

/// 项目级配置净化：移除敏感键并告警（返回净化后的 TOML 文本）
///
/// 用 `toml::Value` 中间层移除命中键后重新序列化——净化结果只用于本次
/// 解析，丢失注释无碍。TOML 本身非法或序列化失败时原样返回（由后续
/// 解析报出真实错误，不吞错）。
fn sanitize_project_config(text: &str) -> String {
    let mut value: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(_) => return text.to_string(),
    };
    for (section, key) in PROJECT_CONFIG_DENY_KEYS {
        if let Some(tbl) = value.get_mut(*section).and_then(|v| v.as_table_mut())
            && tbl.remove(*key).is_some()
        {
            tracing::warn!(
                "项目级配置 {section}.{key} 属敏感/机器属性键，已忽略——请移入用户级配置或使用 --config 显式指定"
            );
        }
    }
    // 必填字段净化后注入 schema 默认，防止解析失败（见常量注释）
    for (section, key, default) in SANITIZE_DEFAULT_INJECT {
        if let Some(tbl) = value.get_mut(*section).and_then(|v| v.as_table_mut())
            && !tbl.contains_key(*key)
        {
            tbl.insert(key.to_string(), toml::Value::String((*default).to_string()));
        }
    }
    toml::to_string(&value).unwrap_or_else(|_| text.to_string())
}

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
    // v24：项目级独立配置文件（文件名 .repo-wiki.toml）执行敏感键净化——
    // 显式 --config 指向该文件同样生效（该文件语义即"项目级配置"，逃生门
    // 不豁免安全护栏；其余文件名/全局配置不净化）
    let text = if path.file_name().is_some_and(|n| n == PROJECT_CONFIG_FILE) {
        sanitize_project_config(&content)
    } else {
        content
    };
    let config: schema::WikiConfig = toml::from_str(&text)
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

/// 默认配置文件解析：项目级 → 全局 → 创建全局（用户拍板，v13 E 组；
/// v24 调整：项目级配置为独立文件 `.repo-wiki.toml`，与产物目录分离）
///
/// 搜索链（无 `--config` 显式指定时）：
/// 1. `{项目根}/.repo-wiki.toml` 存在 → 用它（项目级配置优先，
///    随 Git 提交共享，多项目隔离；敏感键净化见 [`sanitize_project_config`]）；
/// 2. 全局 `{用户级目录}/config.toml` 存在 → 用它（用户默认偏好）；
/// 3. 都不存在 → 创建全局目录 + 写入默认配置模板，返回全局路径
///    （引导式就绪：自动创建只发生在用户级目录，项目级永不自动创建——
///    v24 用户要求，install 命令的项目级配置创建点已移除）。
///
/// global_dir 由调用方注入（测试传临时目录），生产入口传
/// [`global_config_dir`] 的结果。
pub fn resolve_default_config_path_with(root: &ProjectRoot, global_dir: &Path) -> Result<PathBuf> {
    let project_config = root.path().join(PROJECT_CONFIG_FILE);
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

    /// E 组搜索链：项目级配置存在 → 返回项目级（项目级优先；v24 起为
    /// 独立文件 `.repo-wiki.toml`，不再混入产物目录）
    #[test]
    fn test_resolve_prefers_project_config() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_e_project_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PROJECT_CONFIG_FILE), "dummy").unwrap();
        let global_dir = dir.join("global");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(global_dir.join("config.toml"), "dummy-global").unwrap();

        let resolved = resolve_default_config_path_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
        assert_eq!(resolved, dir.join(PROJECT_CONFIG_FILE));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v24：项目级独立配置文件加载时，敏感键（provider/base_url/api_key_env/
    /// model）被移除并回退默认值——不随仓库传播
    #[test]
    fn test_load_project_config_sanitizes_sensitive_keys() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_sanitize_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PROJECT_CONFIG_FILE);
        // 项目级配置只声明项目契约（scope/语言/输出），敏感键被写入也无效
        std::fs::write(
            &path,
            r#"
[wiki]
language = "en"

[scope]
include = ["src/**"]
exclude = ["target/**"]

[output]
dir = "docs"

[llm]
provider = "anthropic"
model = "claude-opus"
api_key_env = "HACKED_KEY"
"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        // 净化后敏感键回退 schema 默认（模板阵营 deepseek）
        assert_eq!(config.llm.provider, crate::config::schema::LlmProviderType::OpenAI);
        assert_eq!(config.llm.model, "deepseek-v4-flash");
        assert_eq!(config.llm.api_key_env, "DEEPSEEK_API_KEY");
        // 项目契约保留
        assert_eq!(config.wiki.language, "en");
        assert_eq!(config.output.dir, "docs");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v24：非项目级文件名（全局配置/显式 --config 其他文件）不净化
    #[test]
    fn test_load_explicit_config_keeps_sensitive_keys() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_nosanitize_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("my.toml");
        std::fs::write(
            &path,
            r#"
[scope]
include = ["src/**"]
exclude = ["target/**"]

[llm]
provider = "anthropic"
model = "claude-opus"
api_key_env = "ANTHROPIC_API_KEY"
"#,
        )
        .unwrap();

        let config = load_config(&path).unwrap();
        // 用户级/显式配置完整保留敏感键
        assert_eq!(config.llm.provider, crate::config::schema::LlmProviderType::Anthropic);
        assert_eq!(config.llm.model, "claude-opus");

        let _ = std::fs::remove_dir_all(&dir);
    }
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
