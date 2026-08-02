//! 生成状态持久化（单进程契约：本文件与 generation_state.json 无文件锁，
//! 同一输出目录并发运行 repo-wiki 不被支持，最后写入者胜——见 README 限制项）
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

/// 生成状态
///
/// 记录上一次生成时的 commit hash 和文件指纹，
/// 用于增量更新时检测变更。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GenerationState {
    /// 上一次生成时的 commit hash
    pub last_commit_hash: Option<String>,
    /// 文件路径 → SHA256 指纹
    pub file_fingerprints: HashMap<String, String>,
    /// 生成时间（ISO 8601）
    pub generated_at: String,
    /// 已生成文档路径 → SHA256 指纹（用于检测人工修改）
    #[serde(default)]
    pub doc_fingerprints: HashMap<String, String>,
    /// 已生成文档路径 → 所属模块名（人工修改反向同步的精确归属依据；
    /// 全局文档 api/overview/toc 无模块归属，不记录。模块名压平为
    /// "::" 连接（module_path.join("::")），与卡片 module_name 同规则，
    /// 精确匹配，杜绝 stem 匹配的下划线歧义）
    #[serde(default)]
    pub doc_modules: HashMap<String, String>,
    /// 人工修改过的文档路径集合（保护集：下次自动更新不覆盖，直到 --force 清空）
    #[serde(default)]
    pub protected_docs: Vec<String>,
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

    /// 保存生成状态到目录（原子写：fs::write_file_atomic 临时文件 +
    /// rename 覆盖，防止崩溃留下半截 JSON——半截状态会被 load 判为
    /// 损坏，进而触发调用方的 fail-loud 路径，见 lib.rs load_protection）
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("创建状态目录失败: {}", state_dir.display()))?;
        let state_path = state_dir.join("generation_state.json");
        let content = serde_json::to_string_pretty(self)?;
        crate::fs::write_file_atomic(&state_path, &content)
    }

    /// 生成前中途存盘的保护字段保留（票 03）
    ///
    /// from_insights 构造的新状态只含 file_fingerprints/commit hash——
    /// 若在 LLM 生成（流水线 Phase 3，最常见失败点）之前直接落盘，
    /// doc_fingerprints/protected_docs/doc_modules 全空；一旦生成失败，
    /// 磁盘状态即"无保护"版本，下次运行的人工修改保护失效。
    /// 本方法把旧状态中的保护字段合并进新状态，使中途存盘只推进
    /// 代码侧状态（commit hash/文件指纹），产物侧保护信息不因中途
    /// 失败而丢失。新状态保护字段非空时保留新值（正常全量完成后
    /// Phase 6 的最终保存不受影响）。
    pub fn preserve_protection(&mut self, old: &GenerationState) {
        if self.protected_docs.is_empty() {
            self.protected_docs = old.protected_docs.clone();
        }
        if self.doc_fingerprints.is_empty() {
            self.doc_fingerprints = old.doc_fingerprints.clone();
        }
        if self.doc_modules.is_empty() {
            self.doc_modules = old.doc_modules.clone();
        }
    }

    /// 从文件解析结果和 commit hash 构建新的生成状态
    ///
    /// root 注入：insight.path 是相对项目根的路径，指纹计算必须先与
    /// root 拼接——直接相对 cwd 打开会随进程 cwd 漂移而错读（watch
    /// 常驻进程的 cwd 漂移不再影响指纹基准）。
    pub fn from_insights(
        root: &crate::project::ProjectRoot,
        insights: &[crate::ingest::parser::FileInsight],
        commit_hash: &str,
    ) -> Result<Self> {
        let mut file_fingerprints = HashMap::new();

        for insight in insights {
            let path_str = insight.path.to_string_lossy().to_string();
            let abs = root.path().join(&insight.path);
            match Self::compute_file_fingerprint(&abs) {
                Ok(fp) => {
                    file_fingerprints.insert(path_str, fp);
                }
                Err(e) => {
                    tracing::warn!("计算文件指纹失败 {}: {}", abs.display(), e);
                }
            }
        }

        Ok(Self {
            last_commit_hash: Some(commit_hash.to_string()),
            file_fingerprints,
            doc_fingerprints: HashMap::new(),
            doc_modules: HashMap::new(),
            protected_docs: Vec::new(),
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
    ///
    /// 路径与 render_all 实际写盘路径一致：`{output_dir}/wiki/{lang}/{file}.md`，
    /// 文件名复用 output::wiki_page_path（ArchitectureOverview 特判写
    /// architecture.md），languages 为主语言 + 扩展语言（与 output::wiki_languages 保持一致）。
    /// 另补记三个全局文档（api.md / overview.md / _toc.md）的指纹，
    /// 路径复用 output::api_doc_path / overview_doc_path / toc_doc_path，
    /// 与 render_all 的保护判定路径同一规则产出。
    /// 卡片同样计入指纹（路径 cards/{lang}/{module.replace("::","_")}.md，与
    /// render_all 写盘路径一致），使人工编辑的卡片与 wiki 页一样被
    /// detect_manually_modified 识别并纳入保护集，全量 generate 不再静默覆盖。
    ///
    /// 返回值：(文档指纹表, 文档模块归属表)。模块归属 = 产物路径 → 模块名
    /// （wiki 页取 module_path.join("::")，卡片取 module_name；api/overview/toc
    /// 全局文档无模块归属不记录），供人工修改反向同步的精确归属——
    /// 精确匹配杜绝了 stem 匹配在模块名含下划线时的串卡片歧义。
    pub fn record_doc_fingerprints(
        docs: &[crate::model::WikiDocument],
        cards: &[crate::model::KnowledgeCard],
        output_dir: &Path,
        languages: &[String],
    ) -> Result<(HashMap<String, String>, HashMap<String, String>)> {
        let mut fps = HashMap::new();
        let mut modules = HashMap::new();
        for lang in languages {
            for doc in docs {
                let doc_path = crate::output::wiki_page_path(output_dir, lang, doc);
                if doc_path.exists() {
                    let fp = Self::compute_file_fingerprint(&doc_path)?;
                    fps.insert(doc_path.to_string_lossy().to_string(), fp);
                    // 模块页记录归属；全局文档（Overview/Architecture）不记录
                    if doc.kind == crate::model::DocumentKind::WikiPage {
                        modules.insert(
                            doc_path.to_string_lossy().to_string(),
                            doc.module_path.join("::"),
                        );
                    }
                }
            }
        }
        // 全局文档指纹：api.md（每种语言一份）、overview.md（仅主语言）、_toc.md（输出目录根）
        for lang in languages {
            let api_path = crate::output::api_doc_path(output_dir, lang);
            if api_path.exists() {
                let fp = Self::compute_file_fingerprint(&api_path)?;
                fps.insert(api_path.to_string_lossy().to_string(), fp);
            }
        }
        if let Some(primary) = languages.first() {
            let overview_path = crate::output::overview_doc_path(output_dir, primary);
            if overview_path.exists() {
                let fp = Self::compute_file_fingerprint(&overview_path)?;
                fps.insert(overview_path.to_string_lossy().to_string(), fp);
            }
        }
        let toc_path = crate::output::toc_doc_path(output_dir);
        if toc_path.exists() {
            let fp = Self::compute_file_fingerprint(&toc_path)?;
            fps.insert(toc_path.to_string_lossy().to_string(), fp);
        }
        // 卡片指纹：所有语言目录下已落盘的卡片都计入（卡片实际写盘语言
        // 取决于关联文档的 doc.language，与 render_all 的写盘路径一致）
        for lang in languages {
            for card in cards {
                let card_path = crate::output::card_page_path(output_dir, lang, &card.module_name);
                if card_path.exists() {
                    let fp = Self::compute_file_fingerprint(&card_path)?;
                    fps.insert(card_path.to_string_lossy().to_string(), fp);
                    modules.insert(
                        card_path.to_string_lossy().to_string(),
                        card.module_name.clone(),
                    );
                }
            }
        }
        Ok((fps, modules))
    }

    /// 比对磁盘文档与生成时指纹，返回人工修改的文档路径集合
    ///
    /// 磁盘文件不存在或指纹读取失败时不视为人工修改（跳过保护）。
    pub fn detect_manually_modified(&self) -> Vec<String> {
        self.doc_fingerprints
            .iter()
            .filter(|(path, fp)| {
                Path::new(path).is_file()
                    && Self::compute_file_fingerprint(Path::new(path))
                        .map(|f| &f != *fp)
                        .unwrap_or(false)
            })
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// 检查文件是否已变更
    ///
    /// 如果文件不在指纹表中（新增文件）或指纹不匹配，返回 true。
    /// root 注入：path 是相对项目根的路径，与 root 拼接后计算
    /// （同 from_insights，不依赖进程 cwd）。
    pub fn is_file_changed(&self, root: &crate::project::ProjectRoot, path: &Path) -> Result<bool> {
        let path_str = path.to_string_lossy().to_string();
        let old_fingerprint = match self.file_fingerprints.get(&path_str) {
            Some(fp) => fp,
            None => return Ok(true), // 新文件
        };

        let new_fingerprint = Self::compute_file_fingerprint(&root.path().join(path))?;
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
    use crate::project::ProjectRoot;
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
            doc_fingerprints: HashMap::new(),
            doc_modules: HashMap::new(),
            protected_docs: Vec::new(),
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
            doc_fingerprints: HashMap::new(),
            doc_modules: HashMap::new(),
            protected_docs: Vec::new(),
            generated_at: String::new(),
        };

        assert!(!state.is_file_changed(&ProjectRoot::new(dir.clone()), &file_path).unwrap());

        // 修改文件
        std::fs::write(&file_path, "world").unwrap();
        assert!(state.is_file_changed(&ProjectRoot::new(dir.clone()), &file_path).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_new_file_is_changed() {
        let state = GenerationState {
            last_commit_hash: None,
            file_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::new(),
            doc_modules: HashMap::new(),
            protected_docs: Vec::new(),
            generated_at: String::new(),
        };

        let path = PathBuf::from("nonexistent.rs");
        // 新文件（不在指纹表中）应视为"已变更"
        assert!(state
            .is_file_changed(&ProjectRoot::new(std::env::temp_dir()), &path)
            .unwrap());
    }

    /// A3：卡片指纹与 wiki 页指纹一起记录，人工编辑的卡片可被检测保护
    #[test]
    fn test_record_doc_fingerprints_includes_cards() {
        let dir = std::env::temp_dir()
            .join(format!("repo_wiki_test_card_fp_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // 预写 wiki 页与卡片（路径与 render_all 落盘一致）
        let card_path = dir.join("cards").join("zh").join("src_testmodule.md");
        std::fs::create_dir_all(card_path.parent().unwrap()).unwrap();
        std::fs::write(&card_path, "卡片内容").unwrap();
        let wiki_path = dir.join("wiki").join("zh").join("src_testmodule.md");
        std::fs::create_dir_all(wiki_path.parent().unwrap()).unwrap();
        std::fs::write(&wiki_path, "页面内容").unwrap();

        let doc = crate::model::WikiDocument {
            title: "TestModule".into(),
            kind: crate::model::DocumentKind::WikiPage,
            content: String::new(),
            language: "zh".into(),
            module_path: vec!["src".into(), "testmodule".into()],
            references: vec![],
            last_updated: String::new(),
            fingerprint: None,
        };
        let card = crate::model::KnowledgeCard {
            module_name: "src::testmodule".into(),
            module_type: "module".into(),
            summary: String::new(),
            key_entities: vec![],
            dependencies: vec![],
            dependents: vec![],
            design_patterns: vec![],
            todo_notes: vec![],
            related_files: vec![],
            coding_spec: None,
            tech_stack: vec![],
            architecture: None,
            pending_manual_edits: vec![],
            features: Vec::new(),
        };

        let (fps, modules) = GenerationState::record_doc_fingerprints(&[doc], &[card], &dir, &["zh".into()]).unwrap();
        assert!(
            fps.contains_key(&card_path.to_string_lossy().to_string()),
            "已落盘的卡片应计入指纹（人工编辑后检测保护的前提）"
        );
        assert!(
            fps.contains_key(&wiki_path.to_string_lossy().to_string()),
            "wiki 页应计入指纹"
        );
        assert_eq!(
            modules.get(&card_path.to_string_lossy().to_string()).map(String::as_str),
            Some("src::testmodule"),
            "卡片指纹应记录模块归属（反向同步的精确匹配依据）"
        );
        assert_eq!(
            modules.get(&wiki_path.to_string_lossy().to_string()).map(String::as_str),
            Some("src::testmodule"),
            "wiki 页指纹应记录模块归属（module_path 连接规则）"
        );

        // 未落盘的卡片（文件不存在）不计指纹，避免无中生有的保护
        let missing_card = crate::model::KnowledgeCard {
            module_name: "src::missing".into(),
            module_type: "module".into(),
            summary: String::new(),
            key_entities: vec![],
            dependencies: vec![],
            dependents: vec![],
            design_patterns: vec![],
            todo_notes: vec![],
            related_files: vec![],
            coding_spec: None,
            tech_stack: vec![],
            architecture: None,
            pending_manual_edits: vec![],
            features: Vec::new(),
        };

        let (fps2, modules2) = GenerationState::record_doc_fingerprints(&[], &[missing_card], &dir, &["zh".into()]).unwrap();
        assert!(fps2.is_empty(), "文件不存在时不应记录指纹");
        assert!(modules2.is_empty(), "文件不存在时不应记录模块归属");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 票 03：preserve_protection 把旧状态保护字段合并进新状态
    ///（生成前中途存盘时调用，防 LLM 失败后保护丢失）
    #[test]
    fn test_preserve_protection_merges_from_old() {
        let old = GenerationState {
            last_commit_hash: Some("old".into()),
            file_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::from([("a.md".to_string(), "fp".to_string())]),
            doc_modules: HashMap::from([("a.md".to_string(), "src".to_string())]),
            protected_docs: vec!["a.md".to_string()],
            generated_at: String::new(),
        };
        let mut fresh = GenerationState {
            last_commit_hash: Some("new".into()),
            file_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::new(),
            doc_modules: HashMap::new(),
            protected_docs: vec![],
            generated_at: String::new(),
        };
        fresh.preserve_protection(&old);
        assert_eq!(fresh.protected_docs, vec!["a.md"]);
        assert_eq!(fresh.doc_fingerprints.get("a.md").map(String::as_str), Some("fp"));
        assert_eq!(fresh.doc_modules.get("a.md").map(String::as_str), Some("src"));
        // commit hash 不被旧值覆盖（只合并保护字段）
        assert_eq!(fresh.last_commit_hash.as_deref(), Some("new"));
    }

    /// 票 03：新状态保护字段非空时不覆盖（最终保存语义优先）
    #[test]
    fn test_preserve_protection_keeps_new_when_present() {
        let old = GenerationState {
            last_commit_hash: None,
            file_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::from([("old.md".to_string(), "old".to_string())]),
            doc_modules: HashMap::new(),
            protected_docs: vec!["old.md".to_string()],
            generated_at: String::new(),
        };
        let mut fresh = GenerationState {
            last_commit_hash: None,
            file_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::from([("new.md".to_string(), "new".to_string())]),
            doc_modules: HashMap::new(),
            protected_docs: vec!["new.md".to_string()],
            generated_at: String::new(),
        };
        fresh.preserve_protection(&old);
        assert_eq!(fresh.protected_docs, vec!["new.md"], "新状态保护字段非空时应保留新值");
        assert_eq!(fresh.doc_fingerprints.get("new.md").map(String::as_str), Some("new"));
        assert!(!fresh.doc_fingerprints.contains_key("old.md"));
    }
}
