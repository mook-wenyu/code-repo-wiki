//! key：LLM API key 交互式配置命令
//!
//! 安全边界（用户拍板）：明文 api_key 只写入**用户级**配置
//! `config.toml`（`%APPDATA%/code-repo-wiki/` 或 `$HOME/code-repo-wiki/`，
//! 见 [`crate::config::global_config_dir`]），**绝不写项目级** `config.toml`
//! ——项目级随 Git 共享，明文凭据写入即泄露。`--env` 模式不落明文，
//! 改写入建议的环境变量名引用（`api_key_env` 是既有机制，见
//! [`crate::config::schema::LlmSection`]，api_key 字段优先于 env 读取）。

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::schema::LlmProviderType;
use crate::config::{
    create_default_config, load_config, load_default_config_with, USER_CONFIG_FILE,
};
use crate::project::ProjectRoot;

/// 生产入口：真实 stdin + 真实 TTY 检测
///
/// stdin 交互与 TTY 判定抽为注入点（`run_with_io`）：`IsTerminal`
/// 无法在测试中伪造，测试注入固定输入与 is_tty 值。
pub fn run(env: bool, config_path: Option<&Path>, root: &ProjectRoot) -> Result<()> {
    run_with_io(
        env,
        config_path,
        root,
        &crate::config::global_config_dir()?,
        std::io::stdin().is_terminal(),
        &mut read_stdin_line,
    )
}

/// 读取 stdin 一行（生产输入源；测试用固定输入闭包替代）
fn read_stdin_line() -> io::Result<String> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line)
}

/// 注入版流程（global_dir / is_tty / read_line 可注入，测试不碰真实
/// APPDATA 与真实键盘输入）
fn run_with_io(
    env: bool,
    config_path: Option<&Path>,
    root: &ProjectRoot,
    global_dir: &Path,
    is_tty: bool,
    read_line: &mut dyn FnMut() -> io::Result<String>,
) -> Result<()> {
    // ① 目标文件固定为用户级配置；缺失时写内置模板（复用 config 模块
    // 现成函数：模板含注释与生产默认值）
    let target = global_dir.join(USER_CONFIG_FILE);
    if !target.exists() {
        create_default_config(&target)?;
    }

    // ② provider 判定：--config 显式时用显式文件（如项目级
    // provider=mock 时提示无需 key）；缺省走默认配置链（项目级字段级
    // 合并覆盖用户级，与主流程同源）。写入目标不受影响，恒为用户级。
    let provider_cfg = match config_path {
        Some(p) => load_config(p)?,
        None => load_default_config_with(root, global_dir)?.1,
    };
    if provider_cfg.llm.provider == LlmProviderType::Mock {
        println!("mock provider 无需 API key（本地模拟，不触网）");
        return Ok(());
    }

    // ③ 环境变量已设置：已配置完成，直接报告退出（env 名取配置中声明的
    // api_key_env）
    let env_name = &provider_cfg.llm.api_key_env;
    if std::env::var(env_name).is_ok() {
        println!("已通过环境变量 {} 配置，无需重复设置", env_name);
        return Ok(());
    }

    // ④ --env 模式：不落明文，写入按 provider 建议的环境变量名引用
    if env {
        let suggested = suggested_env_name(&provider_cfg.llm.provider);
        write_field(&target, "api_key_env", suggested)?;
        println!(
            "已写入环境变量引用 api_key_env = \"{suggested}\"（用户级配置，不随 Git 共享）"
        );
        println!(
            "请设置环境变量 {suggested}（如 export {suggested}=sk-... 或 setx {suggested} sk-...），重启终端后生效"
        );
        return Ok(());
    }

    // ⑤ 非 TTY（管道/CI/外部 Agent）：无法交互，打印引导退出 0
    if !is_tty {
        println!("{}", guidance_text());
        return Ok(());
    }

    // ⑥ 交互输入：读一行，trim 后空输入视为取消
    print!("请输入 API key（直接回车取消）: ");
    io::stdout().flush()?;
    let line = read_line()?;
    let key = line.trim();
    if key.is_empty() {
        println!("未输入内容，取消");
        return Ok(());
    }

    // ⑦ 写入明文 + 写后重新解析验证字段生效
    write_field(&target, "api_key", key)?;
    println!(
        "已写入 {} 的 [llm] api_key（用户级配置，不随 Git 共享）",
        target.display()
    );
    Ok(())
}

/// --env 模式的建议环境变量名（用户拍板：openai→DEEPSEEK_API_KEY、
/// anthropic→ANTHROPIC_API_KEY；openai-compatible 归入 openai 阵营——
/// 默认阵营统一 DeepSeek 模板，见 schema::LlmSection Default）
fn suggested_env_name(provider: &LlmProviderType) -> &'static str {
    match provider {
        LlmProviderType::Anthropic => "ANTHROPIC_API_KEY",
        _ => "DEEPSEEK_API_KEY",
    }
}

/// 非 TTY 引导文本（独立纯函数供测试断言；doctor 同风格引导）
pub(crate) fn guidance_text() -> String {
    [
        "当前环境非交互式终端（管道/CI/外部 Agent），无法读取键盘输入。可用方式：",
        "  1. 在交互式终端运行 `code-repo-wiki key` 直接输入明文 API key（写入用户级 config.toml，不随 Git 共享）",
        "  2. 运行 `code-repo-wiki key --env` 改用环境变量引用（不落明文，key 由 shell 环境提供）",
    ]
    .join("\n")
}

/// 写字段到用户级配置文件并重新解析验证生效（验证失败报错退出 1）
fn write_field(target: &Path, field: &str, value: &str) -> Result<()> {
    let text = std::fs::read_to_string(target)
        .with_context(|| format!("读取配置失败: {}", target.display()))?;
    let updated = set_llm_field(&text, field, value)?;
    crate::fs::write_file_atomic(target, &updated)?;
    // 写后重新解析验证：字段必须真实生效（TOML 转义错误等在此暴露；
    // 用户级文件原样加载，无注入无净化——v30 已整体删除）
    let cfg = load_config(target)
        .with_context(|| format!("写入后配置解析失败: {}", target.display()))?;
    let effective = if field == "api_key" {
        cfg.llm.api_key.as_deref() == Some(value)
    } else {
        cfg.llm.api_key_env == value
    };
    if !effective {
        anyhow::bail!("验证失败：{field} 写入后未生效，请检查 {}", target.display());
    }
    Ok(())
}

/// 在 TOML 文本的 [llm] 段设置字段值
///
/// 行替换优先（保留模板注释）；分支顺序：非注释字段行 → 注释占位行
/// （模板的 `#api_key = ""`）→ 段末追加（模板可能没有该字段行）。
/// 无 [llm] 段时回退 toml::Value 往返（丢注释，仅兜底路径）。
fn set_llm_field(text: &str, field: &str, value: &str) -> Result<String> {
    let escaped = escape_toml_string(value);
    let mut lines: Vec<String> = text.lines().map(String::from).collect();

    let Some(llm_idx) = lines.iter().position(|l| l.trim() == "[llm]") else {
        // 回退：toml::Value 往返（无 [llm] 段时唯一可靠手段；丢注释
        // 可接受——兜底路径只发生在畸形配置）
        let mut doc: toml::Value =
            toml::from_str(text).with_context(|| "解析配置文本失败".to_string())?;
        let llm = doc
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("配置根不是表"))?
            .entry("llm")
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("[llm] 不是表"))?;
        llm.insert(field.to_string(), toml::Value::String(value.to_string()));
        let out = toml::to_string(&doc).with_context(|| "配置序列化失败".to_string())?;
        return Ok(out);
    };

    // 段范围：[llm] 之后到下一个段头（`[x]` / `[[x]]`，行首表头特征），
    // 无后续段时到文件末尾
    let end = lines
        .iter()
        .enumerate()
        .skip(llm_idx + 1)
        .find(|(_, l)| {
            let t = l.trim();
            t.starts_with('[') && t.ends_with(']')
        })
        .map(|(i, _)| i)
        .unwrap_or(lines.len());
    let seg = &lines[llm_idx + 1..end];

    // 字段行匹配：field 前缀后必须紧跟 '='（防 api_key 误配 api_key_env）
    let matches_field = |l: &str| -> bool {
        l.trim_start()
            .strip_prefix(field)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    // 注释占位行匹配：`#api_key = ""`（模板占位，替换时取消注释）
    let matches_comment_field = |l: &str| -> bool {
        l.trim_start().strip_prefix('#').is_some_and(matches_field)
    };

    // 分支 A：非注释字段行 → 整行替换（保留原缩进）
    if let Some((off, line)) = seg.iter().enumerate().find(|(_, l)| matches_field(l)) {
        let indent = line[..line.len() - line.trim_start().len()].to_string();
        lines[llm_idx + 1 + off] = format!("{indent}{field} = \"{escaped}\"");
        return Ok(join_preserving_newline(&lines, text));
    }
    // 分支 B：注释占位行 → 替换为生效字段（取消注释）
    if let Some((off, line)) = seg
        .iter()
        .enumerate()
        .find(|(_, l)| matches_comment_field(l))
    {
        let indent = line[..line.len() - line.trim_start().len()].to_string();
        lines[llm_idx + 1 + off] = format!("{indent}{field} = \"{escaped}\"");
        return Ok(join_preserving_newline(&lines, text));
    }
    // 分支 C：段内最后非空行之后追加（模板可能没有该字段行）
    let insert_at = match seg.iter().rposition(|l| !l.trim().is_empty()) {
        Some(rel) => llm_idx + 1 + rel + 1,
        None => llm_idx + 1, // 段内全空：紧跟段头
    };
    lines.insert(insert_at, format!("{field} = \"{escaped}\""));
    Ok(join_preserving_newline(&lines, text))
}

/// 重建文本并保留原文件尾换行（`lines()` 丢失换行符信息）
fn join_preserving_newline(lines: &[String], original: &str) -> String {
    let mut out = lines.join("\n");
    if original.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// TOML 基本字符串转义（key 中可能含引号/反斜杠；不转义会导致 TOML
/// 解析失败——写后验证虽会兜底报错，显式转义让常见 key 一次成功）
fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 构造临时用户级目录 + 模板变体：api_key_env 指向一个不可能存在的
    /// 环境变量名（规避测试机真实设置 DEEPSEEK_API_KEY/OPENCODEGO2_API_KEY
    /// 等触发③"已配置"早退分支；v29 起模板阵营为 opencode 网关，
    /// 匹配值随模板同源，避免替换落空），provider 可换（测 --env 建议名区分度）
    ///
    /// 临时目录必须位于真实全局配置目录之内：v30 起用户级配置文件名统一
    /// 为 config.toml 且原样加载（无注入无净化，字段缺失由 schema 默认
    /// 兜底）——置于全局目录内语义自洽（用户级信任源），测试目录独立
    /// 命名（pid）用完即删，不触碰真实配置文件本体。
    fn temp_global(tag: &str, provider: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_key_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 用户级配置目录测试注入：直接用临时目录（不解析进程级环境变量——
        // 并行测试下 global_config_dir() 读 HOME/APPDATA 与其他测试的
        // set_var/remove_var 竞态，ubuntu 无 APPDATA 兜底时必现；路径解析
        // 本身由 config/mod.rs 的纯函数单测覆盖，这里只验证写盘行为）
        let global_dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_key_global_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&global_dir);
        std::fs::create_dir_all(&global_dir).unwrap();
        let user_text = include_str!("../config.toml")
            .lines()
            .map(|l| {
                if l.starts_with("provider = ") {
                    format!("provider = \"{provider}\"")
                } else if l.contains("api_key_env = \"OPENCODEGO2_API_KEY\"") {
                    "api_key_env = \"REPO_WIKI_TEST_ENV_NONE\"".to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(global_dir.join(USER_CONFIG_FILE), &user_text).unwrap();
        (dir, global_dir)
    }

    /// 交互模式：模拟 stdin 输入明文 key → 写入用户级文件且 load_config 读回
    #[test]
    fn test_key_writes_plain_api_key_to_user_config() {
        let (dir, global_dir) = temp_global("plain", "openai");
        let root = ProjectRoot::new(dir.clone());
        let mut input = || Ok("sk-test-123".to_string());
        run_with_io(false, None, &root, &global_dir, true, &mut input).unwrap();

        let target = global_dir.join(USER_CONFIG_FILE);
        let text = std::fs::read_to_string(&target).unwrap();
        // 模板的 #api_key = "" 注释占位被替换为明文
        assert!(text.contains("api_key = \"sk-test-123\""), "未写入明文: {text}");
        // 写后重新解析验证生效
        let cfg = load_config(&target).unwrap();
        assert_eq!(cfg.llm.api_key.as_deref(), Some("sk-test-123"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// --env 模式：不落明文，写按 provider 建议的环境变量名引用
    #[test]
    fn test_key_env_mode_writes_env_reference() {
        let (dir, global_dir) = temp_global("env", "anthropic");
        let root = ProjectRoot::new(dir.clone());
        // --env 模式无交互，input 不会被调用
        let mut input = || Ok(String::new());
        run_with_io(true, None, &root, &global_dir, false, &mut input).unwrap();

        let target = global_dir.join(USER_CONFIG_FILE);
        let text = std::fs::read_to_string(&target).unwrap();
        // anthropic provider → 建议环境变量名 ANTHROPIC_API_KEY
        assert!(
            text.contains("api_key_env = \"ANTHROPIC_API_KEY\""),
            "--env 应写建议 env 名: {text}"
        );
        // 不落明文（模板的 #api_key = "" 注释占位仍保留）
        let plain_lines = text
            .lines()
            .filter(|l| l.trim_start().starts_with("api_key ="))
            .count();
        assert_eq!(plain_lines, 0, "不应写明文 api_key: {text}");
        let cfg = load_config(&target).unwrap();
        assert_eq!(cfg.llm.api_key_env, "ANTHROPIC_API_KEY");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非 TTY：打印引导退出 0，不写任何字段（stdin 交互分支不直接测——
    /// IsTerminal 无法在测试中伪造，交互行为经注入参数由前两测覆盖）
    #[test]
    fn test_key_no_tty_prints_guidance() {
        let g = guidance_text();
        assert!(g.contains("非交互"), "应说明非交互: {g}");
        assert!(g.contains("--env"), "应引导 --env 模式: {g}");

        let (dir, global_dir) = temp_global("tty", "openai");
        let root = ProjectRoot::new(dir.clone());
        let mut input = || Ok("sk-test-123".to_string());
        run_with_io(false, None, &root, &global_dir, false, &mut input).unwrap();
        let text = std::fs::read_to_string(global_dir.join(USER_CONFIG_FILE)).unwrap();
        assert!(!text.contains("sk-test-123"), "非 TTY 不应写入: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 段末追加分支：无 api_key 行也无注释占位时在 [llm] 段末追加
    #[test]
    fn test_set_llm_field_appends_when_no_placeholder() {
        let text = "[llm]\nprovider = \"openai\"\napi_key_env = \"X\"\n\n[embed]\nmodel = \"m\"\n";
        let out = set_llm_field(text, "api_key", "sk-abc").unwrap();
        assert!(
            out.contains("api_key_env = \"X\"\napi_key = \"sk-abc\""),
            "应追加在段末: {out}"
        );
        assert!(out.contains("[embed]"), "应保留后续段: {out}");
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["llm"]["api_key"].as_str(), Some("sk-abc"));
        assert_eq!(v["embed"]["model"].as_str(), Some("m"));
    }

    /// 转义分支：key 含引号/反斜杠时 TOML 往返仍可解析回原值
    #[test]
    fn test_set_llm_field_escapes_quotes_and_backslashes() {
        let text = "[llm]\nprovider = \"openai\"\n";
        let out = set_llm_field(text, "api_key", "sk-a\"b\\c").unwrap();
        assert!(out.contains("api_key = \"sk-a\\\"b\\\\c\""), "{out}");
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["llm"]["api_key"].as_str(), Some("sk-a\"b\\c"));
    }

    /// 无 [llm] 段时回退 toml::Value 往返（兜底路径）
    #[test]
    fn test_set_llm_field_falls_back_without_llm_section() {
        let text = "[llm]\nprovider = \"mock\"\n";
        let out = set_llm_field(text, "api_key", "sk-1").unwrap();
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["llm"]["api_key"].as_str(), Some("sk-1"));
    }
}
