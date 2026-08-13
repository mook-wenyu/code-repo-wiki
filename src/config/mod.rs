/// 多 Agent MCP 配置读写（v33；v39 落点统一用户级：opencode 全局 /
/// Claude Code ~/.claude.json User scope / Codex config.toml）
pub mod mcp;
pub mod opencode;

pub mod schema;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::project::ProjectRoot;

/// 项目级配置文件（v25 拍板：项目根 `config.toml`，字段级合并覆盖
/// 用户级配置；v24 的 `.code-repo-wiki.toml` 与 v25 用户级默认文件更名，旧名不再读取）
pub const PROJECT_CONFIG_FILE: &str = "config.toml";

/// 用户级全局配置文件（v25 拍板：`config.toml`，与内置模板
/// 同名同构；v24 及以前的全局 `config.toml` 已废弃不再读取）
pub const USER_CONFIG_FILE: &str = "config.toml";

/// 从文件加载配置（v30：原样解析，无净化无注入——缺失字段由 schema
/// 字段级 serde 默认兜底，见 schema.rs LlmSection/EmbedSection 等；
/// 项目级 config.toml 的 base_url/api_key_env 完整生效，用户拍板）
pub fn load_config(path: &Path) -> Result<schema::WikiConfig> {
    if !path.exists() {
        // t05（v21）：显式 --config 缺失时给出一键引导——裸报"文件不存在"
        // 会让外部 Agent 无从下手；init 命令是创建默认配置的官方入口。
        anyhow::bail!(
            "配置文件不存在: {}（可运行 `code-repo-wiki install` 确保用户级默认配置，或使用 --config 显式指定）",
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

/// 创建默认配置文件（写入 install 模板 config.toml，非 schema 默认值序列化）。
/// 模板含注释与生产默认值（如 DeepSeek base_url），serde 序列化会丢失这些信息。
///
/// audit-cfg-02：用户级配置目录可能承载明文 api_key，创建即收紧 Unix
/// 权限（文件 0600 + 目录 0700），避免「创建后、key 写入前」窗口期被
/// 同机其他用户读取。
pub fn create_default_config(path: &Path) -> Result<schema::WikiConfig> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        crate::fs::restrict_private_permissions(parent)?;
    }
    std::fs::write(path, include_str!("../../config.toml"))?;
    crate::fs::restrict_private_permissions(path)?;
    load_config(path)
}

/// 全局（用户级）配置目录的纯路径组装（可测试，不读环境变量）
///
/// 平台语义（用户拍板，v41——对齐 Codex/Claude Code/Azure CLI 等官方
/// home 点目录惯例：`~/.codex`、`~/.claude`、`%USERPROFILE%\.azure`）：
/// - Windows：`%USERPROFILE%/.code-repo-wiki`（用户主目录点目录）
/// - 其他平台：`$HOME/.code-repo-wiki`
/// - USERPROFILE 缺失（非 Windows 环境）时退化 `$HOME/.code-repo-wiki`；
///   USERPROFILE 与 HOME 都缺失时返回 Err——无法确定用户级目录时显式
///   报错，不静默写当前目录（写错位置比报错更隐蔽）。
pub fn global_config_dir_from(userprofile: Option<&Path>, home: Option<&Path>) -> Result<PathBuf> {
    match userprofile {
        Some(p) if !p.as_os_str().is_empty() => Ok(p.join(".code-repo-wiki")),
        _ => home
            .filter(|h| !h.as_os_str().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("无法确定用户级配置目录（USERPROFILE 与 HOME 均未设置）")
            })
            .map(|h| h.join(".code-repo-wiki")),
    }
}

/// 全局（用户级）配置目录（读环境变量，委托纯函数）
///
/// 解析优先级（v41 拍板）：
/// 1. `CODE_REPO_WIKI_HOME` 环境变量——显式重定位（对齐 CODEX_HOME 惯例，
///    用户可自定义配置根；设置后不做旧路径迁移，旧目录由用户自行处理）；
/// 2. USERPROFILE（Windows 用户真实主目录——Git Bash/Cygwin/MSYS 常把
///    HOME 指向临时值；N11 先例同 opencode.rs config_dir）；
/// 3. HOME。
pub fn global_config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var("CODE_REPO_WIKI_HOME")
        .ok()
        .filter(|v| !v.is_empty())
    {
        return Ok(PathBuf::from(dir));
    }
    let userprofile = std::env::var("USERPROFILE").ok().map(PathBuf::from);
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    global_config_dir_from(userprofile.as_deref(), home.as_deref())
}
/// 全局（用户级）配置目录就绪入口（v41）：解析目录（含
/// `CODE_REPO_WIKI_HOME` 重定位）+ 一次性迁移 v41 前的旧路径
/// （`%APPDATA%/code-repo-wiki` 与 `$HOME/code-repo-wiki`——参考 git
/// 双读与 Claude Code 弃用兼容先例：新路径优先、旧目录内容复制到新
/// 路径、旧目录保留不删；`CODE_REPO_WIKI_HOME` 显式指定时跳过迁移）。
///
/// 返回（目录, 是否发生迁移）——迁移发生时调用方打印提示。
/// 生产入口（lib.rs 配置加载、key 命令）用它替代 [`global_config_dir`]。
pub fn ensure_global_config_dir() -> Result<(PathBuf, bool)> {
    let dir = global_config_dir()?;
    let legacy_dirs: Vec<PathBuf> = if std::env::var("CODE_REPO_WIKI_HOME").is_ok() {
        Vec::new()
    } else {
        let mut legacy = Vec::new();
        if let Some(appdata) = std::env::var("APPDATA").ok().filter(|v| !v.is_empty()) {
            legacy.push(PathBuf::from(appdata).join("code-repo-wiki"));
        }
        if let Some(home) = std::env::var("HOME").ok().filter(|v| !v.is_empty()) {
            legacy.push(PathBuf::from(home).join("code-repo-wiki"));
        }
        legacy
    };
    let migrated = migrate_global_config(&dir, &legacy_dirs)?;
    Ok((dir, migrated))
}

/// 一次性迁移旧用户级配置目录（纯逻辑，可测试）：新目录已有
/// `config.toml` 时不迁移（新配置优先）；否则按序检查候选旧目录，
/// 第一个含 `config.toml` 的旧目录整个复制到新目录。
///
/// 返回是否发生了迁移。旧目录保留不删（配置属用户资产）。
pub fn migrate_global_config(new_dir: &Path, legacy_dirs: &[PathBuf]) -> Result<bool> {
    if new_dir.join(USER_CONFIG_FILE).exists() {
        return Ok(false);
    }
    for legacy in legacy_dirs {
        if !legacy.join(USER_CONFIG_FILE).exists() {
            continue;
        }
        std::fs::create_dir_all(new_dir)
            .with_context(|| format!("创建全局配置目录失败: {}", new_dir.display()))?;
        copy_dir_contents(legacy, new_dir)?;
        return Ok(true);
    }
    Ok(false)
}

/// 递归复制目录内容（迁移用；简单文件复制——配置目录无符号链接场景）
fn copy_dir_contents(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("创建目录失败: {}", to.display()))?;
    for entry in
        std::fs::read_dir(from).with_context(|| format!("读取目录失败: {}", from.display()))?
    {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_dir_contents(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)
                .with_context(|| format!("复制失败: {} → {}", src.display(), dst.display()))?;
        }
    }
    Ok(())
}

/// 字段级 TOML 合并（v25 拍板，主流工具语义，参考 uv/Claude Code/cargo
/// 官方 merge 文档）：表递归合并——overlay 命中的键整体取 overlay
/// （标量/数组走 VS Code"完整清单"语义：数组整体覆盖而非追加），
/// 未命中的键取 base；非表节点 overlay 整体覆盖。
fn merge_config(base: &toml::Value, overlay: &toml::Value) -> toml::Value {
    match (base, overlay) {
        (toml::Value::Table(base_tbl), toml::Value::Table(overlay_tbl)) => {
            let mut merged = base_tbl.clone();
            for (key, overlay_val) in overlay_tbl {
                // 两侧同构子表递归合并（如 [llm] 内只覆盖 model）
                let recursive = merged
                    .get(key)
                    .is_some_and(|bv| bv.is_table() && overlay_val.is_table());
                if recursive {
                    let base_child = merged.get(key).unwrap().clone();
                    merged.insert(key.clone(), merge_config(&base_child, overlay_val));
                } else {
                    // 其余（标量/数组/异构形态）整体覆盖
                    merged.insert(key.clone(), overlay_val.clone());
                }
            }
            toml::Value::Table(merged)
        }
        _ => overlay.clone(),
    }
}

/// 默认配置链加载（v25 拍板，核心入口）：项目级 `config.toml` 字段级
/// 合并覆盖用户级 `config.toml`，返回（实际来源路径, 配置）。
///
/// 链：
/// 1. 项目级存在 → base = 用户级（存在时）或内置模板；项目级原样解析
///    后字段级合并覆盖 base（项目级只写要覆盖的键，其余继承用户级；
///    数组整体覆盖；缺键由 schema 字段级 serde 默认兜底）
/// 2. 项目级不存在 → 用户级存在 → 用之（原样加载不合并）
/// 3. 都缺 → 创建用户级默认配置（模板）→ 用之（自动创建只发生在
///    用户级目录，项目级永不自动创建——v24 用户要求延续）
///
/// 与 [`resolve_default_config_path`] 的区别：本函数返回合并后的完整
/// 配置（合成内容不落盘），路径解析函数只做文件定位。
///
/// v30 用户拍板：彻底删除净化/注入规则——项目级配置原样解析，任何键
/// （含 base_url/api_key_env）完整生效，缺失字段走 schema serde 默认。
pub fn load_default_config_with(
    root: &ProjectRoot,
    global_dir: &Path,
) -> Result<(PathBuf, schema::WikiConfig)> {
    let project_config = root.path().join(PROJECT_CONFIG_FILE);
    let user_config = global_dir.join(USER_CONFIG_FILE);
    if project_config.exists() {
        let base_text = if user_config.exists() {
            std::fs::read_to_string(&user_config)
                .with_context(|| format!("读取用户级配置失败: {}", user_config.display()))?
        } else {
            include_str!("../../config.toml").to_string()
        };
        let base: toml::Value = toml::from_str(&base_text)
            .with_context(|| "解析用户级配置（或模板）失败".to_string())?;
        let project_text = std::fs::read_to_string(&project_config)
            .with_context(|| format!("读取项目级配置失败: {}", project_config.display()))?;
        let overlay: toml::Value = toml::from_str(&project_text)
            .with_context(|| format!("解析项目级配置失败: {}", project_config.display()))?;
        // audit-cfg-06：项目级文件自身含明文 api_key → 显式警告（见 helper 注释）
        warn_project_plaintext_key(&overlay);
        let merged = merge_config(&base, &overlay);
        let text = toml::to_string(&merged).context("合并配置序列化失败")?;
        let config: schema::WikiConfig = toml::from_str(&text)
            .with_context(|| format!("解析合并后配置失败: {}", project_config.display()))?;
        validate_config(&config)?;
        Ok((project_config, config))
    } else if user_config.exists() {
        let config = load_config(&user_config)?;
        Ok((user_config, config))
    } else {
        std::fs::create_dir_all(global_dir)
            .with_context(|| format!("创建全局配置目录失败: {}", global_dir.display()))?;
        create_default_config(&user_config)?;
        let config = load_config(&user_config)?;
        Ok((user_config, config))
    }
}

/// 默认配置链加载（生产入口，全局目录按环境变量解析 + 旧路径迁移）
pub fn load_default_config(root: &ProjectRoot) -> Result<(PathBuf, schema::WikiConfig)> {
    let (global_dir, migrated) = ensure_global_config_dir()?;
    if migrated {
        println!(
            "提示: 用户级配置已迁移到 {}（旧目录保留，未删除）",
            global_dir.display()
        );
    }
    load_default_config_with(root, &global_dir)
}

/// 默认配置文件解析：项目级 → 全局 → 创建全局（用户拍板，v13 E 组；
/// v25 调整：项目级 `config.toml` 字段级合并覆盖用户级
/// `config.toml`——完整合并语义见 [`load_default_config_with`]）
///
/// 搜索链（无 `--config` 显式指定时）：
/// 1. `{项目根}/config.toml` 存在 → 用它（项目级配置优先，
///    随 Git 提交共享，多项目隔离；原样解析，缺失字段由 schema 默认）；
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
    let global_config = global_dir.join(USER_CONFIG_FILE);
    if global_config.exists() {
        return Ok(global_config);
    }
    std::fs::create_dir_all(global_dir)
        .with_context(|| format!("创建全局配置目录失败: {}", global_dir.display()))?;
    create_default_config(&global_config)?;
    Ok(global_config)
}

/// 默认配置文件解析（生产入口，全局目录按环境变量解析 + 旧路径迁移）
pub fn resolve_default_config_path(root: &ProjectRoot) -> Result<PathBuf> {
    let (global_dir, migrated) = ensure_global_config_dir()?;
    if migrated {
        println!(
            "提示: 用户级配置已迁移到 {}（旧目录保留，未删除）",
            global_dir.display()
        );
    }
    resolve_default_config_path_with(root, &global_dir)
}

/// `--config` 参数解析：显式指定原样使用（不存在时由 load_config 报错）；
/// 缺省走默认配置链（见 [`resolve_default_config_path`]）。
pub fn resolve_config_path(config: Option<&Path>, root: &ProjectRoot) -> Result<PathBuf> {
    match config {
        Some(p) => Ok(p.to_path_buf()),
        None => resolve_default_config_path(root),
    }
}

/// 校验配置合法性（audit-cfg-03/04/05：算法项已硬编码，以下为必填契约与
/// 值域校验——违反即报错，阻止带病配置进入运行期；与 provider 构造器的
/// max_concurrency>0 运行时守卫（llm.rs/embed.rs）同源，解析期先行拦截）
fn validate_config(config: &schema::WikiConfig) -> Result<()> {
    // wiki.language：单段语言代码，字符白名单 [A-Za-z0-9_-]（与 MCP 侧
    // validate_lang_segment 同口径，src/mcp.rs:338）。语言值会被拼进产物
    // 路径（output/wiki/{language}/），非法字符（/ \ .. 空格 盘符等）会
    // 造成路径穿越或脏路径——audit-cfg-04。
    if config.wiki.language.is_empty()
        || !config
            .wiki
            .language
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "配置错误: wiki.language = {:?} 不合法（仅允许 [A-Za-z0-9_-] 单段，如 \"zh\"、\"en_US\"）",
            config.wiki.language
        );
    }
    // llm.reasoning_effort：值域白名单（v50 DeepSeek 官方映射 low/high/max，
    // 见 schema.rs LlmSection 注释）——非法值原样透传 API 会在供应商侧报错，
    // 解析期拦截给出明确提示——audit-cfg-05。
    if let Some(level) = &config.llm.reasoning_effort
        && !matches!(level.as_str(), "low" | "high" | "max")
    {
        anyhow::bail!(
            "配置错误: llm.reasoning_effort = {:?} 不合法（仅支持 \"low\" / \"high\" / \"max\"）",
            level
        );
    }
    // max_concurrency：必须为正整数——0 会让 Semaphore::new(0) 无许可，
    // 运行期表现为永久挂起（llm.rs/embed.rs 构造器同款守卫，解析期先行）。
    if config.llm.max_concurrency == Some(0) {
        anyhow::bail!("配置错误: llm.max_concurrency 必须大于 0");
    }
    if config.embed.max_concurrency == Some(0) {
        anyhow::bail!("配置错误: embed.max_concurrency 必须大于 0");
    }
    // model/base_url：非空——空模型名/空端点会请求到空串，供应商侧报错
    // 且错误难以定位；schema 默认值均非空，此处只拦截显式写空的配置。
    if config.llm.model.trim().is_empty() {
        anyhow::bail!("配置错误: llm.model 不能为空");
    }
    if let Some(url) = &config.llm.base_url
        && url.trim().is_empty()
    {
        anyhow::bail!("配置错误: llm.base_url 不能为空");
    }
    if config.embed.model.trim().is_empty() {
        anyhow::bail!("配置错误: embed.model 不能为空");
    }
    if let Some(url) = &config.embed.base_url
        && url.trim().is_empty()
    {
        anyhow::bail!("配置错误: embed.base_url 不能为空");
    }
    Ok(())
}

/// 项目级配置明文 api_key 显式警告（audit-cfg-06）
///
/// 项目级 config.toml 随 Git 提交共享，明文 api_key 一旦提交即泄露（v30
/// 起项目级配置原样解析、不净化，明文键完整生效）。默认配置链的项目分支
/// 加载时检查原始 overlay——只对「项目级文件自身含明文键」告警，用户级
/// 配置（明文键的合法存放位置）不受影响。引导改用 api_key_env 环境变量
/// 引用（`code-repo-wiki key --env` 可自动写入建议名）。
///
/// 为什么不在 load_config 内告警：load_config 同时服务用户级加载（mod.rs
/// 225/231、key.rs 写后验证），无法区分文件级别，用户级明文键是设计内的
/// 合法存放，不该每次加载都告警。
fn warn_project_plaintext_key(overlay: &toml::Value) {
    let has_plain = |section: &str| {
        overlay
            .get(section)
            .and_then(|t| t.get("api_key"))
            .and_then(|k| k.as_str())
            .is_some_and(|s| !s.is_empty())
    };
    if has_plain("llm") || has_plain("embed") {
        eprintln!(
            "警告: 项目级 config.toml 包含明文 api_key（随 Git 共享有泄露风险；建议改用 api_key_env 环境变量引用，可运行 `code-repo-wiki key --env` 自动写入）"
        );
    }
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
        assert_eq!(parsed.llm.api_key_env, "OPENCODEGO2_API_KEY");
        assert_eq!(parsed.wiki.language, "zh");
        assert_eq!(
            parsed.output_dir(),
            std::path::Path::new(crate::config::schema::OUTPUT_DIR)
        );
    }
}

// ============ E 组：全局配置链 ============

/// 全局目录路径组装：USERPROFILE 提供时拼 %USERPROFILE%/.code-repo-wiki
/// （v41 拍板——home 点目录惯例，对齐 ~/.codex、~/.claude）
#[test]
fn test_global_config_dir_from_userprofile() {
    let dir = global_config_dir_from(
        Some(Path::new("C:/Users/wenyu")),
        Some(Path::new("/home/wenyu")),
    )
    .unwrap();
    assert_eq!(dir, PathBuf::from("C:/Users/wenyu/.code-repo-wiki"));
}

/// 全局目录路径组装：USERPROFILE 缺失（非 Windows）时退化 $HOME/.code-repo-wiki
#[test]
fn test_global_config_dir_from_home_fallback() {
    let dir = global_config_dir_from(None, Some(Path::new("/home/wenyu"))).unwrap();
    assert_eq!(dir, PathBuf::from("/home/wenyu/.code-repo-wiki"));
}

/// USERPROFILE 与 HOME 都缺失：显式报错（不静默写当前目录）
#[test]
fn test_global_config_dir_from_missing_both_errors() {
    assert!(global_config_dir_from(None, None).is_err());
    assert!(global_config_dir_from(None, Some(Path::new(""))).is_err());
}

/// 一次性迁移：新目录无 config.toml 且旧目录存在 → 复制内容 + 返回 true
#[test]
fn test_migrate_global_config_migrates_legacy() {
    let tmp = test_tmp_dir("migrate-legacy");
    let legacy = tmp.join("legacy");
    let new = tmp.join("new");
    std::fs::create_dir_all(legacy.join("sub")).unwrap();
    std::fs::write(legacy.join("config.toml"), "llm_model = 'deepseek'").unwrap();
    std::fs::write(legacy.join("sub/notes.txt"), "abc").unwrap();

    assert!(migrate_global_config(&new, std::slice::from_ref(&legacy)).unwrap());
    assert_eq!(
        std::fs::read_to_string(new.join("config.toml")).unwrap(),
        "llm_model = 'deepseek'"
    );
    assert_eq!(
        std::fs::read_to_string(new.join("sub/notes.txt")).unwrap(),
        "abc"
    );
    // 旧目录保留不删
    assert!(legacy.join("config.toml").exists());
}

/// 一次性迁移：新目录已有 config.toml → 不迁移（新配置优先）
#[test]
fn test_migrate_global_config_skips_when_new_exists() {
    let tmp = test_tmp_dir("migrate-new-exists");
    let legacy = tmp.join("legacy");
    let new = tmp.join("new");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("config.toml"), "old").unwrap();
    std::fs::create_dir_all(&new).unwrap();
    std::fs::write(new.join("config.toml"), "new-content").unwrap();

    assert!(!migrate_global_config(&new, &[legacy]).unwrap());
    assert_eq!(
        std::fs::read_to_string(new.join("config.toml")).unwrap(),
        "new-content"
    );
}

/// 一次性迁移：新目录空且旧目录不存在 → 不迁移（正常全新安装）
#[test]
fn test_migrate_global_config_skips_when_legacy_missing() {
    let tmp = test_tmp_dir("migrate-legacy-missing");
    let legacy = tmp.join("missing");
    let new = tmp.join("new");
    assert!(!migrate_global_config(&new, &[legacy]).unwrap());
    assert!(!new.exists());
}

/// 一次性迁移：多个候选旧目录按序取第一个有效的
#[test]
fn test_migrate_global_config_uses_first_legacy_with_config() {
    let tmp = test_tmp_dir("migrate-first-legacy");
    let legacy_empty = tmp.join("empty");
    let legacy_real = tmp.join("real");
    let new = tmp.join("new");
    std::fs::create_dir_all(&legacy_empty).unwrap();
    std::fs::create_dir_all(&legacy_real).unwrap();
    std::fs::write(legacy_real.join("config.toml"), "real-content").unwrap();

    assert!(migrate_global_config(&new, &[legacy_empty, legacy_real]).unwrap());
    assert_eq!(
        std::fs::read_to_string(new.join("config.toml")).unwrap(),
        "real-content"
    );
}

/// 测试用唯一临时目录（std 实现——Cargo.toml 无 dev-dependencies；
/// 进程 id + 原子序号防并行测试冲突——v19 教训）
///
/// clippy 在非测试视角下对 cfg(test) 模块内被测试调用的 helper 会
/// 误报 never used（rustc dead_code 以 lib 编译单元分析）；4 个迁移
/// 测试均调用它（cargo test 全绿），非死代码。
#[allow(dead_code)]
fn test_tmp_dir(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "code-repo-wiki-config-test-{}-{}-{}",
        std::process::id(),
        name,
        SEQ.fetch_add(1, Ordering::SeqCst)
    ))
}

/// E 组搜索链：项目级配置存在 → 返回项目级（项目级优先；v24 起为
/// 独立文件 `config.toml`，不再混入产物目录）
#[test]
fn test_resolve_prefers_project_config() {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_e_project_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(PROJECT_CONFIG_FILE), "dummy").unwrap();
    let global_dir = dir.join("global");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::write(global_dir.join(USER_CONFIG_FILE), "dummy-global").unwrap();

    let resolved =
        resolve_default_config_path_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
    assert_eq!(resolved, dir.join(PROJECT_CONFIG_FILE));

    let _ = std::fs::remove_dir_all(&dir);
}

/// v25：三链加载——项目级 config.toml 存在时，以用户级
/// config.toml（缺则模板）为基，字段级合并覆盖；
/// v30：项目级 llm/embed 键（base_url/api_key_env）完整覆盖用户级值
#[test]
fn test_load_default_config_project_overrides_user() {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_merge_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 用户级：模板 + 自定义 model（v30：scope/output 等段已硬编码，模板即全量默认）
    let global_dir = dir.join("global");
    std::fs::create_dir_all(&global_dir).unwrap();
    let user_text = include_str!("../../config.toml")
        .replace("model = \"deepseek-v4-flash\"", "model = \"user-model\"");
    std::fs::write(global_dir.join(USER_CONFIG_FILE), &user_text).unwrap();

    // 项目级：写 model + api_key_env 覆盖
    std::fs::write(
        dir.join(PROJECT_CONFIG_FILE),
        r#"
[llm]
provider = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-test"
"#,
    )
    .unwrap();

    let (path, config) =
        load_default_config_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
    // 项目级路径胜出（返回项目级文件位置）
    assert_eq!(path, dir.join(PROJECT_CONFIG_FILE));
    // model 字段级覆盖生效
    assert_eq!(config.llm.model, "claude-test");
    // v30：项目级 provider/api_key_env 完整覆盖用户级（不再净化剥离）
    assert_eq!(config.llm.provider, schema::LlmProviderType::Anthropic);
    assert_eq!(config.llm.api_key_env, "ANTHROPIC_API_KEY");

    let _ = std::fs::remove_dir_all(&dir);
}

/// v25：无项目级配置时，用户级存在则直接用（无合并无净化）；
/// 用户级缺失时创建（模板），绝不自动创建项目级文件。
#[test]
fn test_load_default_config_user_only_or_creates() {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_useronly_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // 用户级存在：直接使用
    let global_dir = dir.join("global");
    std::fs::create_dir_all(&global_dir).unwrap();
    let user_text = include_str!("../../config.toml").replace(
        "model = \"deepseek-v4-flash\"",
        "model = \"user-only-model\"",
    );
    std::fs::write(global_dir.join(USER_CONFIG_FILE), &user_text).unwrap();
    let (path, config) =
        load_default_config_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
    assert_eq!(path, global_dir.join(USER_CONFIG_FILE));
    assert_eq!(config.llm.model, "user-only-model");
    // 项目级文件未被创建
    assert!(!dir.join(PROJECT_CONFIG_FILE).exists());

    // 用户级缺失：创建模板；项目级仍不创建
    let global2 = dir.join("global2");
    let (path2, config2) =
        load_default_config_with(&ProjectRoot::new(dir.clone()), &global2).unwrap();
    assert!(path2.ends_with(USER_CONFIG_FILE));
    assert!(global2.join(USER_CONFIG_FILE).exists());
    assert_eq!(config2.llm.model, "deepseek-v4-flash");
    assert!(!dir.join(PROJECT_CONFIG_FILE).exists());

    let _ = std::fs::remove_dir_all(&dir);
}

/// v30：项目级配置文件加载时 base_url/api_key_env 完整生效——
/// 净化/注入规则已整体删除（端点/变量名非密钥明文，项目级可用
/// 配置即写即用）；缺失字段由 schema serde 默认兜底
#[test]
fn test_load_project_config_keeps_sensitive_keys() {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_projcfg_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(PROJECT_CONFIG_FILE);
    // 项目级配置声明项目契约（语言）+ 完整端点与变量名
    std::fs::write(
        &path,
        r#"
[wiki]
language = "en"

[llm]
provider = "anthropic"
model = "claude-opus"
base_url = "https://custom.example.com/v1"
api_key_env = "HACKED_KEY"
"#,
    )
    .unwrap();

    let config = load_config(&path).unwrap();
    // 项目级覆盖值全部保留（v30：不再剥离）
    assert_eq!(
        config.llm.provider,
        crate::config::schema::LlmProviderType::Anthropic
    );
    assert_eq!(config.llm.model, "claude-opus");
    assert_eq!(
        config.llm.base_url.as_deref(),
        Some("https://custom.example.com/v1")
    );
    assert_eq!(config.llm.api_key_env, "HACKED_KEY");
    // 项目契约保留
    assert_eq!(config.wiki.language, "en");
    // v30: output.dir 已硬编码，项目级不可写

    let _ = std::fs::remove_dir_all(&dir);
}

/// v30：缺键由 schema serde 默认兜底——项目级配置省略
/// base_url/api_key_env 等字段仍可加载（使用默认可用阵营）
#[test]
fn test_load_project_config_defaults_for_missing_keys() {
    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_projcfg_defaults_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(PROJECT_CONFIG_FILE);
    std::fs::write(
        &path,
        r#"
[llm]
provider = "mock"
"#,
    )
    .unwrap();

    let config = load_config(&path).unwrap();
    assert_eq!(
        config.llm.provider,
        crate::config::schema::LlmProviderType::Mock
    );
    // 缺失字段由 schema 字段级 serde 默认兜底（v29 可用阵营）
    assert_eq!(
        config.llm.base_url.as_deref(),
        Some("https://opencode.ai/zen/go/v1")
    );
    assert_eq!(config.llm.api_key_env, "OPENCODEGO2_API_KEY");
    assert_eq!(config.embed.model, "qwen3.7-text-embedding");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 任意文件名（显式 --config）均原样加载（v30：净化/注入已整体删除，
/// 文件名不再有语义差异）；缺失字段同样由 schema serde 默认兜底
#[test]
fn test_load_explicit_config_keeps_sensitive_keys() {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_anyname_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("my.toml");
    std::fs::write(
        &path,
        r#"
[llm]
provider = "anthropic"
model = "claude-opus"
api_key_env = "ANTHROPIC_API_KEY"
"#,
    )
    .unwrap();

    let config = load_config(&path).unwrap();
    // 用户级/显式配置完整保留敏感键
    assert_eq!(
        config.llm.provider,
        crate::config::schema::LlmProviderType::Anthropic
    );
    assert_eq!(config.llm.model, "claude-opus");

    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn test_resolve_falls_back_to_global() {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_e_global_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let global_dir = dir.join("global");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::write(global_dir.join(USER_CONFIG_FILE), "dummy-global").unwrap();

    let resolved =
        resolve_default_config_path_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
    assert_eq!(resolved, global_dir.join(USER_CONFIG_FILE));

    let _ = std::fs::remove_dir_all(&dir);
}

/// E 组搜索链：项目级与全局都缺失 → 创建全局目录 + 默认配置，返回全局路径
#[test]
fn test_resolve_creates_global_config_when_missing() {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_e_create_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let global_dir = dir.join("global");

    let resolved =
        resolve_default_config_path_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
    assert_eq!(resolved, global_dir.join(USER_CONFIG_FILE));
    assert!(
        global_dir.join(USER_CONFIG_FILE).exists(),
        "缺失时应创建全局默认配置"
    );
    // 创建的配置必须可加载（模板完整）
    assert!(load_config(&resolved).is_ok());

    // 幂等：再次解析仍返回同一路径，不重复创建
    let resolved2 =
        resolve_default_config_path_with(&ProjectRoot::new(dir.clone()), &global_dir).unwrap();
    assert_eq!(resolved2, resolved);

    let _ = std::fs::remove_dir_all(&dir);
}

/// resolve_config_path：显式指定原样返回（不触发创建）
#[test]
fn test_resolve_config_path_explicit_wins() {
    let dir =
        std::env::temp_dir().join(format!("code_repo_wiki_e_explicit_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let explicit = dir.join("custom.toml");
    let resolved = resolve_config_path(Some(&explicit), &ProjectRoot::new(dir.clone())).unwrap();
    assert_eq!(resolved, explicit);
    // 显式指定不创建全局目录/文件
    assert!(!dir.join("global").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

// ============ A7.6 audit-cfg-03/04/05：validate_config 解析期校验 ============

/// 写入配置文件并 load_config，断言返回 Err 且错误消息含关键词
/// （仅测试调用；cfg(test) 隔离避免非测试构建 dead_code 告警）
#[cfg(test)]
fn assert_config_rejected(tag: &str, text: &str, needle: &str) {
    let dir = std::env::temp_dir().join(format!(
        "code_repo_wiki_cfg_valid_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    let err = load_config(&path).unwrap_err().to_string();
    assert!(
        err.contains(needle),
        "应拒绝非法配置（{needle}），实际: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// audit-cfg-04：wiki.language 含路径分隔符/点段 → 解析期拒绝
#[test]
fn test_validate_rejects_language_with_path_chars() {
    for bad in ["zh/../en", "zh en", "..", "C:zh"] {
        assert_config_rejected(
            "lang",
            &format!("[wiki]\nlanguage = \"{bad}\"\n"),
            "wiki.language",
        );
    }
    // 反斜杠在 TOML 基本字符串需转义（\e 非法、解析层就拒），用字面量
    // 字符串形态注入，验证 charset 校验本身拒绝反斜杠（Windows 路径分隔符）
    assert_config_rejected("lang_bs", "[wiki]\nlanguage = 'zh\\en'\n", "wiki.language");
}

/// audit-cfg-05：llm.reasoning_effort 非白名单值 → 解析期拒绝
#[test]
fn test_validate_rejects_invalid_reasoning_effort() {
    assert_config_rejected(
        "effort",
        "[llm]\nreasoning_effort = \"banana\"\n",
        "reasoning_effort",
    );
}

/// audit-cfg-03：max_concurrency=0 → 解析期拒绝（Semaphore 永久挂起前拦截）
#[test]
fn test_validate_rejects_zero_max_concurrency() {
    assert_config_rejected("llm_mc", "[llm]\nmax_concurrency = 0\n", "max_concurrency");
    assert_config_rejected(
        "embed_mc",
        "[embed]\nmax_concurrency = 0\n",
        "max_concurrency",
    );
}

/// audit-cfg-03：model / base_url 显式写空 → 解析期拒绝
#[test]
fn test_validate_rejects_empty_model_and_base_url() {
    assert_config_rejected("empty_model", "[llm]\nmodel = \"\"\n", "llm.model");
    assert_config_rejected("empty_base", "[llm]\nbase_url = \"\"\n", "llm.base_url");
    assert_config_rejected("empty_embed", "[embed]\nmodel = \"\"\n", "embed.model");
}

/// audit-cfg-03：合法配置（含默认语言 zh）不被误拒
#[test]
fn test_validate_accepts_valid_configs() {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_cfg_ok_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    // 最小合法配置：缺 [wiki] → 默认 zh；缺 model/base_url → schema 默认填充
    std::fs::write(&path, "[llm]\nprovider = \"mock\"\n").unwrap();
    assert!(load_config(&path).is_ok(), "最小合法配置应通过校验");
    // 合法 reasoning_effort 值域内 + 正并发
    std::fs::write(
        &path,
        "[llm]\nprovider = \"mock\"\nreasoning_effort = \"high\"\nmax_concurrency = 8\n",
    )
    .unwrap();
    assert!(load_config(&path).is_ok(), "合法值域配置应通过校验");
    let _ = std::fs::remove_dir_all(&dir);
}

/// audit-cfg-02：create_default_config 创建的用户级 config 在 Unix 下
/// 权限收紧（文件 0600 + 目录 0700），key 写入前窗口期即受保护
#[test]
fn test_create_default_config_sets_private_permissions() {
    let dir = std::env::temp_dir().join(format!("code_repo_wiki_cfg_perm_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config_path = dir.join("config.toml");
    create_default_config(&config_path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&config_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "用户级配置应 0600"
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "用户级目录应 0700"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
