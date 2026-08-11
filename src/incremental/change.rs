//! 实体级代码变化分类（演进计划 T2.1）
//!
//! 把文件级 diff（[`GitDiffResult`]）细化为实体级变化分类。
//! 方法：对 modified 文件从 from_commit 读取旧内容（git2），用同一
//! ParserRegistry 重解析出旧实体集，与当前工作区实体集（FileInsight）
//! 按 name 对比判定变化类型。
//!
//! 分类语义（驱动 T2.2 语义传播）：
//! - **新增/删除/签名变更** = 接口级变化：影响调用方，需向依赖方传播；
//! - **正文变化**（BodyChanged）= 实现级变化：只影响本模块产物。
//!
//! 边界：非 UTF-8 旧文件、无对应解析器的文件、from_commit 中不存在的
//! 文件，均安全降级（视为无旧实体或空变化），不中断增量流程。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::ingest::parser::{Entity, FileInsight, ParserRegistry};

use super::diff::GitDiffResult;

/// 实体变化类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityChangeKind {
    /// 新增实体（接口级）
    Added,
    /// 实体被删除（接口级）
    Removed,
    /// 实体签名变化（接口级：需向依赖方传播）
    SignatureChanged,
    /// 实体实现变化（函数体修改，仅影响本模块）
    BodyChanged,
}

/// 单个实体的变化记录
#[derive(Debug, Clone)]
pub struct EntityChange {
    pub file: PathBuf,
    pub entity_name: String,
    pub kind: EntityChangeKind,
    /// 变化前位置（新增时为 None）
    pub old_range: Option<(usize, usize)>,
    /// 变化后位置（删除时为 None）
    pub new_range: Option<(usize, usize)>,
}

/// 实体级变化集合
#[derive(Debug, Clone, Default)]
pub struct EntityChangeSet {
    pub changes: Vec<EntityChange>,
}

impl EntityChangeSet {
    /// 是否包含接口级变化（新增/删除/签名变更）
    pub fn has_interface_change(&self) -> bool {
        self.changes.iter().any(|c| match c.kind {
            EntityChangeKind::Added
            | EntityChangeKind::Removed
            | EntityChangeKind::SignatureChanged => true,
            EntityChangeKind::BodyChanged => false,
        })
    }
}

/// 计算「无实体变更」文件集合（v23 A1）
///
/// 判据：文件在 changed_files（git diff 报告有变化）且仍存在于磁盘，但
/// `entity_changes` 中没有任何该文件的记录（compare_entities 的三元组全等
/// 判定跳过）——变化只可能发生在注释/空白/换行符等不产出实体的文本上。
/// 这类文件不触发模块重生成：模块页内容与 api.md 行号引用均不受影响。
///
/// 调用方必须用它**同时**剔除两类输入，否则语义不一致：
/// - 影响传播的起点集合（mod.rs）：无实体变更文件作为起点会走「仅本模块」
///   分支，把本模块捞入 affected_modules → 模块页仍被重生成；
/// - 生成过滤（generate/mod.rs）：changed_insights 过滤条件。
///
/// 删除文件不在此集合（磁盘不存在，判据第一项排除），快照回填逻辑不受影响。
pub fn no_entity_change_files(
    changed_files: &[PathBuf],
    entity_changes: &EntityChangeSet,
    root: &crate::project::ProjectRoot,
) -> std::collections::HashSet<PathBuf> {
    // 空变化集 = 无分类信息（非 git 仓库/无上次提交时 classify 返回
    // 默认空集，见 classify_entity_changes_at:107），不是"确无实体变化"。
    // 此时剔除会把起点全清空，FileWatch 增量在非 git 仓库退化为 0 模块
    // （回归：test_watch_e2e 修改文件后 api.md 永不更新）。与
    // classification_failed 同语义：无法分类时保守保留全部起点。
    if entity_changes.changes.is_empty() {
        return std::collections::HashSet::new();
    }
    changed_files
        .iter()
        .filter(|f| {
            root.path().join(f).exists()
                && !entity_changes
                    .changes
                    .iter()
                    .any(|c| c.file.as_path() == f.as_path())
        })
        .cloned()
        .collect()
}

/// 在指定项目根下对 Git diff 做实体级变化分类
///
/// git 仓库定位基准显式注入（与 git 仓库定位同源，root 由
/// run_incremental_update_at 统一传入）：不再依赖进程 cwd。
pub fn classify_entity_changes_at(
    root: &crate::project::ProjectRoot,
    diff: &GitDiffResult,
    current_insights: &[FileInsight],
) -> Result<EntityChangeSet> {
    if diff.from_commit.is_empty() {
        // 无上次生成记录（首次生成）时无法读旧内容，视为无实体变化
        return Ok(EntityChangeSet::default());
    }
    let repo = git2::Repository::open(root.path())
        .with_context(|| "实体级变化分类需要 Git 仓库")?;
    // commit/tree 只解析一次（避免每个文件重复 find_commit 的 ODB 开销）
    let from_commit = repo.find_commit(git2::Oid::from_str(&diff.from_commit)?)?;
    let from_tree = from_commit.tree()?;
    let registry = ParserRegistry::new();

    // 当前工作区实体：路径 → 实体列表（键归一化：git diff 路径正斜杠，
    // insight 路径在 Windows 上是反斜杠，统一为 "/" 才能匹配；且 insight
    // 路径是绝对路径，须先相对化（strip root），否则与 diff.modified 的
    // 相对路径永远不匹配→旧实体有、新实体空→误判 Removed（接口级双向
    // 传播误伤依赖方，见 test_incremental_git_e2e 场景 A 回归）
    let current: HashMap<String, Vec<Entity>> = current_insights
        .iter()
        .map(|i| {
            let rel = std::path::Path::new(&i.path)
                .strip_prefix(root.path())
                .unwrap_or(std::path::Path::new(&i.path));
            (super::norm_sep(&rel.to_string_lossy()), i.entities.clone())
        })
        .collect();

    let mut set = EntityChangeSet::default();

    // modified：新旧实体集对比
    for path in &diff.modified {
        let old_entities = read_old_entities(&repo, &from_tree, path, &registry)?;
        let new_entities = current
            .get(&super::norm_sep(&path.to_string_lossy()))
            .cloned()
            .unwrap_or_default();
        compare_entities(&mut set, path, &old_entities, &new_entities);
    }
    // added：全部实体视为新增
    for path in &diff.added {
        if let Some(ents) = current.get(&super::norm_sep(&path.to_string_lossy())) {
            for e in ents {
                set.changes.push(EntityChange {
                    file: path.clone(),
                    entity_name: e.name.clone(),
                    kind: EntityChangeKind::Added,
                    old_range: None,
                    new_range: Some((e.line_start, e.line_end)),
                });
            }
        }
    }
    // deleted：从旧内容解析全部实体视为删除
    for path in &diff.deleted {
        for e in read_old_entities(&repo, &from_tree, path, &registry)? {
            set.changes.push(EntityChange {
                file: path.clone(),
                entity_name: e.name.clone(),
                kind: EntityChangeKind::Removed,
                old_range: Some((e.line_start, e.line_end)),
                new_range: None,
            });
        }
    }
    Ok(set)
}

/// 对比同一文件的新旧实体集，产出逐实体变化
///
/// 同名实体按签名归一化集合比较：签名集合不变 → BodyChanged；
/// 签名集合变化 → SignatureChanged；数量变化 → Added/Removed。
fn compare_entities(
    set: &mut EntityChangeSet,
    path: &Path,
    old: &[Entity],
    new: &[Entity],
) {
    let old_by_name: HashMap<&str, Vec<&Entity>> = group_by_name(old);
    let new_by_name: HashMap<&str, Vec<&Entity>> = group_by_name(new);

    // 新增实体
    for (name, entries) in &new_by_name {
        if !old_by_name.contains_key(*name) {
            for e in entries {
                set.changes.push(EntityChange {
                    file: path.to_path_buf(),
                    entity_name: e.name.clone(),
                    kind: EntityChangeKind::Added,
                    old_range: None,
                    new_range: Some((e.line_start, e.line_end)),
                });
            }
        }
    }
    // 删除实体
    for (name, entries) in &old_by_name {
        if !new_by_name.contains_key(*name) {
            for e in entries {
                set.changes.push(EntityChange {
                    file: path.to_path_buf(),
                    entity_name: e.name.clone(),
                    kind: EntityChangeKind::Removed,
                    old_range: Some((e.line_start, e.line_end)),
                    new_range: None,
                });
            }
        }
    }
    // 同名实体：按签名集合比较判定接口/实现变化
    for (name, old_entries) in &old_by_name {
        if let Some(new_entries) = new_by_name.get(*name) {
            let old_sigs: Vec<String> = old_entries
                .iter()
                .map(|e| normalize_sig(e.signature.as_deref()))
                .collect();
            let new_sigs: Vec<String> = new_entries
                .iter()
                .map(|e| normalize_sig(e.signature.as_deref()))
                .collect();
            let kind = if old_sigs == new_sigs {
                EntityChangeKind::BodyChanged
            } else {
                EntityChangeKind::SignatureChanged
            };
            // 按位置顺序配对输出（同名多实体取并集逐条记录）
            for (old_e, new_e) in old_entries.iter().zip(new_entries.iter()) {
                // v23 A1 实体级分类精化：签名集合相等时进一步比较配对实体三元组
                // （起止行号 + 签名）。三元组全等 = 该实体的声明与位置均未变化
                // （变化只可能发生在注释/空白等不产实体的文本上），不记录任何
                // 变更——生成层据此把整文件判为「无实体变更」跳过重生成。
                // 任一元素不等（含 zip 顺序错位）→ 保守记录 BodyChanged。
                let unchanged = kind == EntityChangeKind::BodyChanged
                    && old_e.line_start == new_e.line_start
                    && old_e.line_end == new_e.line_end
                    && normalize_sig(old_e.signature.as_deref())
                        == normalize_sig(new_e.signature.as_deref());
                if unchanged {
                    continue;
                }
                set.changes.push(EntityChange {
                    file: path.to_path_buf(),
                    entity_name: (*name).to_string(),
                    kind,
                    old_range: Some((old_e.line_start, old_e.line_end)),
                    new_range: Some((new_e.line_start, new_e.line_end)),
                });
            }
        }
    }
}

/// 签名归一化：删除全部空白字符比较（容忍换行/多余空格/括号内空格）
fn normalize_sig(sig: Option<&str>) -> String {
    sig.unwrap_or("")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// 从 from_commit 读取文件旧内容并解析出实体集
///
/// 文件在 from_commit 中不存在（新文件被判定为 modified 的边界场景）
/// 返回空集；非 UTF-8 或无可解析器返回空集（降级不中断）。
fn read_old_entities(
    repo: &git2::Repository,
    from_tree: &git2::Tree,
    path: &Path,
    registry: &ParserRegistry,
) -> Result<Vec<Entity>> {
    let entry = match from_tree.get_path(path) {
        Ok(e) => e,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let obj = entry
        .to_object(repo)
        .with_context(|| format!("读取 {} 旧版本失败", path.display()))?;
    let blob = obj
        .into_blob()
        .map_err(|_| anyhow::anyhow!("{} 在 from_commit 中不是 blob", path.display()))?;
    let content = match std::str::from_utf8(blob.content()) {
        Ok(c) => c,
        // 二进制文件（图片/数据库/序列化产物等）无实体概念：降级为空集，
        // 不中断分类——否则仓库内任一被提交的二进制文件会让整个 classify
        // 失败回退保守传播（模块级误重生成）。与头部注释的降级语义一致。
        Err(_) => return Ok(Vec::new()),
    };
    Ok(match registry.get_for_file(path) {
        Some(parser) => parser.parse(content, path)?.entities,
        None => Vec::new(),
    })
}

/// 按实体名分组（保留同名多实体，如重载/多 impl 块）
fn group_by_name(entities: &[Entity]) -> HashMap<&str, Vec<&Entity>> {
    let mut map: HashMap<&str, Vec<&Entity>> = HashMap::new();
    for e in entities {
        map.entry(e.name.as_str()).or_default().push(e);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entity(name: &str, sig: &str, start: usize, end: usize) -> Entity {
        Entity {
            name: name.into(),
            kind: "fn".into(),
            line_start: start,
            line_end: end,
            doc_comment: None,
            signature: Some(sig.into()), visibility: None,
        }
    }

    #[test]
    fn test_compare_added_and_removed() {
        let mut set = EntityChangeSet::default();
        let old = vec![make_entity("gone", "fn gone()", 1, 2)];
        let new = vec![make_entity("fresh", "fn fresh()", 5, 6)];
        compare_entities(&mut set, Path::new("src/a.rs"), &old, &new);
        assert_eq!(set.changes.len(), 2);
        assert_eq!(set.changes[0].kind, EntityChangeKind::Added);
        assert_eq!(set.changes[1].kind, EntityChangeKind::Removed);
        assert!(set.has_interface_change());
    }

    #[test]
    fn test_compare_signature_changed() {
        let mut set = EntityChangeSet::default();
        let old = vec![make_entity("f", "fn f(a: i32)", 1, 3)];
        let new = vec![make_entity("f", "fn f(a: i32, b: i32)", 1, 4)];
        compare_entities(&mut set, Path::new("src/a.rs"), &old, &new);
        assert_eq!(set.changes.len(), 1);
        assert_eq!(set.changes[0].kind, EntityChangeKind::SignatureChanged);
        assert!(set.has_interface_change());
    }

    #[test]
    fn test_compare_body_changed_only() {
        let mut set = EntityChangeSet::default();
        // 签名一致、行号范围变化 = 正文修改
        let old = vec![make_entity("f", "fn f()", 1, 3)];
        let new = vec![make_entity("f", "fn f()", 1, 5)];
        compare_entities(&mut set, Path::new("src/a.rs"), &old, &new);
        assert_eq!(set.changes.len(), 1);
        assert_eq!(set.changes[0].kind, EntityChangeKind::BodyChanged);
        assert!(!set.has_interface_change());
    }

    #[test]
    fn test_normalize_sig_ignores_whitespace() {
        let a = "fn  f( a : i32 )";
        let b = "fn f(a: i32)";
        assert_eq!(normalize_sig(Some(a)), normalize_sig(Some(b)));
    }
}
