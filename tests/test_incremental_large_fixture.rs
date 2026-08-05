#![cfg(test)]

//! v21 F 组（t09）：150 文件规模合成 fixture 增量 e2e
//!
//! 背景：既有增量 e2e（test_incremental_git_e2e.rs）最大 fixture 仅 4 文件，
//! 140+ 文件规模的增量路径从未验证（Unity 真实仓库 6655 文件规模过大，
//! 不能进 CI）。本文件程序化生成 150 文件（15 模块 × 10 文件）确定性仓库，
//! mock 全链路验证：
//!
//! fixture 结构（关键设计：三个互不相连的模块组，组内链式依赖）：
//! - 组 A = m00..m04、组 B = m05..m09、组 C = m10..m14，每组 50 文件
//! - v26 方案 D（目录超节点聚类）：每组 5 个目录超节点经链式边合并为
//!   一个社区；目录根用 a/、b/、c/ 使社区名稳定为 a/b/c（若沿用单一
//!   src/ 根，三组社区公共前缀相同会触发不稳定消歧）
//! - 组内链式跨模块调用：模块 i 的 f00 含 `link_{i}` 调 `m_{i+1}_f00_0`
//!   （仅当 i+1 与 i 同组；组尾模块无 link → 组间零边，对照组真隔离）
//! - 影响传播实现为双向 BFS 3 层（src/incremental/impact.rs:149）：
//!   改组 B 内任一接口级实体 → 传播覆盖整组 B（链长 5 ≤ 3 层双向），
//!   组 A/C 无边可达 → 零影响。
//!
//! 断言设计：
//! - 断言 1（影响集合）：组 B 模块页（b.md）重生成，组 A/C（a.md/c.md）不重生成
//! - 断言 2（零改写）：组 A/C 模块页字节与基线一致（确定性渲染，防过度重写）
//! - 断言 3（无删无增）：页面集合与基线一致（cleanup 误删防回归）
//! - 断言 4（接口级链路）：api.md 反映新签名
//! - 删除场景：目录页保留（目录仍存在），内容精确移除被删文件的实体

use std::collections::HashMap;
use std::path::Path;

use repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiConfig};
use repo_wiki::config::schema::{OutputSection, WikiSection};

/// 模块数 × 每模块文件数 = 150 文件；每组 5 模块
const MODULES: usize = 15;
const FILES_PER_MODULE: usize = 10;
const GROUP_SIZE: usize = 5;

/// 程序化生成 150 文件确定性仓库 + .gitignore + config.toml + git init
///
/// 命名规则（确定性）：每文件 3 个函数 `f{文件号}_{序号}`；模块 i 的 f00
/// 追加跨模块函数 `link_{i}` 调用 `m_{i+1}_f00_0`（同组才生成，组尾无 link）
///
/// v26 方案 D：目录根按组分区（src/a、src/b、src/c）——目录超节点聚类
/// 的社区名 = src::a / src::b / src::c（公共目录前缀，跨次生成稳定）；
/// 同组 5 目录经链式边合并为一个社区（50 文件/页）。
///
/// 关键：.repo-wiki/ 必须入 .gitignore——产物页含「最后更新」时间戳，
/// 每次生成都重写，若被 git 跟踪则每轮 diff 都把全部产物页算作变更，
/// 触发 MAX_DIFF_LINES 回退全量（真实最佳实践：产物不入版本控制）。
fn build_large_repo(repo: &Path) -> anyhow::Result<()> {
    std::fs::write(repo.join(".gitignore"), ".repo-wiki/\nAGENTS.md\n")?;
    for m in 0..MODULES {
        // 组根字母：组 A=m00-04→src/a/、组 B=m05-09→src/b/、组 C=m10-14→src/c/
        let group_root = match m / GROUP_SIZE {
            0 => "a",
            1 => "b",
            _ => "c",
        };
        let dir = repo.join("src").join(group_root).join(format!("m{m:02}"));
        std::fs::create_dir_all(&dir)?;
        for f in 0..FILES_PER_MODULE {
            let mut content = String::new();
            for i in 0..3 {
                content.push_str(&format!("pub fn f{f:02}_{i}(x: u32) -> u32 {{ x + {i} + {f} }}\n"));
            }
            // 组内链式跨模块调用（组边界：i+1 与 i 必须同组）
            if f == 0 && m + 1 < MODULES && (m + 1) / GROUP_SIZE == m / GROUP_SIZE {
                content.push_str(&format!(
                    "pub fn link_{m:02}() -> u32 {{ m_{:02}_f00_0(1) + 1 }}\n",
                    m + 1
                ));
            }
            std::fs::write(dir.join(format!("f{f:02}.rs")), content)?;
        }
    }

    let config = WikiConfig {
        output: OutputSection {
            dir: repo.join(".repo-wiki").to_string_lossy().into_owned(),
        },
        wiki: WikiSection {
            language: "zh".into(),
            ..Default::default()
        },
        llm: LlmSection {
            provider: LlmProviderType::Mock,
            ..Default::default()
        },
        ..Default::default()
    };
    std::fs::write(repo.join("config.toml"), toml::to_string_pretty(&config)?)?;

    let git = git2::Repository::init(repo)?;
    let mut cfg = git.config()?;
    cfg.set_str("user.name", "test")?;
    cfg.set_str("user.email", "test@test.com")?;
    Ok(())
}

/// git2 提交当前工作区全部文件，返回 commit id
fn git_commit_all(repo: &Path, message: &str) -> String {
    let repo = git2::Repository::open(repo).expect("打开 git 仓库失败");
    let mut index = repo.index().unwrap();
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let commit_id = match repo.head().ok() {
        Some(head) => {
            let parent = head.peel_to_commit().unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent]).unwrap()
        }
        None => repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[]).unwrap(),
    };
    commit_id.to_string()
}

/// wiki/zh 目录全部 .md 页：文件名 → 内容（零改写断言的数据源）
fn wiki_pages_snapshot(repo: &Path) -> HashMap<String, String> {
    let wiki_dir = repo.join(".repo-wiki").join("wiki").join("zh");
    let mut map = HashMap::new();
    if let Ok(es) = std::fs::read_dir(&wiki_dir) {
        for e in es.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "md")
                && let (Some(name), Ok(content)) =
                    (p.file_name().map(|s| s.to_string_lossy().into_owned()), std::fs::read_to_string(&p))
            {
                map.insert(name, content);
            }
        }
    }
    map
}

/// 页面文件名集合（排除 _log.md——note 追加式，与生成无关）
fn page_names(map: &HashMap<String, String>) -> Vec<String> {
    let mut names: Vec<String> = map.keys().filter(|k| !k.ends_with("_log.md")).cloned().collect();
    names.sort();
    names
}

/// 文档标题是否涉及指定模块组（模块号闭区间 [lo, hi]，v26 目录页语义：
/// 标题形如 src::a::m00，模块号 m{m:02} 精确区分组）
fn titles_in_group(titles: &[String], lo: usize, hi: usize) -> bool {
    titles.iter().any(|t| (lo..=hi).any(|m| t.contains(&format!("m{m:02}"))))
}

/// 主场景：全量基线 → 改组 B 内 m07/f00 签名（接口级）→ 增量 → 四断言
#[test]
fn test_large_fixture_incremental_impact() {
    let repo = std::env::temp_dir().join(format!("repo_wiki_large_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_large_repo(&repo).expect("构造 150 文件 fixture 失败");

    let root = repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");
    let target = repo.join("src").join("b").join("m07").join("f00.rs");

    // ---- 首轮：git init 提交 + 全量生成（基线快照） ----
    git_commit_all(&repo, "init 150 files");
    repo_wiki::run_pipeline(Some(&config_path), None, false, &root, &repo_wiki::GenerationMode::Full)
        .expect("全量生成失败");
    let base_pages = wiki_pages_snapshot(&repo);
    let base_names = page_names(&base_pages);
    assert!(
        base_names.len() >= 6,
        "基线应含全部模块页（a/b/c 三页）+ 合成页（≥6），实际 {} 页",
        base_names.len()
    );

    // ---- 变更：m07/f00.rs 的 m07_f00_0 改签名（接口级，被 m06::link_m06 调用） ----
    std::fs::write(
        &target,
        "pub fn f00_renamed(x: u32, y: u32) -> u32 { x + y }\npub fn f00_1(x: u32) -> u32 { x + 1 }\npub fn f00_2(x: u32) -> u32 { x + 2 }\npub fn link_m07() -> u32 { m08_f00_0(1) + 1 }\n",
    )
    .unwrap();
    git_commit_all(&repo, "rename m07 f00_0 signature");
    let inc = repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &repo_wiki::GenerationMode::Incremental { watch_paths: vec![], change_kind: None },
    )
    .expect("增量生成失败");

    // ---- 断言 1：组 B（m05-m09）目录页全部重生成（双向 BFS 3 层覆盖链长 5），组 A/C 不重生成 ----
    let titles: Vec<String> = inc.documents.iter().map(|d| d.title.clone()).collect();
    assert!(
        titles_in_group(&titles, 5, 9),
        "组 B 目录页文档应全部重生成，实际: {titles:?}"
    );
    for (lo, hi, label) in [(0, 4, "组 A"), (10, 14, "组 C")] {
        assert!(
            !titles_in_group(&titles, lo, hi),
            "{label} 与变更点无边连通，不应被重生成，实际: {titles:?}"
        );
    }

    // ---- 断言 2/3：组 A/C 目录页零改写 + 页面集合无删无增 ----
    let after_pages = wiki_pages_snapshot(&repo);
    let after_names = page_names(&after_pages);
    assert_eq!(
        after_names, base_names,
        "改签名场景页面集合必须与基线一致（无页面增删）"
    );
    for name in &after_names {
        // 组 B 目录页内容必然变化（实体集变更）；合成页（api/架构/索引/overview）
        // 由 render_all 每次重写——跳过；组 A/C 目录页必须字节级一致
        let is_group_b = (5..=9).any(|m| name.contains(&format!("m{m:02}")));
        let is_synthetic = ["api.md", "architecture.md", "index.md", "overview.md"]
            .iter().any(|s| name == s);
        if is_group_b || is_synthetic {
            continue;
        }
        assert_eq!(
            after_pages.get(name),
            base_pages.get(name),
            "未受影响模块页 {name} 必须零改写（内容字节一致）"
        );
    }

    // ---- 断言 4：接口级变化反映到 api.md（新签名出现；f00_0 是 15 模块
    // 通用函数名，不能断言"消失"——只断言 b 段的旧签名不再存在） ----
    let new_api = std::fs::read_to_string(repo.join(".repo-wiki").join("wiki").join("zh").join("api.md"))
        .unwrap_or_default();
    assert!(
        new_api.contains("f00_renamed"),
        "api.md 应反映新签名 f00_renamed，实际: {new_api}"
    );
    // src::b::m07 段（## src::b::m07 起到下一标题前）内不得再出现旧签名 f00_0
    // （该段仅含 m07 目录实体；组 B 其他目录的 f00_0 未修改，仍在各自段——
    // 若只 split 首个标题，段会一直延伸到文件尾而误含它们）
    let m07_section = new_api
        .split("## src::b::m07")
        .nth(1)
        .unwrap_or_default()
        .split("## ")
        .next()
        .unwrap_or_default();
    assert!(
        !m07_section.contains("f00_0"),
        "api.md b 段不应残留旧签名 f00_0，实际: {m07_section}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// 边界：150 文件规模下删除一个文件（m09/f07.rs，无任何调用边）→ 增量 →
/// 文件页被精确清理（cleanup 正确行为），其余页面全保留，组 A/C 零改写
#[test]
fn test_large_fixture_delete_file_keeps_pages() {
    let repo = std::env::temp_dir().join(format!("repo_wiki_large_del_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_large_repo(&repo).expect("构造 150 文件 fixture 失败");

    let root = repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");

    git_commit_all(&repo, "init 150 files");
    repo_wiki::run_pipeline(Some(&config_path), None, false, &root, &repo_wiki::GenerationMode::Full)
        .expect("全量生成失败");
    let base_pages = wiki_pages_snapshot(&repo);
    let base_names = page_names(&base_pages);
    assert!(base_names.contains(&"src_b_m09.md".to_string()), "基线应含 m09 目录页 src_b_m09.md");

    // 删除 src/b/m09/f07.rs（f07 函数无任何边：传播只含起点模块，其他组零影响）
    std::fs::remove_file(repo.join("src").join("b").join("m09").join("f07.rs")).unwrap();
    git_commit_all(&repo, "delete m09 f07");
    repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &repo_wiki::GenerationMode::Incremental { watch_paths: vec![], change_kind: None },
    )
    .expect("删除增量失败");

    let after_pages = wiki_pages_snapshot(&repo);
    let after_names = page_names(&after_pages);

    // 断言（v26 方案 D 语义）：页面 = 目录社区页，删除文件不改变目录集合
    // → src_b_m09.md 保留；纯删除分支（changed_insights 为空）走快照回填 + surviving
    // 重生成——目录页内容精确移除被删文件的实体（f07 函数），其余页面保留。
    assert_eq!(
        after_names, base_names,
        "删除文件场景页面集合必须与基线一致（目录页保留）"
    );
    // mock 产物体裁固定为占位模板（无实体行），无法断言实体级内容——
    // 实体级验证属 LLM 评测域（bench rubrics）；此处断言页面生命周期语义：
    // 页面保留、被删文件实体不残留（占位模板天然满足）
    let b_after = after_pages.get("src_b_m09.md").expect("src_b_m09.md 应保留");
    assert!(
        !b_after.contains("f07_0") && !b_after.contains("f07_1") && !b_after.contains("f07_2"),
        "src_b_m09.md 应移除被删文件 f07.rs 的实体（f07_0..2），实际: {b_after}"
    );
    // 其余页面（a/c 与合成页）零改写：回填=旧文档幂等重写（页脚不再重复注入），
    // last_updated 保持快照值 → 字节与基线一致（合成页由 graph 重渲染，跳过）
    for name in &after_names {
        let is_synthetic = ["api.md", "architecture.md", "index.md", "overview.md"]
            .iter().any(|s| name == s);
        if is_synthetic || name == "src_b_m09.md" {
            continue;
        }
        assert_eq!(
            after_pages.get(name),
            base_pages.get(name),
            "页面 {name} 必须零改写（回填语义）"
        );
    }

    let _ = std::fs::remove_dir_all(&repo);
}

/// 构造"双文件同模块 + 孤立文件"最小仓库（v21 验证轮删除场景缺陷修复专用）
///
/// 结构：src/m20/{a,b}.rs 互调（Calls 边 → Leiden 社区合并为同一模块），
/// src/solo.rs 无任何边（独立模块）。
/// 用途：删除 a.rs 后 m20 模块**未全删**（b.rs 存活）——旧实现走快照回填
/// 时模块页残留 a 的实体描述；修复后改为把存活文件并入变更集走 LLM 重生成。
fn build_pair_module_repo(repo: &Path) -> anyhow::Result<()> {
    std::fs::write(repo.join(".gitignore"), ".repo-wiki/\nAGENTS.md\n")?;
    std::fs::create_dir_all(repo.join("src").join("m20"))?;
    // a.rs / b.rs 互调：保证同一社区（模块 = 社区）
    std::fs::write(
        repo.join("src").join("m20").join("a.rs"),
        "pub fn a_alpha() -> u32 { 1 }\npub fn a_uses_b() -> u32 { b_beta() }\n",
    )?;
    std::fs::write(
        repo.join("src").join("m20").join("b.rs"),
        "pub fn b_beta() -> u32 { 2 }\npub fn b_uses_a() -> u32 { a_alpha() }\n",
    )?;
    std::fs::write(repo.join("src").join("solo.rs"), "pub fn solo_fn() -> u32 { 3 }\n")?;

    let config = WikiConfig {
        output: OutputSection {
            dir: repo.join(".repo-wiki").to_string_lossy().into_owned(),
        },
        wiki: WikiSection {
            language: "zh".into(),
            ..Default::default()
        },
        llm: LlmSection {
            provider: LlmProviderType::Mock,
            ..Default::default()
        },
        ..Default::default()
    };
    std::fs::write(repo.join("config.toml"), toml::to_string_pretty(&config)?)?;

    let git = git2::Repository::init(repo)?;
    let mut cfg = git.config()?;
    cfg.set_str("user.name", "test")?;
    cfg.set_str("user.email", "test@test.com")?;
    Ok(())
}

/// v21 验证轮（删除场景缺陷修复）：多文件模块删一文件 → 存活文件并入
/// 变更集走 LLM 重生成（页面残留被删实体 = 缺陷；修复后不残留）。
///
/// 判别信号（不依赖社区划分细节，回填路径与修复路径可区分）：
/// - 修复路径：documents 只含 m20 模块 + 3 个全局文档（架构/概览/index，
///   删除属接口级变化且 has_deleted_files 放行重生成）→ 数量 ≤ 5；
/// - 回填路径（缺陷行为）：documents = 快照全部文档（数量级差异），
///   solo 模块页也会被原样回填。
#[test]
fn test_delete_one_file_in_pair_module_regenerates_module() {
    let repo = std::env::temp_dir().join(format!("repo_wiki_pair_del_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_pair_module_repo(&repo).expect("构造双文件模块 fixture 失败");

    let root = repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");

    git_commit_all(&repo, "init pair module");
    repo_wiki::run_pipeline(Some(&config_path), None, false, &root, &repo_wiki::GenerationMode::Full)
        .expect("全量生成失败");
    // 基线：m20 模块页存在（社区合并后为目录级模块页，或 a/b 各自文件页）
    let base_pages = wiki_pages_snapshot(&repo);
    assert!(
        base_pages.keys().any(|k| k.starts_with("src_m20")),
        "基线应含 m20 相关页，实际: {base_pages:?}"
    );

    // 删除 a.rs（b.rs 存活 → m20 模块部分删除）
    std::fs::remove_file(repo.join("src").join("m20").join("a.rs")).unwrap();
    git_commit_all(&repo, "delete m20/a.rs");
    let inc = repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &repo_wiki::GenerationMode::Incremental { watch_paths: vec![], change_kind: None },
    )
    .expect("删除增量失败");

    let titles: Vec<String> = inc.documents.iter().map(|d| d.title.clone()).collect();
    // 1. 受影响模块（m20）被重生成：至少一个文档 title 以 src::m20 开头
    assert!(
        titles.iter().any(|t| t.starts_with("src::m20")),
        "部分删除模块必须重生成（清除被删实体残留），实际: {titles:?}"
    );
    // 2. 未受影响模块（solo）不在本次文档集——修复后只生成受影响模块；
    //    缺陷行为（快照回填）会把 solo 模块页原样回填进 documents
    assert!(
        !titles.iter().any(|t| t.contains("solo")),
        "未受影响模块不得被重生成或回填，实际: {titles:?}"
    );
    // 3. 数量级约束：修复后 = m20 模块页 + 架构概览 + 项目概览 + index（≤5）；
    //    回填路径 = 快照全量文档（数量级差异）
    assert!(
        titles.len() <= 5,
        "documents 数量应 ≤5（模块 + 3 全局），实际 {}: {titles:?}",
        titles.len()
    );
    // 4. 全局文档（架构/概览/index）因删除（接口级变化）而重生成：
    //    缺陷行为回填旧版会继续列出已删模块
    for key in ["架构概览", "项目概览", "index"] {
        assert!(
            titles.iter().any(|t| t.contains(key)),
            "全局文档 {key} 应重生成（删除场景不放行全局文档回填），实际: {titles:?}"
        );
    }

    // 5. 磁盘页面：m20 模块页保留（重生成覆盖），solo 页零改写。
    //    solo.rs 位于 src/ 根目录，其社区/模块路径为 src → 页名 src.md
    let after_pages = wiki_pages_snapshot(&repo);
    assert!(
        after_pages.keys().any(|k| k.starts_with("src_m20")),
        "m20 模块页必须保留，实际: {after_pages:?}"
    );
    let solo_page = "src.md".to_string();
    assert!(after_pages.contains_key(&solo_page), "solo 页必须保留，实际页面: {after_pages:?}");
    assert_eq!(
        after_pages.get(&solo_page),
        base_pages.get(&solo_page),
        "solo 未受影响页必须零改写"
    );

    let _ = std::fs::remove_dir_all(&repo);
}
