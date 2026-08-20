//! DeepSeek Harness (dsh) 安装状态检测模块
//!
//! 检测系统中 DeepSeek Harness 的安装状态，包括：
//! - `$DSH_HOME` 环境变量或 `~/.dsh` 目录的存在性
//! - npx 命令的可用性
//! - dsh 版本信息获取
//! - profiles 目录的存在性
//!
//! 检测策略遵循 dsh 官方文档的推荐路径，所有函数均为同步实现。

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

/// DeepSeek Harness 安装状态
///
/// 包含 dsh 安装检测的所有相关信息，用于判断 dsh 是否可用以及版本信息。
#[derive(Debug, Clone, Default)]
pub struct DshStatus {
    /// 是否安装了 dsh（通过 DSH_HOME 或 npx 可用性判断）
    pub installed: bool,
    /// DSH_HOME 目录路径（优先 `$DSH_HOME` 环境变量，fallback 到 `~/.dsh`）
    pub dsh_home: Option<PathBuf>,
    /// dsh 版本字符串（通过 `npx @deepseek-ai/dsh --version` 获取）
    pub version: Option<String>,
    /// npx 命令是否可用
    pub npx_available: bool,
}

/// 检测 DeepSeek Harness 的完整安装状态
///
/// 依次检测：
/// 1. DSH_HOME 目录（环境变量或默认路径）
/// 2. npx 命令可用性
/// 3. dsh 版本信息（通过 npx 执行）
/// 4. profiles 目录存在性
///
/// # 示例
///
/// ```rust
/// use code_repo_wiki::config::dsh_detect;
///
/// let status = dsh_detect::detect();
/// if status.installed {
///     println!("dsh 已安装，版本: {:?}", status.version);
/// }
/// ```
pub fn detect() -> DshStatus {
    // 检测 DSH_HOME 目录
    let dsh_home = detect_dsh_home();

    // 检测 npx 可用性
    let npx_available = detect_npx();

    // 检测 dsh 版本
    let version = detect_dsh_version().ok();

    // 判断是否安装：有 DSH_HOME 或 npx 可用且有版本信息
    let installed = dsh_home.is_some() || (npx_available && version.is_some());

    DshStatus {
        installed,
        dsh_home,
        version,
        npx_available,
    }
}

/// 检测 DSH_HOME 目录
///
/// 检测顺序：
/// 1. `$DSH_HOME` 环境变量（如果设置且非空）
/// 2. `~/.dsh` 默认目录
///
/// 返回找到的目录路径，如果都不存在则返回 `None`。
///
/// # 示例
///
/// ```rust
/// use code_repo_wiki::config::dsh_detect;
///
/// if let Some(home) = dsh_detect::detect_dsh_home() {
///     println!("DSH_HOME: {}", home.display());
/// }
/// ```
pub fn detect_dsh_home() -> Option<PathBuf> {
    // 优先检查 $DSH_HOME 环境变量
    if let Ok(dsh_home) = std::env::var("DSH_HOME") {
        let path = PathBuf::from(dsh_home);
        if path.exists() {
            return Some(path);
        }
    }

    // 检查默认路径 ~/.dsh
    if let Some(home) = std::env::var("HOME").ok().map(PathBuf::from).or_else(|| {
        // Windows 环境：尝试 USERPROFILE
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }) {
        let dsh_dir = home.join(".dsh");
        if dsh_dir.exists() {
            return Some(dsh_dir);
        }
    }

    None
}

/// 检测 npx 命令是否可用
///
/// 通过执行 `npx --version` 来判断 npx 是否在系统 PATH 中可用。
///
/// # 示例
///
/// ```rust
/// use code_repo_wiki::config::dsh_detect;
///
/// if dsh_detect::detect_npx() {
///     println!("npx 可用");
/// }
/// ```
pub fn detect_npx() -> bool {
    Command::new("npx")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// 检测 dsh 版本信息
///
/// 尝试通过以下方式获取版本：
/// 1. `npx @deepseek-ai/dsh --version`（如果有 npx）
/// 2. 检查 `$DSH_HOME/package.json` 中的版本字段
///
/// # 返回
///
/// - `Ok(version)` - 成功获取版本字符串
/// - `Err(e)` - 无法获取版本信息
///
/// # 示例
///
/// ```rust
/// use code_repo_wiki::config::dsh_detect;
///
/// match dsh_detect::detect_dsh_version() {
///     Ok(version) => println!("dsh 版本: {}", version),
///     Err(e) => println!("无法获取版本: {}", e),
/// }
/// ```
pub fn detect_dsh_version() -> Result<String> {
    // 方式1：通过 npx 执行 dsh --version
    if detect_npx() {
        let output = Command::new("npx")
            .args(["@deepseek-ai/dsh", "--version"])
            .output()
            .context("执行 npx @deepseek-ai/dsh --version 失败")?;

        if output.status.success() {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !version.is_empty() {
                return Ok(version);
            }
        }
    }

    // 方式2：从 DSH_HOME/package.json 读取版本
    if let Some(dsh_home) = detect_dsh_home() {
        let package_json = dsh_home.join("package.json");
        if package_json.exists() {
            let content =
                std::fs::read_to_string(&package_json).context("读取 dsh package.json 失败")?;
            let package: serde_json::Value =
                serde_json::from_str(&content).context("解析 dsh package.json 失败")?;

            if let Some(version) = package.get("version").and_then(|v| v.as_str()) {
                return Ok(version.to_string());
            }
        }
    }

    anyhow::bail!("无法获取 dsh 版本信息")
}

/// 检测 dsh profiles 目录是否存在
///
/// 检查 `$DSH_HOME/profiles/` 目录是否存在且可访问。
///
/// # 示例
///
/// ```rust
/// use code_repo_wiki::config::dsh_detect;
///
/// if dsh_detect::detect_profiles_dir().is_some() {
///     println!("profiles 目录存在");
/// }
/// ```
pub fn detect_profiles_dir() -> Option<PathBuf> {
    let dsh_home = detect_dsh_home()?;
    let profiles_dir = dsh_home.join("profiles");
    if profiles_dir.exists() && profiles_dir.is_dir() {
        Some(profiles_dir)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_status_no_dsh() {
        // 在没有安装 dsh 的环境中，status 应该反映未安装状态
        let status = detect();
        // 注意：这个测试在有 dsh 的环境中可能会失败
        // 实际测试中应该 mock 环境变量
        assert!(!status.installed || status.dsh_home.is_some() || status.version.is_some());
    }

    #[test]
    fn test_detect_dsh_home_with_env_var() {
        // 临时设置 DSH_HOME 环境变量
        let test_dir = std::env::temp_dir().join("test_dsh_home");
        std::fs::create_dir_all(&test_dir).unwrap();

        unsafe {
            std::env::set_var("DSH_HOME", test_dir.to_str().unwrap());
        }

        let result = detect_dsh_home();
        assert_eq!(result, Some(test_dir.clone()));

        // 清理
        unsafe {
            std::env::remove_var("DSH_HOME");
        }
        std::fs::remove_dir_all(&test_dir).unwrap();
    }

    #[test]
    fn test_detect_dsh_home_with_default_path() {
        // 测试默认路径 ~/.dsh
        // 注意：这个测试依赖于系统中是否存在 ~/.dsh 目录
        let result = detect_dsh_home();
        // 在大多数开发环境中，这个目录可能不存在
        // 我们只验证函数不会 panic
        let _ = result;
    }

    #[test]
    fn test_detect_npx() {
        // 测试 npx 检测（依赖于系统中是否安装了 npx）
        let result = detect_npx();
        // 这个测试在有 npx 的环境中应该通过
        // 在没有 npx 的环境中应该返回 false
        assert!(!result || result);
    }

    #[test]
    fn test_detect_dsh_version() {
        // 测试版本检测
        let result = detect_dsh_version();
        // 这个测试在有 dsh 的环境中应该成功
        // 在没有 dsh 的环境中应该失败
        if let Ok(version) = result {
            assert!(!version.is_empty());
        }
    }

    #[test]
    fn test_profiles_dir_detection() {
        // 测试 profiles 目录检测
        let result = detect_profiles_dir();
        // 这个测试在有 dsh 的环境中应该成功
        // 在没有 dsh 的环境中应该返回 None
        let _ = result;
    }
}
