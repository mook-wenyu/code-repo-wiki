use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

/// 生成状态
///
/// 记录上一次生成时的 commit hash、文件指纹和模块指纹，
/// 用于增量更新时检测变更。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GenerationState {
    /// 上一次生成时的 commit hash
    pub last_commit_hash: Option<String>,
    /// 文件路径 → SHA256 指纹
    pub file_fingerprints: HashMap<String, String>,
    /// 模块名 → SHA256 指纹
    pub module_fingerprints: HashMap<String, String>,
    /// 生成时间（ISO 8601）
    pub generated_at: String,
    /// 已生成文档路径 → SHA256 指纹（用于检测人工修改）
    #[serde(default)]
    pub doc_fingerprints: HashMap<String, String>,
}

impl GenerationState {
    /// 从状态目录加载生成状态
    pub fn load(state_dir: &Path) -> Result<Self> {
        let state_path = state_dir.join("generation_state.json");
        let file = std::fs::File::open(&state_path)
            .with_context(|| format!("打开状态文件失败: {}", state_path.display()))?;
        let reader = std::io::BufReader::new(file);
        let state: GenerationState = serde_json::from_reader(reader)
            .with_context(|| "解析状态文件 JSON 失败")?;
        Ok(state)
    }

    /// 保存生成状态到目录
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("创建状态目录失败: {}", state_dir.display()))?;
        let state_path = state_dir.join("generation_state.json");
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&state_path, content)
            .with_context(|| format!("写入状态文件失败: {}", state_path.display()))?;
        Ok(())
    }

    /// 从文件解析结果和 commit hash 构建新的生成状态
    pub fn from_insights(
        insights: &[crate::ingest::parser::FileInsight],
        commit_hash: &str,
    ) -> Result<Self> {
        let mut file_fingerprints = HashMap::new();

        for insight in insights {
            let path_str = insight.path.to_string_lossy().to_string();
            match Self::compute_file_fingerprint(&insight.path) {
                Ok(fp) => {
                    file_fingerprints.insert(path_str, fp);
                }
                Err(e) => {
                    tracing::warn!("计算文件指纹失败 {}: {}", insight.path.display(), e);
                }
            }
        }

        Ok(Self {
            last_commit_hash: Some(commit_hash.to_string()),
            file_fingerprints,
            module_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::new(),
            generated_at: chrono::Utc::now().to_rfc3339(),
        })
    }

    /// 计算文件的 SHA256 指纹
    ///
    /// 非 UTF-8 文件返回错误。
    pub fn compute_file_fingerprint(path: &Path) -> Result<String> {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("打开文件失败: {}", path.display()))?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .with_context(|| format!("读取文件失败: {}", path.display()))?;
        Ok(sha256_hex(&buffer))
    }

    /// 记录所有已生成文档的 SHA256 指纹
    pub fn record_doc_fingerprints(docs: &[crate::model::WikiDocument], output_dir: &Path) -> Result<HashMap<String, String>> {
        let mut fps = HashMap::new();
        for doc in docs {
            let file_name = if doc.module_path.is_empty() {
                format!("{}.md", doc.title)
            } else {
                format!("{}.md", doc.module_path.join("_"))
            };
            let doc_path = output_dir.join("wiki").join(&file_name);
            if doc_path.exists() {
                let fp = Self::compute_file_fingerprint(&doc_path)?;
                fps.insert(doc_path.to_string_lossy().to_string(), fp);
            }
        }
        Ok(fps)
    }

    /// 检查文件是否已变更
    ///
    /// 如果文件不在指纹表中（新增文件）或指纹不匹配，返回 true。
    pub fn is_file_changed(&self, path: &Path) -> Result<bool> {
        let path_str = path.to_string_lossy().to_string();
        let old_fingerprint = match self.file_fingerprints.get(&path_str) {
            Some(fp) => fp,
            None => return Ok(true), // 新文件
        };

        let new_fingerprint = Self::compute_file_fingerprint(path)?;
        Ok(&new_fingerprint != old_fingerprint)
    }
}

/// 计算字节数组的 SHA256 十六进制字符串
fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_sha256_hex() {
        let data = b"hello world";
        let hash = sha256_hex(data);
        assert_eq!(hash.len(), 64);
        // SHA256("hello world") 的前几个字符
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_state_save_load_roundtrip() {
        let dir = std::env::temp_dir().join("repo-wiki-test-state");
        let _ = std::fs::remove_dir_all(&dir);

        let state = GenerationState {
            last_commit_hash: Some("abc123".into()),
            file_fingerprints: {
                let mut m = HashMap::new();
                m.insert("src/main.rs".into(), "deadbeef".into());
                m
            },
            module_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::new(),
            generated_at: "2025-01-01T00:00:00Z".into(),
        };

        state.save(&dir).unwrap();
        assert!(dir.join("generation_state.json").exists());

        let loaded = GenerationState::load(&dir).unwrap();
        assert_eq!(loaded.last_commit_hash, Some("abc123".into()));
        assert_eq!(
            loaded.file_fingerprints.get("src/main.rs").unwrap(),
            "deadbeef"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_file_changed() {
        let dir = std::env::temp_dir().join("repo-wiki-test-fingerprint");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let state = GenerationState {
            last_commit_hash: None,
            file_fingerprints: {
                let mut m = HashMap::new();
                m.insert(
                    file_path.to_string_lossy().to_string(),
                    GenerationState::compute_file_fingerprint(&file_path).unwrap(),
                );
                m
            },
            module_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::new(),
            generated_at: String::new(),
        };

        assert!(!state.is_file_changed(&file_path).unwrap());

        // 修改文件
        std::fs::write(&file_path, "world").unwrap();
        assert!(state.is_file_changed(&file_path).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_new_file_is_changed() {
        let state = GenerationState {
            last_commit_hash: None,
            file_fingerprints: HashMap::new(),
            module_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::new(),
            generated_at: String::new(),
        };

        let path = PathBuf::from("nonexistent.rs");
        // 新文件（不在指纹表中）应视为"已变更"
        assert!(state.is_file_changed(&path).unwrap());
    }
}
