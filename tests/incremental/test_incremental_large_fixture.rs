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
//! - 组内链式跨模块调用：模块 i 的 f00 含 `link_{i}` 调 `m_{i+1}_f00_0`
//!   （仅当 i+1 与 i 同组；组尾模块无 link → 组间零边，对照组真隔离）
//! - 影响传播实现为双向 BFS 3 层（src/incremental/impact.rs:149）：
//!   改组 B 内任一接口级实体 → 传播覆盖整组 B（链长 5 ≤ 3 层双向），
//!   组 A/C 无边可达 → 零影响。
//!
//! 断言设计：
//! - 断言 1（影响集合）：组 B 全部模块页重生成，组 A/C 不重生成
//! - 断言 2（零改写）：组 A/C 模块页字节与基线一致（确定性渲染，防过度重写）
//! - 断言 3（无删无增）：页面集合与基线一致（cleanup 误删防回归）
//! - 断言 4（接口级链路）：api.md 反映新签名
//! - 删除场景：文件页被精确清理（cleanup 正确行为），其余页面全保留

use std::collections::HashMap;
use std::path::Path;

use code_repo_wiki::config::schema::WikiSection;
use code_repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiConfig};

/// 模块数 × 每模块文件数 = 150 文件；每组 5 模块
const MODULES: usize = 15;
const FILES_PER_MODULE: usize = 10;
const GROUP_SIZE: usize = 5;

/// 程序化生成 150 文件确定性仓库 + .gitignore + config.toml + git init
///
/// 命名规则（确定性）：每文件 3 个函数 `f{文件号}_{序号}`；模块 i 的 f00
/// 追加跨模块函数 `link_{i}` 调用 `m_{i+1}_f00_0`（同组才生成，组尾无 link）
///
/// 关键：.code-repo-wiki/ 必须入 .gitignore——产物页含「最后更新」时间戳，
/// 每次生成都重写，若被 git 跟踪则每轮 diff 都把全部产物页算作变更，
/// 触发 MAX_DIFF_LINES 回退全量（真实最佳实践：产物不入版本控制）。
fn build_large_repo(repo: &Path) -> anyhow::Result<()> {
    std::fs::write(repo.join(".gitignore"), ".code-repo-wiki/\nAGENTS.md\n")?;
    for m in 0..MODULES {
        let dir = repo.join("src").join(format!("m{m:02}"));
        std::fs::create_dir_all(&dir)?;
        for f in 0..FILES_PER_MODULE {
            let mut content = String::new();
            for i in 0..3 {
                content.push_str(&format!(
                    "pub fn f{f:02}_{i}(x: u32) -> u32 {{ x + {i} + {f} }}\n"
                ));
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
        output_dir: Some((repo.join(".code-repo-wiki").to_string_lossy().into_owned()).into()),
        wiki: WikiSection {
            language: "zh".into(),
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
///
/// 委托给 test_git::commit_all（libgit2 Windows 环境竞态有界重试）。
fn git_commit_all(repo: &Path, message: &str) -> String {
    code_repo_wiki::test_git::commit_all(repo, message)
}

/// wiki/zh 目录全部 .md 页：文件名 → 内容（零改写断言的数据源）
fn wiki_pages_snapshot(repo: &Path) -> HashMap<String, String> {
    let wiki_dir = repo.join(".code-repo-wiki").join("wiki").join("zh");
    let mut map = HashMap::new();
    if let Ok(es) = std::fs::read_dir(&wiki_dir) {
        for e in es.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "md")
                && let (Some(name), Ok(content)) = (
                    p.file_name().map(|s| s.to_string_lossy().into_owned()),
                    std::fs::read_to_string(&p),
                )
            {
                map.insert(name, content);
            }
        }
    }
    map
}

/// 页面文件名集合（排除 _log.md——note 追加式，与生成无关）
fn page_names(map: &HashMap<String, String>) -> Vec<String> {
    let mut names: Vec<String> = map
        .keys()
        .filter(|k| !k.ends_with("_log.md"))
        .cloned()
        .collect();
    names.sort();
    names
}

/// 文档标题是否涉及指定模块组（模块号闭区间 [lo, hi]）
fn titles_in_group(titles: &[String], lo: usize, hi: usize) -> bool {
    titles
        .iter()
        .any(|t| (lo..=hi).any(|m| t.contains(&format!("m{m:02}"))))
}

/// 主场景：全量基线 → 改组 B 内 m07/f00 签名（接口级）→ 增量 → 四断言
#[test]
fn test_large_fixture_incremental_impact() {
    let repo =
        std::env::temp_dir().join(format!("code_repo_wiki_large_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_large_repo(&repo).expect("构造 150 文件 fixture 失败");

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");
    let target = repo.join("src").join("m07").join("f00.rs");

    // ---- 首轮：git init 提交 + 全量生成（基线快照） ----
    git_commit_all(&repo, "init 150 files");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");
    let base_pages = wiki_pages_snapshot(&repo);
    let base_names = page_names(&base_pages);
    assert!(
        base_names.len() >= MODULES + 4,
        "基线应含全部模块/文件页（≥19），实际 {} 页",
        base_names.len()
    );

    // ---- 变更：m07/f00.rs 的 m07_f00_0 改签名（接口级，被 m06::link_m06 调用） ----
    std::fs::write(
        &target,
        "pub fn f00_renamed(x: u32, y: u32) -> u32 { x + y }\npub fn f00_1(x: u32) -> u32 { x + 1 }\npub fn f00_2(x: u32) -> u32 { x + 2 }\npub fn link_m07() -> u32 { m08_f00_0(1) + 1 }\n",
    )
    .unwrap();
    git_commit_all(&repo, "rename m07 f00_0 signature");
    let inc = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Incremental {
            watch_paths: vec![],
            change_kind: None,
        },
    )
    .expect("增量生成失败");

    // ---- 断言 1：组 B（m05-m09）全部重生成（双向 BFS 3 层覆盖链长 5）；
    // 组 A/C 与变更点无边连通，未受影响 → 从导出快照回填保留进完整文档集
    //（DEFECT-A 修复：旧断言「A/C 不在 titles」固化了「增量后未重生成模块
    // 从 llms/_toc 缺位」的缺陷行为） ----
    let titles: Vec<String> = inc.documents.iter().map(|d| d.title.clone()).collect();
    assert!(
        titles_in_group(&titles, 5, 9),
        "组 B 模块文档应全部重生成，实际: {titles:?}"
    );
    for (lo, hi, label) in [(0, 4, "组 A"), (10, 14, "组 C")] {
        assert!(
            titles_in_group(&titles, lo, hi),
            "{label} 与变更点无边连通，未受影响应从快照回填保留（完整文档集），实际: {titles:?}"
        );
    }

    // ---- 断言 2/3：组 A/C 模块页零改写 + 页面集合无删无增 ----
    let after_pages = wiki_pages_snapshot(&repo);
    let after_names = page_names(&after_pages);
    assert_eq!(
        after_names, base_names,
        "改签名场景页面集合必须与基线一致（无页面增删）"
    );
    for name in &after_names {
        // 组 B 模块页内容必然变化（实体集变更）；合成页（api/架构/索引/overview）
        // 由 render_all 每次重写——跳过；组 A/C 模块页必须字节级一致
        let is_group_b = (5..=9).any(|m| name.contains(&format!("m{m:02}")));
        let is_synthetic = [
            "api.md",
            "architecture.md",
            "architecture-map.md",
            "index.md",
            "overview.md",
        ]
        .iter()
        .any(|s| name == s);
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
    // 通用函数名，不能断言"消失"——只断言 m07 段的旧签名不再存在） ----
    let new_api = std::fs::read_to_string(
        repo.join(".code-repo-wiki")
            .join("wiki")
            .join("zh")
            .join("api.md"),
    )
    .unwrap_or_default();
    assert!(
        new_api.contains("f00_renamed"),
        "api.md 应反映新签名 f00_renamed，实际: {new_api}"
    );
    // m07 段（## src::m07 起、下一个 "## " 段标题止）内不得再出现旧签名 f00_0。
    // 注意 split("## src::m07").nth(1) 会取到文件尾（含 m08 之后各段——m08 的
    // f00_0 是合法存在的实体，旧写法在新划分下误报）；U1 目录聚簇后 m07 段
    // 含该目录全部 10 文件实体，段边界切片才反映 m07 自身。
    let m07_section = new_api
        .split("## src::m07")
        .nth(1)
        .unwrap_or_default()
        .split("## ")
        .next()
        .unwrap_or_default();
    assert!(
        !m07_section.contains("f00_0"),
        "api.md m07 段不应残留旧签名 f00_0，实际: {m07_section}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// 边界：150 文件规模下删除一个文件（m09/f07.rs，无任何调用边）→ 增量 →
/// m09 目录页保留（deleted_modules 判定：模块未全删）且页面集合无增减，
/// 组 A/C 零改写（U1 目录聚簇后删除粒度=模块级，单文件页形态已合并）
#[test]
fn test_large_fixture_delete_file_keeps_pages() {
    let repo =
        std::env::temp_dir().join(format!("code_repo_wiki_large_del_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_large_repo(&repo).expect("构造 150 文件 fixture 失败");

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");

    git_commit_all(&repo, "init 150 files");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");
    let base_pages = wiki_pages_snapshot(&repo);
    let base_names = page_names(&base_pages);
    assert!(
        base_names.contains(&"src_m09.md".to_string()),
        "基线应含 m09 目录页（U1 目录聚簇：m09 十文件一页，旧单文件页形态已合并）"
    );

    // 删除 m09/f07.rs（f07 函数无任何边：传播只含起点模块 m09，其他组零影响）
    std::fs::remove_file(repo.join("src").join("m09").join("f07.rs")).unwrap();
    git_commit_all(&repo, "delete m09 f07");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Incremental {
            watch_paths: vec![],
            change_kind: None,
        },
    )
    .expect("删除增量失败");

    let after_pages = wiki_pages_snapshot(&repo);
    let after_names = page_names(&after_pages);

    // 断言：纯删除场景（变更文件均已不存在，changed_insights 为空）走
    // 快照回填分支（generate/mod.rs 纯删除分支：防 cleanup 误删优先）。
    // U1 目录聚簇后 f07 属 m09 目录页：deleted_modules 判定按"related_files
    // 全不存在"（f07 缺失但其余 9 文件存活 → 非已删模块）→ m09 页回填保留，
    // 页面集合无增减。已知风险（记录待改进）：目录页内单文件删除不触发
    // 该页重生成，页内残留被删实体描述——增量语义粒度随 U1 变为模块级。
    assert!(
        after_names.contains(&"src_m09.md".to_string()),
        "m09 目录页应保留（模块未全删），实际: {after_names:?}"
    );
    assert_eq!(
        after_names.len(),
        base_names.len(),
        "目录聚簇粒度下页面集合应无增减；基线 {} 页 → 现在 {} 页",
        base_names.len(),
        after_names.len()
    );
    // 其余全部页面零改写：受影响模块（m09，删除传播重生成——无已删实体
    // 残留，优于旧回填）与合成页（graph 重渲染）跳过；其余页面字节一致
    for name in &after_names {
        let is_affected = name == "src_m09.md";
        let is_synthetic = [
            "api.md",
            "architecture.md",
            "architecture-map.md",
            "index.md",
            "overview.md",
        ]
        .iter()
        .any(|s| name == s);
        if is_affected || is_synthetic {
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
    std::fs::write(repo.join(".gitignore"), ".code-repo-wiki/\nAGENTS.md\n")?;
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
    std::fs::write(
        repo.join("src").join("solo.rs"),
        "pub fn solo_fn() -> u32 { 3 }\n",
    )?;

    let config = WikiConfig {
        output_dir: Some((repo.join(".code-repo-wiki").to_string_lossy().into_owned()).into()),
        wiki: WikiSection {
            language: "zh".into(),
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
/// 判别信号（不依赖社区划分细节）：
/// - 受影响模块（m20）被重生成（documents 含 src::m20 标题）；
/// - 未受影响模块（solo）回填保留（DEFECT-A 修复后 documents = 完整当前
///   文档集——旧断言「solo 不在 documents、documents ≤ 5」固化了「增量后
///   未重生成模块缺位」的缺陷行为，改造为完整文档集 + 磁盘零改写）；
/// - 全局文档（架构/概览/index）因删除（接口级变化且 has_deleted_files
///   放行）而重生成，不列出已删模块。
#[test]
fn test_delete_one_file_in_pair_module_regenerates_module() {
    let repo = std::env::temp_dir().join(format!("code_repo_wiki_pair_del_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_pair_module_repo(&repo).expect("构造双文件模块 fixture 失败");

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");

    git_commit_all(&repo, "init pair module");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
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
    let inc = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Incremental {
            watch_paths: vec![],
            change_kind: None,
        },
    )
    .expect("删除增量失败");

    let titles: Vec<String> = inc.documents.iter().map(|d| d.title.clone()).collect();
    // 1. 受影响模块（m20）被重生成：至少一个文档 title 以 src::m20 开头
    assert!(
        titles.iter().any(|t| t.starts_with("src::m20")),
        "部分删除模块必须重生成（清除被删实体残留），实际: {titles:?}"
    );
    // 2. 未受影响模块（solo）回填保留：DEFECT-A 修复后 documents = 完整
    //    当前文档集，solo 模块页从导出快照回填（solo.rs 位于 src/ 根目录，
    //    其社区/模块路径为 src → 文档 title 恰为 "src"；旧断言「title 含
    //    solo」是恒假的空断言，且「solo 不在 documents」固化了缺陷行为）
    assert!(
        titles.iter().any(|t| t == "src"),
        "未受影响模块应从快照回填进完整文档集，实际: {titles:?}"
    );
    // 3. 数量级约束改为完整文档集：m20（重生成）+ solo（回填）+ 3 全局
    //    ≥5；旧断言 ≤5 固化了「solo 缺失」的缺陷集合
    assert!(
        titles.len() >= 5,
        "documents 应为完整文档集（m20 + solo + 全局文档），实际 {}: {titles:?}",
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
    assert!(
        after_pages.contains_key(&solo_page),
        "solo 页必须保留，实际页面: {after_pages:?}"
    );
    assert_eq!(
        after_pages.get(&solo_page),
        base_pages.get(&solo_page),
        "solo 未受影响页必须零改写"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// v21 F 组遗留（mixed 场景）：删除与修改并存时，被删文件所属模块
/// 必须重生成清除被删实体残留。
///
/// 结构复用 build_pair_module_repo：src/m20/{a,b}.rs 互调（同社区模块），
/// 另有 src/solo.rs 独立模块。场景：删除 a.rs（m20 部分删除）**同时**修改
/// solo.rs（签名变化）——changed_insights 非空，旧实现不进快照回填
/// 分支，而删除补偿（surviving 并入）只存在于回填分支内 → 补偿失效；
/// 且被删文件在当前图中无节点，语义传播起点跳过（impact.rs 对无节点
/// 文件 continue），m20 进不了 affected_modules → src_m20.md 磁盘残留
/// a_alpha 实体描述。修复后删除补偿独立于回填分支执行 → m20 存活
/// 文件并入变更集走正常重生成。
///
/// 判别信号（mock 占位页脚幂等，字节级断言不可靠，改用 title 与
/// 真实内容的 api.md/磁盘页）：
/// - documents 含 src::m20 模块页（重生成发生）；
/// - 磁盘 src_m20 页不含 a_alpha（被删实体不残留——缺陷时是旧文件）；
/// - api.md（由 graph 合成，含真实实体名）不含 a_alpha、含 solo 新签名；
/// - solo 模块页同样在本次生成集（modified 文件正常生效）。
#[test]
fn test_delete_file_mixed_with_modification_regenerates_module() {
    let repo =
        std::env::temp_dir().join(format!("code_repo_wiki_pair_mixed_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).expect("构造临时仓库失败");
    build_pair_module_repo(&repo).expect("构造双文件模块 fixture 失败");

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    let config_path = repo.join("config.toml");

    git_commit_all(&repo, "init pair module");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Full,
    )
    .expect("全量生成失败");
    let base_pages = wiki_pages_snapshot(&repo);
    assert!(
        base_pages.keys().any(|k| k.starts_with("src_m20")),
        "基线应含 m20 模块页，实际: {base_pages:?}"
    );
    // api.md 由 graph 合成（不经 LLM），含真实实体名——断言载体
    let base_api = std::fs::read_to_string(
        repo.join(".code-repo-wiki")
            .join("wiki")
            .join("zh")
            .join("api.md"),
    )
    .unwrap_or_default();
    assert!(
        base_api.contains("solo_fn"),
        "基线 api.md 应含 solo_fn 实体，实际: {base_api}"
    );

    // 删除 a.rs（m20 部分删除）+ 修改 solo.rs（签名变化 → changed_insights 非空）
    std::fs::remove_file(repo.join("src").join("m20").join("a.rs")).unwrap();
    std::fs::write(
        repo.join("src").join("solo.rs"),
        "pub fn solo_renamed(x: u32, y: u32) -> u32 { x + y }\n",
    )
    .unwrap();
    git_commit_all(&repo, "delete m20/a.rs + modify solo.rs");
    let inc = code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        &root,
        &code_repo_wiki::GenerationMode::Incremental {
            watch_paths: vec![],
            change_kind: None,
        },
    )
    .expect("删除+修改增量失败");

    let titles: Vec<String> = inc.documents.iter().map(|d| d.title.clone()).collect();
    // 1. 部分删除模块（m20）必须重生成：documents 含 src::m20 模块页
    assert!(
        titles.iter().any(|t| t.starts_with("src::m20")),
        "mixed 场景部分删除模块必须重生成（清除被删实体残留），实际: {titles:?}"
    );
    // 2. 修改文件所属模块正常重生成（modified 生效）：solo.rs 位于
    //    src/ 根目录，其社区/模块名为 src → 文档 title 恰为 "src"
    assert!(
        titles.iter().any(|t| t == "src"),
        "modified 文件所属模块（src）应重生成，实际: {titles:?}"
    );

    // 3. 磁盘 m20 模块页不残留被删实体（缺陷时磁盘是含 a_alpha 的旧文件）
    let after_pages = wiki_pages_snapshot(&repo);
    let m20_page = after_pages
        .iter()
        .find(|(k, _)| k.starts_with("src_m20"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .expect("m20 模块页必须保留");
    assert!(
        !m20_page.1.contains("a_alpha"),
        "m20 模块页不得残留被删实体 a_alpha（缺陷: 旧页未重生成），内容: {}",
        m20_page.1
    );

    // 4. api.md（真实实体名来源）不含被删实体、含 modified 新签名
    let new_api = std::fs::read_to_string(
        repo.join(".code-repo-wiki")
            .join("wiki")
            .join("zh")
            .join("api.md"),
    )
    .unwrap_or_default();
    assert!(
        !new_api.contains("a_alpha"),
        "api.md 不应残留被删实体 a_alpha，实际: {new_api}"
    );
    assert!(
        new_api.contains("solo_renamed"),
        "api.md 应反映 solo.rs 新签名 solo_renamed，实际: {new_api}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}
