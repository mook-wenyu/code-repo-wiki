//! 生成状态持久化（单进程契约：本文件与 generation_state.json 无文件锁，
//! 同一输出目录并发运行 code-repo-wiki 不被支持，最后写入者胜——见 README 限制项）
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
    /// 生成时工具版本（v19 t01 版本自检依据）
    ///
    /// from_insights 写入 env!("CARGO_PKG_VERSION")；doctor 读取并对比
    /// 当前二进制版本，捕获「PATH 里的旧版二进制生成产物后又被新版
    /// 调用」的静默漂移（旧版缺 doctor/dry-run，实测报 unrecognized
    /// subcommand exit 2，用户无从知道产物是旧格式）。旧状态文件无此
    /// 字段（serde default None）→ doctor 提示无法判断，不误报。
    #[serde(default)]
    pub tool_version: Option<String>,
    /// 上次生成失败被隔离的模块名（卡片或页面生成失败，v22 修复）
    ///
    /// 失败隔离（generate_wiki_pages/generate_all_cards 的 record_failure）
    /// 只跳过失败模块、不中断整体生成——但失败模块若源码不再变更将永远
    /// 无法补生成（增量以 git diff 触发，失败模块不在 diff 中）。
    /// 本轮生成结束时把失败模块写入状态，下次 update 时并入变更集重试；
    /// no-op 快速判定（should_skip_noop）也因非空跳过。清空时机：
    /// 成功重试（该模块生成成功）或全量生成（自然覆盖）。
    #[serde(default)]
    pub failed_modules: Vec<String>,
}

impl GenerationState {
    /// 从状态目录加载生成状态
    pub fn load(state_dir: &Path) -> Result<Self> {
        let state_path = state_dir.join("generation_state.json");
        let file = std::fs::File::open(&state_path)
            .with_context(|| format!("打开状态文件失败: {}", state_path.display()))?;
        let reader = std::io::BufReader::new(file);
        let state: GenerationState =
            serde_json::from_reader(reader).with_context(|| "解析状态文件 JSON 失败")?;
        Ok(state)
    }

    /// 保存生成状态到目录（原子写：fs::write_file_atomic 临时文件 +
    /// rename 覆盖，防止崩溃留下半截 JSON——半截状态会被 load 判为
    /// 损坏，进而触发调用方的 fail-loud 路径，见 lib.rs load_protection）
    ///
    /// 确定性序列化：file_fingerprints/doc_fingerprints/doc_modules 是
    /// HashMap，迭代序随 RandomState 每次进程而异——直序列化会让状态
    /// 文件字节级漂移（同样的内容两次写入字节不同，git diff 噪音且
    /// 破坏"同输入同输出"的确定性契约）。写入前把三个 map 按键排序
    /// 组装成 serde_json::Map，保证同状态同字节；load 反序列化对
    /// JSON 对象键序不敏感，旧文件完全兼容。
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("创建状态目录失败: {}", state_dir.display()))?;
        let state_path = state_dir.join("generation_state.json");

        let mut obj = serde_json::Map::new();
        obj.insert(
            "last_commit_hash".into(),
            serde_json::to_value(&self.last_commit_hash)?,
        );
        obj.insert(
            "file_fingerprints".into(),
            sorted_json_object(&self.file_fingerprints)?,
        );
        obj.insert(
            "generated_at".into(),
            serde_json::to_value(&self.generated_at)?,
        );
        obj.insert(
            "doc_fingerprints".into(),
            sorted_json_object(&self.doc_fingerprints)?,
        );
        obj.insert("doc_modules".into(), sorted_json_object(&self.doc_modules)?);
        obj.insert(
            "protected_docs".into(),
            serde_json::to_value(&self.protected_docs)?,
        );
        obj.insert(
            "tool_version".into(),
            serde_json::to_value(&self.tool_version)?,
        );
        obj.insert(
            "failed_modules".into(),
            serde_json::to_value(&self.failed_modules)?,
        );

        let content = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
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
            tool_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            failed_modules: Vec::new(),
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
    /// 磁盘文件不存在时不视为人工修改（跳过保护）；指纹读取失败时
    /// **保守计入**修改集（保护优先）——宁可多保护一次也不让人工编辑
    /// 内容在下次生成中被静默覆盖（读取失败通常伴随权限/IO 异常，
    /// 此时无法确认磁盘内容是否仍是上次生成的原样）。
    pub fn detect_manually_modified(&self) -> Vec<String> {
        let mut modified = Vec::new();
        for (path, fp) in &self.doc_fingerprints {
            let p = Path::new(path);
            if !p.is_file() {
                // 文件不存在（被删除或从未落盘）：不构成"人工修改"
                continue;
            }
            match Self::compute_file_fingerprint(p) {
                Ok(cur) => {
                    if &cur != fp {
                        modified.push(path.clone());
                    }
                }
                Err(e) => {
                    tracing::warn!("文档指纹读取失败，保守计入保护集: {}: {}", path, e);
                    modified.push(path.clone());
                }
            }
        }
        modified
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

/// 把字符串→字符串 HashMap 按键排序序列化为 JSON 对象
/// （save 的确定性序列化用：排序后的 serde_json::Map 保证输出字节稳定）
fn sorted_json_object(map: &HashMap<String, String>) -> Result<serde_json::Value> {
    let mut sorted: Vec<(&String, &String)> = map.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut obj = serde_json::Map::new();
    for (k, v) in sorted {
        obj.insert(k.clone(), serde_json::to_value(v)?);
    }
    Ok(serde_json::Value::Object(obj))
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
        let dir = std::env::temp_dir().join("code-repo-wiki-test-state");
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
            tool_version: None,
            failed_modules: vec!["src::output".into(), "tests::edge".into()],
        };

        state.save(&dir).unwrap();
        assert!(dir.join("generation_state.json").exists());

        let loaded = GenerationState::load(&dir).unwrap();
        assert_eq!(loaded.last_commit_hash, Some("abc123".into()));
        assert_eq!(
            loaded.file_fingerprints.get("src/main.rs").unwrap(),
            "deadbeef"
        );
        // v23 C 组防回归：失败模块必须随状态落盘（此前 lib.rs 顺序错误
        // 导致恒为空数组，v22 补偿机制静默失效——实测全量 generate 的
        // 3 个失败模块未被记录）
        assert_eq!(loaded.failed_modules, vec!["src::output", "tests::edge"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_file_changed() {
        let dir = std::env::temp_dir().join("code-repo-wiki-test-fingerprint");
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
            tool_version: None,
            failed_modules: vec![],
        };

        assert!(
            !state
                .is_file_changed(&ProjectRoot::new(dir.clone()), &file_path)
                .unwrap()
        );

        // 修改文件
        std::fs::write(&file_path, "world").unwrap();
        assert!(
            state
                .is_file_changed(&ProjectRoot::new(dir.clone()), &file_path)
                .unwrap()
        );

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
            tool_version: None,
            failed_modules: vec![],
        };

        let path = PathBuf::from("nonexistent.rs");
        // 新文件（不在指纹表中）应视为"已变更"
        assert!(
            state
                .is_file_changed(&ProjectRoot::new(std::env::temp_dir()), &path)
                .unwrap()
        );
    }

    /// A3：卡片指纹与 wiki 页指纹一起记录，人工编辑的卡片可被检测保护
    #[test]
    fn test_record_doc_fingerprints_includes_cards() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_card_fp_{}",
            std::process::id()
        ));
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
            based_on_commit: None,
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
            design_rationale: None,
            pending_manual_edits: vec![],
            features: Vec::new(),
        };

        let (fps, modules) =
            GenerationState::record_doc_fingerprints(&[doc], &[card], &dir, &["zh".into()])
                .unwrap();
        assert!(
            fps.contains_key(&card_path.to_string_lossy().to_string()),
            "已落盘的卡片应计入指纹（人工编辑后检测保护的前提）"
        );
        assert!(
            fps.contains_key(&wiki_path.to_string_lossy().to_string()),
            "wiki 页应计入指纹"
        );
        assert_eq!(
            modules
                .get(&card_path.to_string_lossy().to_string())
                .map(String::as_str),
            Some("src::testmodule"),
            "卡片指纹应记录模块归属（反向同步的精确匹配依据）"
        );
        assert_eq!(
            modules
                .get(&wiki_path.to_string_lossy().to_string())
                .map(String::as_str),
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
            design_rationale: None,
            pending_manual_edits: vec![],
            features: Vec::new(),
        };

        let (fps2, modules2) =
            GenerationState::record_doc_fingerprints(&[], &[missing_card], &dir, &["zh".into()])
                .unwrap();
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
            tool_version: None,
            failed_modules: vec![],
        };
        let mut fresh = GenerationState {
            last_commit_hash: Some("new".into()),
            file_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::new(),
            doc_modules: HashMap::new(),
            protected_docs: vec![],
            generated_at: String::new(),
            tool_version: None,
            failed_modules: vec![],
        };
        fresh.preserve_protection(&old);
        assert_eq!(fresh.protected_docs, vec!["a.md"]);
        assert_eq!(
            fresh.doc_fingerprints.get("a.md").map(String::as_str),
            Some("fp")
        );
        assert_eq!(
            fresh.doc_modules.get("a.md").map(String::as_str),
            Some("src")
        );
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
            tool_version: None,
            failed_modules: vec![],
        };
        let mut fresh = GenerationState {
            last_commit_hash: None,
            file_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::from([("new.md".to_string(), "new".to_string())]),
            doc_modules: HashMap::new(),
            protected_docs: vec!["new.md".to_string()],
            generated_at: String::new(),
            tool_version: None,
            failed_modules: vec![],
        };
        fresh.preserve_protection(&old);
        assert_eq!(
            fresh.protected_docs,
            vec!["new.md"],
            "新状态保护字段非空时应保留新值"
        );
        assert_eq!(
            fresh.doc_fingerprints.get("new.md").map(String::as_str),
            Some("new")
        );
        assert!(!fresh.doc_fingerprints.contains_key("old.md"));
    }

    /// B1：状态文件字节级确定性——同一状态两次 save 字节完全一致
    /// （三个 HashMap 的迭代序随机，直序列化会漂移；save 按键排序后
    /// 保证同状态同字节，防 git diff 噪音与确定性契约破坏）
    #[test]
    fn test_save_is_byte_deterministic() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_state_deterministic_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // 故意乱序插入，且键序跨越不同前缀，确保排序逻辑真正生效
        let mut file_fps = HashMap::new();
        file_fps.insert("z.rs".into(), "z-fp".into());
        file_fps.insert("a/b.rs".into(), "b-fp".into());
        file_fps.insert("m.rs".into(), "m-fp".into());
        let mut doc_fps = HashMap::new();
        doc_fps.insert("wiki/zh/zz.md".into(), "1".into());
        doc_fps.insert("wiki/zh/aa.md".into(), "2".into());
        let mut doc_mods = HashMap::new();
        doc_mods.insert("wiki/zh/zz.md".into(), "z".into());
        doc_mods.insert("wiki/zh/aa.md".into(), "a".into());

        let state = GenerationState {
            last_commit_hash: Some("abc".into()),
            file_fingerprints: file_fps,
            doc_fingerprints: doc_fps,
            doc_modules: doc_mods,
            protected_docs: vec!["wiki/zh/aa.md".into()],
            generated_at: "2026-01-01T00:00:00Z".into(),
            tool_version: None,
            failed_modules: vec![],
        };

        state.save(&dir).unwrap();
        let bytes1 = std::fs::read(dir.join("generation_state.json")).unwrap();

        // 直接调用 save 到另一个目录再比对（同状态两次序列化）
        let dir2 = dir.join("again");
        state.save(&dir2).unwrap();
        let bytes2 = std::fs::read(dir2.join("generation_state.json")).unwrap();

        assert_eq!(
            bytes1, bytes2,
            "同一状态两次 save 必须字节一致（HashMap 迭代序不得泄漏到序列化输出）"
        );

        // load 兼容：排序后的 JSON 反序列化回等值状态
        let loaded = GenerationState::load(&dir).unwrap();
        assert_eq!(
            loaded.file_fingerprints.get("z.rs").map(String::as_str),
            Some("z-fp")
        );
        assert_eq!(
            loaded.file_fingerprints.get("a/b.rs").map(String::as_str),
            Some("b-fp")
        );
        assert_eq!(
            loaded.doc_modules.get("wiki/zh/aa.md").map(String::as_str),
            Some("a")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A5：指纹读取失败（文件存在但不可读）时保守计入保护集，
    /// 防止人工修改内容在下次生成中被静默覆盖
    #[cfg(windows)]
    #[test]
    fn test_detect_manually_modified_read_failure_is_protected() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_detect_readfail_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let locked = dir.join("locked.md");
        std::fs::write(&locked, "content").unwrap();

        // 独占锁定文件：后续 File::open 在 Windows 上会因共享冲突失败
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .expect("独占打开应成功");

        let state = GenerationState {
            last_commit_hash: None,
            file_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::from([(
                locked.to_string_lossy().to_string(),
                "旧指纹".to_string(),
            )]),
            doc_modules: HashMap::new(),
            protected_docs: Vec::new(),
            generated_at: String::new(),
            tool_version: None,
            failed_modules: vec![],
        };

        let modified = state.detect_manually_modified();
        assert!(
            modified.iter().any(|p| Path::new(p) == locked.as_path()),
            "指纹读取失败的文件应保守计入保护集（否则人工修改会被覆盖）: {:?}",
            modified
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A5：常规分支——指纹不匹配计入、文件不存在跳过、指纹匹配不计入
    #[test]
    fn test_detect_manually_modified_regular_branches() {
        let dir = std::env::temp_dir().join(format!(
            "code_repo_wiki_test_detect_regular_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let unchanged = dir.join("unchanged.md");
        std::fs::write(&unchanged, "原样").unwrap();
        let edited = dir.join("edited.md");
        std::fs::write(&edited, "原样").unwrap();

        let state = GenerationState {
            last_commit_hash: None,
            file_fingerprints: HashMap::new(),
            doc_fingerprints: HashMap::from([
                (
                    unchanged.to_string_lossy().to_string(),
                    GenerationState::compute_file_fingerprint(&unchanged).unwrap(),
                ),
                // 指纹与磁盘内容不符 → 人工修改
                (
                    edited.to_string_lossy().to_string(),
                    "definitely-not-matching".to_string(),
                ),
                // 文件不存在 → 跳过
                (
                    dir.join("missing.md").to_string_lossy().to_string(),
                    "x".to_string(),
                ),
            ]),
            doc_modules: HashMap::new(),
            protected_docs: Vec::new(),
            generated_at: String::new(),
            tool_version: None,
            failed_modules: vec![],
        };

        std::fs::write(&edited, "被人改了").unwrap();

        let modified = state.detect_manually_modified();
        assert_eq!(
            modified.len(),
            1,
            "只有内容不符的文件应计入: {:?}",
            modified
        );
        assert!(modified.iter().any(|p| Path::new(p) == edited.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
