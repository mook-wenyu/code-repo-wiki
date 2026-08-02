pub mod scanner;
pub mod parser;

use anyhow::Result;
use crate::config::schema::WikiConfig;
use crate::project::ProjectRoot;
use parser::{FileInsight, ParserRegistry};


/// 在指定项目根下执行扫描和解析（全量解析：委托缓存版，传入空变更集）
///
/// 扫描根与路径相对化基准都取自 root，不再依赖进程 cwd——
/// 测试可在临时目录构造 ProjectRoot 验证扫描行为，watch 常驻进程
/// 的 cwd 漂移不再影响扫描范围。
pub fn scan_and_parse_at(root: &ProjectRoot, config: &WikiConfig) -> Result<Vec<FileInsight>> {
    scan_and_parse_cached_at(root, config, &None, &std::collections::HashSet::new())
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
    config: &WikiConfig,
    cache_path: &Option<std::path::PathBuf>,
    changed_files: &std::collections::HashSet<std::path::PathBuf>,
) -> Result<Vec<FileInsight>> {
    let scanner = scanner::Scanner::new(root.path(), &config.scope);
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
            }
        }
    }

    // 写回缓存（辅助产物：失败仅告警，下次扫描降级为空缓存全量重建）
    if let Some(path) = cache_path
        && let Err(e) = save_insights_cache(path, &cache)
    {
        tracing::warn!("解析缓存写入失败: {}", e);
    }

    tracing::info!(
        "扫描完成: 共 {} 个文件, 成功解析 {} 个（缓存复用 {} 个）",
        files.len(),
        insights.len(),
        reused
    );
    Ok(insights)
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
    std::fs::write(cache_path, serde_json::to_string_pretty(&list)?)?;
    Ok(())
}

/// 文件内容 SHA256 指纹（与 GenerationState::compute_file_fingerprint 同款算法，
/// 此处直接对已读入内容计算，避免二次读盘）
fn fingerprint_of(source: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    hex::encode(hasher.finalize())
}
