//! doctor：环境诊断命令（v17 t08）
//!
//! 对运行环境做七查并输出逐项结果，定位"配置加载失败/产物目录不可写/
//! Key 缺失/网络不可达/协议不兼容/版本漂移"类问题，给出可操作提示。
//! 语义（wayfinder-v17 t08 拍板）：全过退出码 0，任一失败退出码 1（与
//! lint 三态同族，供 CI/脚本门禁使用）。网络与协议检查只覆盖 LLM base_url
//! （mock provider 跳过——本地模拟不触网，网络检查无意义）。

use anyhow::Result;
use std::path::Path;

use crate::config::schema::LlmProviderType;
use crate::config::load_config;
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

/// 协议兼容性探测（v52）：确认配置的 provider 协议形态在 base_url 上
/// 真实存在。
///
/// 发送最小探测请求体（1 token 预算，不解析响应内容），只区分
/// 「协议存在 / 不存在」：2xx/5xx/4xx（401/403/429 等）均视为端点存在
/// （认证/限流/服务端错误由实际调用显式报告），仅 404/405 视为协议
/// 不存在（端点到不了 = 配置形态错误）。网络错误视为不可用。
async fn probe_protocol(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
) -> bool {
    match client.post(url).bearer_auth(api_key).json(body).send().await {
        Ok(resp) => {
            let status = resp.status();
            !(status == reqwest::StatusCode::NOT_FOUND
                || status == reqwest::StatusCode::METHOD_NOT_ALLOWED)
        }
        Err(_) => false,
    }
}

/// 七查诊断：配置可解析 → 产物目录可写 → 输出目录状态 → LLM Key →
/// 网络连通性 → 协议探测 → 版本漂移。任何一项失败不中断后续检查（一次跑完给出
/// 全景，排障友好）。
///
/// 注意：配置加载失败时无法获得输出目录/Key/网络信息，只返回第一项
/// 失败结果（避免在无效配置上继续猜测）。
pub fn run(config_path: Option<&Path>, root: &ProjectRoot) -> Result<Vec<CheckResult>> {
    let mut checks = Vec::new();

    // 1. 配置存在且可解析（None 走默认配置链——项目级字段级合并覆盖
    // 用户级，与主流程 load_config_rooted 同源；失败即终止——后续检查
    // 都依赖配置）
    let resolved = match config_path {
        Some(p) => load_config(p).map(|c| (p.to_path_buf(), c)),
        None => crate::config::load_default_config(root),
    };
    let config = match resolved {
        Ok((path, mut c)) => {
            // 输出目录相对路径统一解析到 root（与 load_config_rooted 同语义）
            let output_dir = c.output_dir();
            if output_dir.is_relative() {
                c.output_dir = Some(root.path().join(output_dir));
            }
            checks.push(CheckResult {
                name: "配置",
                ok: true,
                detail: Some(format!("加载成功: {}", path.display())),
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
    let output_dir = config.output_dir();
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

    // 6. 协议兼容性探测（v52，生产诊断关键）：确认配置的 provider 协议
    //    形态在 base_url 上真实存在。此前网络检查只证明根路径可达，不证明
    //    /responses 或 /chat/completions 存在——协议缺失（404/405）时生成
    //    中途才爆错，且曾存在静默回退掩盖配置错误（v51 移除回退后，本探测
    //    承担前置诊断职责）。
    //    mock 跳过（本地模拟不触网）；Anthropic 跳过（messages 端点形态
    //    固定、无协议 404 风险，网络检查已覆盖可达性）——只探测 OpenAI 系
    //    双协议（Responses / chat/completions），按 provider 类型取对应端点。
    let protocol_check = match config.llm.provider {
        LlmProviderType::Mock | LlmProviderType::Anthropic => CheckResult {
            name: "协议",
            ok: true,
            detail: Some("mock/Anthropic：跳过协议探测（本地模拟不触网 / 协议形态固定）".to_string()),
        },
        _ => {
            let is_responses = config.llm.provider == LlmProviderType::OpenAI;
            let base_url = config
                .llm
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let endpoint = if is_responses { "responses" } else { "chat/completions" };
            let url = format!("{}/{}", base_url.trim_end_matches('/'), endpoint);
            // 最小探测请求体（1 token 预算，stream:false 一次收包；探测只关心
            // 端点到不到，不解析响应内容——即使 body 被拒（400）也说明端点存在）
            let body = if is_responses {
                serde_json::json!({
                    "model": config.llm.model,
                    "input": "ping",
                    "max_output_tokens": 1,
                    "stream": false,
                })
            } else {
                serde_json::json!({
                    "model": config.llm.model,
                    "messages": [{"role": "user", "content": "ping"}],
                    "max_tokens": 1,
                    "stream": false,
                })
            };
            let api_key = config.llm.api_key.clone().unwrap_or_default();
            // 5s 超时（与网络检查同款：探测只关心协议存在性，业务错误由
            // 实际调用暴露）。reqwest 未启用 blocking feature（与项目异步
            // 客户端一致），用 tokio runtime 单次 block_on 包装
            let available = tokio::runtime::Runtime::new()
                .map(|rt| {
                    rt.block_on(async {
                        match reqwest::Client::builder()
                            .timeout(std::time::Duration::from_secs(5))
                            .build()
                        {
                            Ok(client) => probe_protocol(&client, &url, &api_key, &body).await,
                            Err(_) => false,
                        }
                    })
                })
                .unwrap_or(false);
            let protocol_name = if is_responses { "Responses" } else { "chat/completions" };
            CheckResult {
                name: "协议",
                ok: available,
                detail: Some(if available {
                    format!("{} 协议端点存在（POST {url}）", protocol_name)
                } else {
                    format!(
                        "{} 端点不可用（POST {url} 返回 404/405 或连接失败）——请检查 provider 类型与 base_url（如 DeepSeek 仅 v4-flash 支持 Responses；不支持时应改用 openai-compatible provider）",
                        protocol_name
                    )
                }),
            }
        }
    };
    checks.push(protocol_check);

    // 7. 版本自检（v19 t01）：产物由哪个工具版本生成，与当前二进制是否一致。
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;

    /// 本地 mock HTTP server：读请求后返回固定状态码响应（协议探测测试用）。
    /// 响应带 Connection: close；返回形如 http://127.0.0.1:<port> 的 base_url。
    fn spawn_status_server(status: u16) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                std::thread::spawn(move || {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let body = r#"{"error":"probe"}"#;
                    let reason = if status == 200 { "OK" } else { "Error" };
                    let raw = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status, reason, body.len(), body
                    );
                    let _ = stream.write_all(raw.as_bytes());
                });
            }
        });
        base_url
    }

    /// 构造临时目录内的最小 mock 配置（可追加覆盖段）
    fn temp_config(tag: &str, extra: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_doctor_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("doctor-test.toml");
        std::fs::write(
            &config,
            format!(
                r#"[llm]
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
        let checks = run(Some(&config), &root).unwrap();
        assert_eq!(checks.len(), 7, "应恰好七项检查: {:?}", checks);
        for c in &checks {
            assert!(c.ok, "{} 应通过: {:?}", c.name, c.detail);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_doctor_config_missing_fails_first_check_only() {
        let dir = std::env::temp_dir().join(format!("code_repo_wiki_doctor_missing_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = ProjectRoot::new(dir.clone());
        let checks = run(Some(&dir.join("nope.toml")), &root).unwrap();
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
        let checks = run(Some(&config), &root).unwrap();
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
        let checks = run(Some(&config), &root).unwrap();
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
        let checks = run(Some(&config), &root).unwrap();
        let ver = checks.iter().find(|c| c.name == "版本").expect("应有版本检查");
        assert!(ver.ok, "无状态文件时应通过: {:?}", ver.detail);
        assert!(
            ver.detail.clone().unwrap().contains("尚无生成状态"),
            "应提示首次生成: {:?}",
            ver.detail
        );

        // 写入旧版本状态（tool_version=0.0.0）→ 提示漂移
        std::fs::create_dir_all(dir.join(".code-repo-wiki/.state")).unwrap();
        std::fs::write(
            dir.join(".code-repo-wiki/.state/generation_state.json"),
            r#"{"last_commit_hash":null,"file_fingerprints":{},"generated_at":"2025-01-01T00:00:00Z","tool_version":"0.0.0"}"#,
        )
        .unwrap();
        let checks = run(Some(&config), &root).unwrap();
        let ver = checks.iter().find(|c| c.name == "版本").expect("应有版本检查");
        assert!(ver.ok);
        let detail = ver.detail.clone().unwrap();
        assert!(detail.contains("0.0.0"), "应报告记录版本: {detail}");
        assert!(detail.contains("升级产物"), "应建议升级产物: {detail}");

        // 写入当前版本状态 → 一致通过
        let current = env!("CARGO_PKG_VERSION");
        std::fs::write(
            dir.join(".code-repo-wiki/.state/generation_state.json"),
            format!(
                r#"{{"last_commit_hash":null,"file_fingerprints":{{}},"generated_at":"2025-01-01T00:00:00Z","tool_version":"{current}"}}"#
            ),
        )
        .unwrap();
        let checks = run(Some(&config), &root).unwrap();
        let ver = checks.iter().find(|c| c.name == "版本").expect("应有版本检查");
        assert!(ver.ok);
        assert!(
            ver.detail.clone().unwrap().contains("由当前版本"),
            "版本一致应通过: {:?}",
            ver.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v52：协议探测——/responses 返回 404（服务未实现）→ 协议检查失败；
    /// 服务器在监听 → 网络检查仍通过（协议探测独立于网络层，暴露的是
    /// 协议形态错误而非可达性——生产诊断关键：404 曾触发静默回退掩盖问题）
    #[test]
    fn test_doctor_protocol_probe_detects_missing_responses() {
        let base = spawn_status_server(404);
        let (dir, config) = temp_config("proto404", "");
        let cfg_text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            &config,
            cfg_text
                .replace("provider = \"mock\"", "provider = \"openai\"")
                + &format!("base_url = \"{}/v1\"\n", base),
        )
        .unwrap();
        let root = ProjectRoot::new(dir.clone());
        let checks = run(Some(&config), &root).unwrap();
        let proto = checks.iter().find(|c| c.name == "协议").expect("应有协议检查");
        assert!(!proto.ok, "404 应判协议不可用: {:?}", proto.detail);
        assert!(
            proto.detail.clone().unwrap().contains("provider 类型"),
            "失败说明应给出配置引导: {:?}",
            proto.detail
        );
        let net = checks.iter().find(|c| c.name == "网络").expect("应有网络检查");
        assert!(net.ok, "监听中的服务器应可达: {:?}", net.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v52：协议探测——/responses 返回 401（认证错误）→ 端点存在，协议
    /// 可用（认证问题由实际调用暴露，不属于协议探测范围）
    #[test]
    fn test_doctor_protocol_probe_ok_on_auth_error() {
        let base = spawn_status_server(401);
        let (dir, config) = temp_config("proto401", "");
        let cfg_text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            &config,
            cfg_text
                .replace("provider = \"mock\"", "provider = \"openai\"")
                + &format!("base_url = \"{}/v1\"\n", base),
        )
        .unwrap();
        let root = ProjectRoot::new(dir.clone());
        let checks = run(Some(&config), &root).unwrap();
        let proto = checks.iter().find(|c| c.name == "协议").expect("应有协议检查");
        assert!(proto.ok, "401 应判协议可用（端点存在）: {:?}", proto.detail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v52：chat/completions 协议探测——端点返回 200 → 协议可用
    ///（openai-compatible 默认配置即此路径：opencode 网关 /chat/completions）
    #[test]
    fn test_doctor_protocol_probe_chat_completions_ok() {
        let base = spawn_status_server(200);
        let (dir, config) = temp_config("protochat", "");
        let cfg_text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            &config,
            cfg_text
                .replace("provider = \"mock\"", "provider = \"openai-compatible\"")
                + &format!("base_url = \"{}/v1\"\n", base),
        )
        .unwrap();
        let root = ProjectRoot::new(dir.clone());
        let checks = run(Some(&config), &root).unwrap();
        let proto = checks.iter().find(|c| c.name == "协议").expect("应有协议检查");
        assert!(proto.ok, "chat/completions 200 应判协议可用: {:?}", proto.detail);
        assert!(
            proto.detail.clone().unwrap().contains("chat/completions"),
            "应说明探测的协议形态: {:?}",
            proto.detail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
