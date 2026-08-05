//! doctor：环境诊断命令（v17 t08）
//!
//! 对运行环境做六查并输出逐项结果，定位"配置加载失败/产物目录不可写/
//! Key 缺失/网络不可达/版本漂移"类问题，给出可操作提示。语义（wayfinder-v17 t08
//! 拍板）：全过退出码 0，任一失败退出码 1（与 lint 三态同族，供 CI/脚本
//! 门禁使用）。网络检查只覆盖 LLM base_url（mock provider 跳过——本地
//! 模拟不触网，网络检查无意义）。

use anyhow::Result;
use std::path::Path;

use crate::config::schema::LlmProviderType;
use crate::config::{load_config, resolve_config_path};
use crate::incremental::state::GenerationState;
use crate::project::ProjectRoot;

/// 单项检查结果
#[derive(Debug)]
pub struct CheckResult {
    /// 检查项名称（终端输出用）
    pub name: &'static str,
    /// 是否通过
    pub ok: bool,
    /// 附加说明（失败原因 / 通过时的补充信息）
    pub detail: Option<String>,
}

/// 六查诊断：配置可解析 → 产物目录可写 → 输出目录状态 → LLM Key →
/// 网络连通性 → 版本漂移。任何一项失败不中断后续检查（一次跑完给出
/// 全景，排障友好）。
///
/// 注意：配置加载失败时无法获得输出目录/Key/网络信息，只返回第一项
/// 失败结果（避免在无效配置上继续猜测）。
pub fn run(config_path: &Path, root: &ProjectRoot) -> Result<Vec<CheckResult>> {
    let mut checks = Vec::new();

    // 1. 配置存在且可解析（走 resolve_config_path 与主流程一致；
    // 失败即终止——后续检查都依赖配置）
    let config = match resolve_config_path(Some(config_path), root).and_then(|p| load_config(&p)) {
        Ok(mut c) => {
            // output.dir 相对路径统一解析到 root（与 load_config_rooted 同语义，
            // doctor 独立于 CLI 层无法复用该入口，此处保持一致性）
            let output_dir = Path::new(&c.output.dir);
            if output_dir.is_relative() {
                c.output.dir = root.path().join(output_dir).to_string_lossy().into_owned();
            }
            checks.push(CheckResult {
                name: "配置",
                ok: true,
                detail: Some(format!("加载成功: {}", config_path.display())),
            });
            c
        }
        Err(e) => {
            checks.push(CheckResult {
                name: "配置",
                ok: false,
                detail: Some(format!("加载失败: {:#}", e)),
            });
            return Ok(checks);
        }
    };

    // 2. 产物目录可写（建目录 + 写探针文件删除；不可写时生成必失败，
    // 提前暴露比生成中途报错友好）
    let output_dir = Path::new(&config.output.dir);
    let probe = output_dir.join(".doctor-write-probe");
    let writable = std::fs::create_dir_all(output_dir)
        .and_then(|_| std::fs::write(&probe, b"probe"))
        .and_then(|_| std::fs::remove_file(&probe));
    checks.push(CheckResult {
        name: "产物目录可写",
        ok: writable.is_ok(),
        detail: Some(match writable {
            Ok(()) => format!("{} 可写", output_dir.display()),
            Err(e) => format!("{} 不可写: {}", output_dir.display(), e),
        }),
    });

    // 3. 输出目录状态（已存在非空产物 → 提示将增量更新/可能覆盖；
    // 空目录/不存在 → 全新生成提示）
    let has_existing = output_dir.exists()
        && std::fs::read_dir(output_dir).map(|mut d| d.next().is_some()).unwrap_or(false);
    checks.push(CheckResult {
        name: "输出目录",
        ok: true,
        detail: Some(if has_existing {
            format!("{} 已存在历史产物，生成将按指纹增量更新（人工修改受保护）", output_dir.display())
        } else {
            format!("{} 为空或不存在，将全新生成", output_dir.display())
        }),
    });

    // 4. LLM Key（api_key 字段优先，其次 api_key_env 环境变量；
    // 空字符串视为未配置——空 api_key 是常见误配置）
    let key_ok = config
        .llm
        .api_key
        .as_ref()
        .map(|k| !k.is_empty())
        .unwrap_or(false)
        || (!config.llm.api_key_env.is_empty()
            && std::env::var_os(&config.llm.api_key_env).is_some());
    checks.push(CheckResult {
        name: "LLM Key",
        ok: key_ok,
        detail: Some(if key_ok {
            if config.llm.api_key.is_some() {
                "已配置 api_key（配置文件）".to_string()
            } else {
                format!("环境变量 {} 已设置", config.llm.api_key_env)
            }
        } else if config.llm.api_key_env.is_empty() {
            "未配置 api_key 且 api_key_env 为空（mock provider 除外）".to_string()
        } else {
            format!(
                "未设置：api_key 为空且环境变量 {} 未定义。请设置 {} 或编辑配置文件的 [llm] 段",
                config.llm.api_key_env, config.llm.api_key_env
            )
        }),
    });

    // 5. 网络连通性（mock 跳过——本地模拟不触网；其余查 base_url 5s 超时）
    if config.llm.provider == LlmProviderType::Mock {
        checks.push(CheckResult {
            name: "网络",
            ok: true,
            detail: Some("mock provider：跳过网络检查（本地模拟）".to_string()),
        });
    } else {
        let base_url = config
            .llm
            .base_url
            .clone()
            .unwrap_or_else(|| match config.llm.provider {
                LlmProviderType::Anthropic => "https://api.anthropic.com/v1".to_string(),
                _ => "https://api.openai.com/v1".to_string(),
            });
        // 5s 超时探活（GET 根路径；服务端 4xx/5xx 也算可达——连通性
        // 检查只关心网络层通不通，业务错误由实际调用暴露）。
        // reqwest 未启用 blocking feature（与项目异步客户端一致），
        // 用 tokio runtime 单次 block_on 包装
        let reachable = tokio::runtime::Runtime::new()
            .map(|rt| {
                rt.block_on(async {
                    match reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(5))
                        .build()
                    {
                        Ok(client) => match client.get(&base_url).send().await {
                            Ok(resp) => resp.status().as_u16() < 600,
                            Err(_) => false,
                        },
                        Err(_) => false,
                    }
                })
            })
            .unwrap_or(false);
        checks.push(CheckResult {
            name: "网络",
            ok: reachable,
            detail: Some(if reachable {
                format!("{} 可达", base_url)
            } else {
                format!("{} 不可达（5s 超时/拒绝连接）。检查网络或 base_url 配置", base_url)
            }),
        });
    }

    // 6. 版本自检（v19 t01）：产物由哪个工具版本生成，与当前二进制是否一致。
    // 捕获 PATH 旧版二进制静默漂移（旧版缺 doctor/dry-run，调用报
    // unrecognized subcommand exit 2，用户无从知道产物是旧格式）。
    // 无状态文件 → 尚未生成，提示首次生成（恒通过，不算失败）。
    // 状态缺 tool_version 字段（旧版本生成的状态文件）→ 无法判断，提示
    // 建议重新全量生成（恒通过，不阻断——漂移是告警级问题）。
    let state_path = output_dir.join(".state").join("generation_state.json");
    let version_check = if !state_path.exists() {
        CheckResult {
            name: "版本",
            ok: true,
            detail: Some("尚无生成状态（首次生成将记录工具版本）".to_string()),
        }
    } else {
        let current = env!("CARGO_PKG_VERSION");
        match GenerationState::load(&output_dir.join(".state")) {
            Ok(state) => match state.tool_version {
                Some(recorded) if recorded == current => CheckResult {
                    name: "版本",
                    ok: true,
                    detail: Some(format!("产物由当前版本 {} 生成", current)),
                },
                Some(recorded) => CheckResult {
                    name: "版本",
                    ok: true,
                    detail: Some(format!(
                        "产物由 v{} 生成，当前二进制 v{}——建议运行一次完整 generate 升级产物",
                        recorded, current
                    )),
                },
                None => CheckResult {
                    name: "版本",
                    ok: true,
                    detail: Some("产物由旧版本生成（状态无版本记录），建议运行一次完整 generate".to_string()),
                },
            },
            Err(e) => CheckResult {
                name: "版本",
                ok: true,
                detail: Some(format!("状态文件读取失败（不阻断）: {}", e)),
            },
        }
    };
    checks.push(version_check);

    Ok(checks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 构造临时目录内的最小 mock 配置（可追加覆盖段）
    fn temp_config(tag: &str, extra: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("repo_wiki_doctor_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                r#"[scope]
include = ["src/**"]
exclude = []

[output]
dir = ".repo-wiki"

[llm]
provider = "mock"
model = "mock-model"
api_key = "mock"
api_key_env = ""
max_concurrent = 1
{extra}"#
            ),
        )
        .unwrap();
        (dir, config)
    }

    #[test]
    fn test_doctor_all_pass_with_mock() {
        let (dir, config) = temp_config("pass", "");
        let root = ProjectRoot::new(dir.clone());
        let checks = run(&config, &root).unwrap();
        assert_eq!(checks.len(), 6, "应恰好六项检查: {:?}", checks);
        for c in &checks {
            assert!(c.ok, "{} 应通过: {:?}", c.name, c.detail);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_doctor_config_missing_fails_first_check_only() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_doctor_missing_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = ProjectRoot::new(dir.clone());
        let checks = run(&dir.join("nope.toml"), &root).unwrap();
        assert_eq!(checks.len(), 1, "配置失败应只返回配置检查: {:?}", checks);
        assert!(!checks[0].ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_doctor_key_missing_reports_actionable_guidance() {
        let (dir, config) = temp_config("key", "");
        let root = ProjectRoot::new(dir.clone());
        // api_key 置空（空字符串视为未配置）+ env 未设置
        std::fs::write(
            &config,
            std::fs::read_to_string(&config)
                .unwrap()
                .replace("api_key = \"mock\"", "api_key = \"\""),
        )
        .unwrap();
        let checks = run(&config, &root).unwrap();
        let key = checks.iter().find(|c| c.name == "LLM Key").expect("应有 Key 检查");
        assert!(!key.ok);
        let detail = key.detail.clone().unwrap();
        assert!(detail.contains("api_key_env"), "应给出可操作引导: {detail}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_doctor_network_unreachable_fails() {
        // base_url 指向未监听端口 → 不可达；同段内改写 provider 与追加 base_url
        // （追加第二个 [llm] 段会因重复表头解析失败，测试须改写原段）
        let (dir, config) = temp_config("net", "");
        let cfg_text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            &config,
            cfg_text
                .replace("provider = \"mock\"", "provider = \"openai-compatible\"")
                + "base_url = \"http://127.0.0.1:1/v1\"\n",
        )
        .unwrap();
        let root = ProjectRoot::new(dir.clone());
        let checks = run(&config, &root).unwrap();
        let net = checks.iter().find(|c| c.name == "网络").expect("应有网络检查");
        assert!(!net.ok, "未监听端口应报不可达: {:?}", net.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v19 t01：状态无版本记录（旧版产物）→ 版本检查恒通过但提示漂移；
    /// 版本一致 → 通过且说明由当前版本生成
    #[test]
    fn test_doctor_version_reports_drift() {
        let (dir, config) = temp_config("ver", "");
        let root = ProjectRoot::new(dir.clone());
        let checks = run(&config, &root).unwrap();
        let ver = checks.iter().find(|c| c.name == "版本").expect("应有版本检查");
        assert!(ver.ok, "无状态文件时应通过: {:?}", ver.detail);
        assert!(
            ver.detail.clone().unwrap().contains("尚无生成状态"),
            "应提示首次生成: {:?}",
            ver.detail
        );

        // 写入旧版本状态（tool_version=0.0.0）→ 提示漂移
        std::fs::create_dir_all(dir.join(".repo-wiki/.state")).unwrap();
        std::fs::write(
            dir.join(".repo-wiki/.state/generation_state.json"),
            r#"{"last_commit_hash":null,"file_fingerprints":{},"generated_at":"2025-01-01T00:00:00Z","tool_version":"0.0.0"}"#,
        )
        .unwrap();
        let checks = run(&config, &root).unwrap();
        let ver = checks.iter().find(|c| c.name == "版本").expect("应有版本检查");
        assert!(ver.ok);
        let detail = ver.detail.clone().unwrap();
        assert!(detail.contains("0.0.0"), "应报告记录版本: {detail}");
        assert!(detail.contains("升级产物"), "应建议升级产物: {detail}");

        // 写入当前版本状态 → 一致通过
        let current = env!("CARGO_PKG_VERSION");
        std::fs::write(
            dir.join(".repo-wiki/.state/generation_state.json"),
            format!(
                r#"{{"last_commit_hash":null,"file_fingerprints":{{}},"generated_at":"2025-01-01T00:00:00Z","tool_version":"{current}"}}"#
            ),
        )
        .unwrap();
        let checks = run(&config, &root).unwrap();
        let ver = checks.iter().find(|c| c.name == "版本").expect("应有版本检查");
        assert!(ver.ok);
        assert!(
            ver.detail.clone().unwrap().contains("由当前版本"),
            "版本一致应通过: {:?}",
            ver.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
