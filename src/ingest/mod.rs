/// 扫描与解析层（单进程契约：insights_cache.json 无文件锁，
/// 同一输出目录并发运行不被支持，见 README 限制项）
pub mod scanner;
pub mod parser;

use anyhow::Result;
use crate::project::ProjectRoot;
use parser::{FileInsight, ParserRegistry};

/// 扫描结果：成功解析的文件 + 解析失败文件数（B5：失败可观测，
/// 此前失败仅在日志中出现，AnalysisStats 无计数——解析失败的文件
/// 不会出现在 insights 中，下游（覆盖率/统计）会误以为全部成功）
pub struct ScanOutput {
    pub insights: Vec<FileInsight>,
    /// 扫描范围内解析失败的文件数（非 UTF-8 读取失败 / tree-sitter 解析错误）
    pub files_failed: usize,
}

/// 在指定项目根下执行扫描和解析（全量解析：委托缓存版，传入空变更集）
///
/// 扫描根与路径相对化基准都取自 root，不再依赖进程 cwd——
/// 测试可在临时目录构造 ProjectRoot 验证扫描行为，watch 常驻进程
/// 的 cwd 漂移不再影响扫描范围。
pub fn scan_and_parse_at(root: &ProjectRoot) -> Result<ScanOutput> {
    scan_and_parse_cached_at(root, &None, &std::collections::HashSet::new())
}

/// 带解析缓存的扫描（真增量扫描的 parse 层增量）
///
/// `cache_path` 为 Some 时启用缓存：变更集内的文件强制重新解析，其余文件
/// 按内容指纹复用缓存结果（指纹不匹配才重新 tree-sitter 解析）。
/// 缓存缺失/损坏时 warn 并全量重建（缓存是加速产物，重建代价可接受，
/// 属可观测性契约内的降级路径，不静默）。`cache_path` 为 None 时全量解析。
///
/// 变更集路径判定用 PathBuf 组件比较（Windows 下正/反斜杠视为同一路径，
/// 与 git diff 的正斜杠相对路径形态一致）。
pub fn scan_and_parse_cached_at(
    root: &ProjectRoot,
    cache_path: &Option<std::path::PathBuf>,
    changed_files: &std::collections::HashSet<std::path::PathBuf>,
) -> Result<ScanOutput> {
    let scanner = scanner::Scanner::new(root.path());
    // 扫描产出绝对路径；转换为相对扫描根的路径——
    // 模块名派生（graph/chunk 的 Normal 组件提取）、搜索索引、指纹记录
    // 全部以相对路径为基准，杜绝绝对路径污染模块名（此前产出
    // RustProjects_repo-wiki_src 这类含机器路径的模块名）。
    let files = scanner
        .scan()?
        .into_iter()
        .map(|f| f.strip_prefix(root.path()).map(|p| p.to_path_buf()).unwrap_or(f))
        .collect::<Vec<_>>();

    let mut cache = load_insights_cache(cache_path);
    let registry = ParserRegistry::new();

    let mut insights = Vec::new();
    let mut reused = 0usize;
    let mut files_failed = 0usize;
    for file in &files {
        let processor = match registry.get_for_file(file) {
            Some(p) => p,
            None => continue,
        };
        // 读取用绝对路径（相对路径依赖 cwd，--root 与 cwd 分离时会读错文件）；
        // insight.path 保持相对路径（下游模块名派生/指纹记录的既定基准）
        let abs = if file.is_absolute() { file.clone() } else { root.path().join(file) };
        let source = match std::fs::read_to_string(&abs) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("跳过非 UTF-8 文件 {}: {}", abs.display(), e);
                files_failed += 1;
                continue;
            }
        };

        // 缓存命中判定：变更集内文件强制重解析；否则按内容指纹复用
        let fingerprint = fingerprint_of(&source);
        let key = file.to_string_lossy().to_string();
        let cached = cache.get(&key);
        let use_cache = !changed_files.contains(file)
            && cached.is_some_and(|c| c.fingerprint == fingerprint);
        // 缓存命中直接复用（指纹一致且不在变更集内）
        if use_cache
            && let Some(c) = cached
        {
            insights.push(c.insight.clone());
            reused += 1;
            continue;
        }

        match processor.parse(&source, file) {
            Ok(insight) => {
                let cached = CachedInsight {
                    path: key,
                    fingerprint,
                    insight: insight.clone(),
                };
                cache.insert(cached.path.clone(), cached);
                insights.push(insight);
            }
            Err(e) => {
                tracing::error!("解析失败 {}: {}", file.display(), e);
                files_failed += 1;
            }
        }
    }

    // N15：缓存按本次扫描文件集裁剪——被删除/移出 include 的源文件
    // 残留缓存条目（路径+旧解析结果）随文件消失成为死数据，且其
    // file_path 相对形态与本次扫描不一致（旧前缀目录），写回前剔除，
    // 防止缓存无限膨胀与陈旧条目误命中（watch 长期运行的场景）
    let valid_keys: std::collections::HashSet<&std::path::Path> =
        files.iter().map(|f| f.as_path()).collect();
    cache.retain(|path, _| valid_keys.contains(std::path::Path::new(path)));

    // 写回缓存（辅助产物：失败仅告警，下次扫描降级为空缓存全量重建）
    if let Some(path) = cache_path
        && let Err(e) = save_insights_cache(path, &cache)
    {
        tracing::warn!("解析缓存写入失败: {}", e);
    }

    tracing::info!(
        "扫描完成: 共 {} 个文件, 成功解析 {} 个（缓存复用 {} 个, 失败 {} 个）",
        files.len(),
        insights.len(),
        reused,
        files_failed
    );
    Ok(ScanOutput { insights, files_failed })
}

/// 解析缓存条目：路径（相对项目根，与 insight.path 同形态）+ 内容指纹 + 解析结果
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CachedInsight {
    pub path: String,
    pub fingerprint: String,
    pub insight: FileInsight,
}

/// 读取解析缓存（路径为 None 返回空缓存；文件缺失/损坏返回空缓存并告警）
fn load_insights_cache(cache_path: &Option<std::path::PathBuf>) -> std::collections::HashMap<String, CachedInsight> {
    let Some(path) = cache_path else {
        return std::collections::HashMap::new();
    };
    if !path.exists() {
        return std::collections::HashMap::new();
    }
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<Vec<CachedInsight>>(&content) {
            Ok(list) => list.into_iter().map(|c| (c.path.clone(), c)).collect(),
            Err(e) => {
                tracing::warn!("解析缓存损坏（将全量重建）: {}: {}", path.display(), e);
                std::collections::HashMap::new()
            }
        },
        Err(e) => {
            tracing::warn!("解析缓存读取失败（将全量重建）: {}: {}", path.display(), e);
            std::collections::HashMap::new()
        }
    }
}

/// 写回解析缓存（按路径排序保证确定性；版本演进时旧格式自然读失败 → 全量重建）
fn save_insights_cache(cache_path: &std::path::Path, cache: &std::collections::HashMap<String, CachedInsight>) -> Result<()> {
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut list: Vec<&CachedInsight> = cache.values().collect();
    list.sort_by(|a, b| a.path.cmp(&b.path));
    // 原子写（fs::write_file_atomic）：缓存损坏的后果只是全量重建
    // （warn 路径），但半截文件会增加损坏概率，原子写消除此来源
    crate::fs::write_file_atomic(cache_path, &serde_json::to_string_pretty(&list)?)
}

/// 文件内容 SHA256 指纹（与 GenerationState::compute_file_fingerprint 同款算法，
/// 此处直接对已读入内容计算，避免二次读盘）
fn fingerprint_of(source: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    hex::encode(hasher.finalize())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 构造临时项目根：src/a.rs + src/b.rs
    fn temp_project(tag: &str) -> ProjectRoot {
        let dir = std::env::temp_dir().join(format!("repo_wiki_cache_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.join("src").join("b.rs"), "pub fn beta() {}\n").unwrap();
        ProjectRoot::new(dir)
    }

    fn cache_path(root: &ProjectRoot) -> std::path::PathBuf {
        root.path().join(".state").join("insights_cache.json")
    }

    /// 首次扫描写缓存：缓存文件存在且为合法 JSON（可反序列化为 CachedInsight 列表）
    #[test]
    fn test_cached_scan_writes_cache_file() {
        let root = temp_project("write");
        let cp = Some(cache_path(&root));
        let insights = scan_and_parse_cached_at(&root, &cp, &std::collections::HashSet::new()).unwrap().insights;
        assert_eq!(insights.len(), 2, "两个 .rs 文件都应解析");

        let content = std::fs::read_to_string(cache_path(&root)).unwrap();
        let list: Vec<CachedInsight> = serde_json::from_str(&content).unwrap();
        assert_eq!(list.len(), 2, "缓存应含两个条目");
        let _ = std::fs::remove_dir_all(root.path());
    }

    /// 指纹失效重解析：修改文件内容后再次扫描，返回的 insight.source 是新内容
    ///（若缓存错误地命中旧指纹，source 会是旧内容——source 字段是复用的直接证据）
    #[test]
    fn test_cached_scan_reparses_on_fingerprint_change() {
        let root = temp_project("invalidate");
        let cp = Some(cache_path(&root));
        let first = scan_and_parse_cached_at(&root, &cp, &std::collections::HashSet::new()).unwrap().insights;
        let alpha_source = first.iter().find(|i| i.path.ends_with("a.rs")).unwrap().source.clone();
        assert!(alpha_source.contains("alpha"), "初始内容含 alpha");

        // 修改 a.rs 内容（新函数 alpha_v2）
        std::fs::write(root.path().join("src").join("a.rs"), "pub fn alpha_v2() {}\n").unwrap();
        let second = scan_and_parse_cached_at(&root, &cp, &std::collections::HashSet::new()).unwrap().insights;
        let alpha2_source = second.iter().find(|i| i.path.ends_with("a.rs")).unwrap().source.clone();
        assert!(
            alpha2_source.contains("alpha_v2") && !alpha2_source.contains("alpha()"),
            "指纹变化后应重解析出新内容, 实际: {alpha2_source}"
        );
        // b.rs 未变化：缓存命中（复用路径——source 仍为初始内容）
        let beta_source = second.iter().find(|i| i.path.ends_with("b.rs")).unwrap().source.clone();
        assert!(beta_source.contains("beta"), "未变更文件应正常复用");

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// 缓存损坏重建：缓存文件写入垃圾后扫描仍返回正确结果（warn + 全量重建）
    #[test]
    fn test_cached_scan_rebuilds_on_corrupt_cache() {
        let root = temp_project("corrupt");
        let cp = Some(cache_path(&root));
        // 先正常扫一次（写缓存），再破坏缓存
        scan_and_parse_cached_at(&root, &cp, &std::collections::HashSet::new()).unwrap();
        std::fs::write(cache_path(&root), "{ 垃圾内容").unwrap();

        let insights = scan_and_parse_cached_at(&root, &cp, &std::collections::HashSet::new()).unwrap().insights;
        assert_eq!(insights.len(), 2, "损坏缓存应触发全量重建而非失败");
        assert!(insights.iter().any(|i| i.source.contains("alpha")));

        let _ = std::fs::remove_dir_all(root.path());
    }

    /// 缓存路径为 None（全量模式）时不读写缓存文件
    #[test]
    fn test_cached_scan_without_cache_path() {
        let root = temp_project("nocache");
        let insights = scan_and_parse_cached_at(&root, &None, &std::collections::HashSet::new()).unwrap().insights;
        assert_eq!(insights.len(), 2);
        assert!(!root.path().join(".state").exists(), "无缓存路径时不应创建 .state 目录");
        let _ = std::fs::remove_dir_all(root.path());
    }

    /// changed_files 强制重解析：变更集内的文件即使指纹一致也重解析
    ///（watch 语义：事件路径是变更的直接证据，不受指纹缓存影响）
    #[test]
    fn test_cached_scan_forced_reparse_by_changed_set() {
        let root = temp_project("forced");
        let cp = Some(cache_path(&root));
        let _ = scan_and_parse_cached_at(&root, &cp, &std::collections::HashSet::new()).unwrap();

        let mut changed = std::collections::HashSet::new();
        changed.insert(PathBuf::from("src/a.rs"));
        let insights = scan_and_parse_cached_at(&root, &cp, &changed).unwrap().insights;
        assert_eq!(insights.len(), 2, "强制重解析不改变结果集合");
        let _ = std::fs::remove_dir_all(root.path());
    }

    /// B5：解析失败文件计数——扫描范围内存在非 UTF-8 .rs 文件时，
    /// files_failed 应准确计数（此前失败仅日志可见，统计无法反映）
    #[test]
    fn test_scan_counts_failed_files() {
        let dir = std::env::temp_dir().join(format!("repo_wiki_failed_cnt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        // 正常文件
        std::fs::write(dir.join("src").join("ok.rs"), "pub fn ok() {}\n").unwrap();
        // 非法 UTF-8 文件（read_to_string 失败 → files_failed 计数）
        std::fs::write(dir.join("src").join("bad.rs"), [0xFFu8, 0xFE, 0x00]).unwrap();
        // 非 .rs 文件（无处理器，不计入失败——扫描范围外）
        std::fs::write(dir.join("src").join("notes.txt"), "text").unwrap();

        let root = ProjectRoot::new(dir.clone());
        let out = scan_and_parse_at(&root).unwrap();

        assert_eq!(out.insights.len(), 1, "只有正常文件被解析");
        assert_eq!(out.files_failed, 1, "非 UTF-8 文件应计数为失败");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
