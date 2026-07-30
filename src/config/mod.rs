pub mod opencode;
pub mod schema;

use std::path::Path;

use anyhow::{Context, Result};

/// 从文件加载配置，缺失字段用默认值填充
pub fn load_config(path: &Path) -> Result<schema::WikiConfig> {
    if !path.exists() {
        anyhow::bail!("配置文件不存在: {}", path.display());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
    let config: schema::WikiConfig = toml::from_str(&content)
        .with_context(|| format!("解析配置文件失败: {}", path.display()))?;
    validate_config(&config)?;
    Ok(config)
}

/// 创建默认配置文件
pub fn create_default_config(path: &Path) -> Result<schema::WikiConfig> {
    let config = schema::WikiConfig::default();
    let content = toml::to_string_pretty(&config)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(config)
}

/// 校验配置合法性
fn validate_config(config: &schema::WikiConfig) -> Result<()> {
    if config.llm.max_concurrent == 0 {
        anyhow::bail!("llm.max_concurrent 必须大于 0");
    }
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
        assert_eq!(parsed.llm.model, "gpt-4o");
        assert_eq!(parsed.wiki.language, "zh");
        assert_eq!(parsed.output.dir, ".repo-wiki");
    }

    #[test]
    fn test_validate_zero_concurrent() {
        let mut config = schema::WikiConfig::default();
        config.llm.max_concurrent = 0;
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn test_validate_empty_include() {
        let mut config = schema::WikiConfig::default();
        config.scope.include = vec![];
        assert!(validate_config(&config).is_err());
    }
}
