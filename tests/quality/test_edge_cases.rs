#![cfg(test)]

//! 边界/退化场景 e2e 测试（worker 补缺）
//!
//! 覆盖四处已识别的测试缺口：
//! 1. 空仓库（无源文件）bail "未找到任何源文件"（lib.rs 扫描阶段，无需 LLM）。
//! 2. cards/{lang}/ 目录被删后生成自动重建（render_all 的 create_dir_all）。
//! 3. 规约文件内容为空被跳过（collect_spec_files：空文件不纳入 →
//!    spec_files 为空 + 无 notes → 整卡不生成，不 panic）。
//! 4. wiki.language = "en" 时产物落 cards/en/（primary_language 单值路径）。
//!
//! fixture 模式与 tests/test_project_cards.rs 同源：临时 git 仓库 + config.toml
//! + mock provider（无网络）；除空仓库场景外均含 src/main.rs 作为源文件。

use std::path::Path;

use code_repo_wiki::config::schema::{LlmProviderType, LlmSection, WikiConfig, WikiSection};

/// 最小源文件（保证文件_insights 非空，避免触发空仓库 bail）
const MAIN_RS: &str = "pub fn main_fn() -> u32 { 42 }\n";

/// 构造临时 git 仓库：写入 config.toml（mock provider）+ 指定语言 + 可选源文件。
/// 返回仓库根路径。
fn build_repo(repo: &Path, lang: &str, write_main_rs: bool, agents_md: Option<&str>) {
    std::fs::create_dir_all(repo).expect("创建临时仓库失败");
    if write_main_rs {
        std::fs::create_dir_all(repo.join("src")).expect("创建 src 失败");
        std::fs::write(repo.join("src").join("main.rs"), MAIN_RS).unwrap();
    }
    if let Some(a) = agents_md {
        std::fs::write(repo.join("AGENTS.md"), a).unwrap();
    }

    let config = WikiConfig {
        output_dir: Some((repo.join(".code-repo-wiki").to_string_lossy().into_owned()).into()),
        wiki: WikiSection {
            language: lang.into(),
        },
        llm: LlmSection {
            provider: LlmProviderType::Mock,
            ..Default::default()
        },
        ..Default::default()
    };
    std::fs::write(
        repo.join("config.toml"),
        toml::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    let git = git2::Repository::init(repo).expect("git init 失败");
    let mut cfg = git.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
}

/// 全量生成；返回 Result（空仓库场景期望 Err 而非 panic）
fn full_generate_result(
    repo: &Path,
    root: &code_repo_wiki::project::ProjectRoot,
) -> anyhow::Result<code_repo_wiki::AnalysisResult> {
    let config_path = repo.join("config.toml");
    code_repo_wiki::run_pipeline(
        Some(&config_path),
        None,
        false,
        root,
        &code_repo_wiki::GenerationMode::Full,
    )
}

/// 全量生成；期望成功（失败即 panic）
fn full_generate(repo: &Path, root: &code_repo_wiki::project::ProjectRoot) {
    full_generate_result(repo, root).expect("全量生成失败");
}

/// 列出 cards/{lang}/ 目录下的卡片文件 basename（仅 .md，递归 project/ 子目录）
fn list_card_files(repo: &Path, lang: &str) -> Vec<String> {
    let cards_dir = repo.join(".code-repo-wiki").join("cards").join(lang);
    if !cards_dir.exists() {
        return Vec::new();
    }
    let mut names: Vec<String> = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().map(|x| x == "md").unwrap_or(false) {
                    out.push(path.file_name().unwrap().to_string_lossy().into_owned());
                }
            }
        }
    }
    walk(&cards_dir, &mut names);
    names.sort();
    names
}

/// 场景 1：空仓库（无任何源文件）→ 扫描阶段 bail "未找到任何源文件"。
///
/// 触发路径：run_pipeline → run_pipeline_with_config → scan_and_parse_at_with_scope
/// → file_insights.is_empty() → bail("未找到任何源文件")。此 bail 位于 LLM 调用
/// 之前（扫描阶段），故即使不提供可用的 LLM 也会触发；配置仍需有效加载。
/// config.toml 不属于源码扩展名，不产 FileInsight（与清单/规约文件同源）。
#[test]
fn test_empty_repo_bails_no_source_files() {
    let repo =
        std::env::temp_dir().join(format!("code_repo_wiki_edge_empty_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    build_repo(&repo, "zh", false, None); // 无源文件

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    // AnalysisResult 未实现 Debug，用 match 提取错误（期望 Err）
    let err_text = match full_generate_result(&repo, &root) {
        Ok(_) => panic!("空仓库应 bail，而非成功生成"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        err_text.contains("未找到任何源文件"),
        "空仓库应报错 '未找到任何源文件', 实际: {err_text}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// 场景 2：cards/{lang}/ 目录被外部删除后，再次生成自动重建目录与卡文件。
///
/// render_all 对 cards/{primary_lang} 执行 create_dir_all（output/mod.rs:369），
/// 故目录被删后无需人工重建。测试：全量生成 → 记录已落盘的模块卡文件 →
/// 删除整个 cards/zh/ 目录 → 再次全量生成 → 断言目录与这些卡文件重新存在。
#[test]
fn test_cards_dir_recreated_after_deletion() {
    let repo = std::env::temp_dir().join(format!(
        "code_repo_wiki_edge_cards_recreate_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    build_repo(&repo, "zh", true, None);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname=\"demo\"\n\n[dependencies]\nserde=\"1.0\"\n",
    )
    .unwrap();

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    full_generate(&repo, &root);

    // 全量完成后应已有卡片落盘
    let before = list_card_files(&repo, "zh");
    assert!(
        !before.is_empty(),
        "全量生成后 cards/zh/ 应至少产出一张卡, 实际: {:?}",
        before
    );

    // 删除整个 cards/zh/ 目录（模拟损坏/误删）
    let cards_zh = repo.join(".code-repo-wiki").join("cards").join("zh");
    std::fs::remove_dir_all(&cards_zh).unwrap();
    assert!(!cards_zh.exists(), "前置：cards/zh/ 应已被删除");

    // 再次全量生成 → 目录与卡文件重建
    full_generate(&repo, &root);
    assert!(cards_zh.exists(), "cards/zh/ 目录应在再次生成时被重建");
    for card in &before {
        assert!(
            list_card_files(&repo, "zh").contains(card),
            "卡文件 {card} 应在再次生成后重新落盘"
        );
    }
    // 卡片索引也应重建
    assert!(
        cards_zh.join("_index.json").exists(),
        "_index.json 应在再次生成后重建"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

/// 场景 3：规约文件内容为空 → 该文件被跳过；全部为空且无 notes →
/// Spec 卡不生成（输入驱动防幻觉），流水线不 panic。
///
/// collect_spec_files（project_card.rs）对空内容文件 `continue`（跳过该文件），
/// 故空 AGENTS.md 不进入 spec_files；空 + 无非空规约 + 无 notes →
/// generate_project_spec_card 返回 Ok(None) → 不向 LLM 请求 → 无 Spec 卡。
/// 断言与实现一致：生成成功（不 panic）且无 Spec 卡落盘。
#[test]
fn test_empty_spec_file_skipped_no_panic() {
    let repo = std::env::temp_dir().join(format!(
        "code_repo_wiki_edge_empty_spec_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    // 空 AGENTS.md + 只有 main.rs（无 Cargo.toml → 也无 TechStack 卡）
    build_repo(&repo, "zh", true, Some(""));

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    // 不 panic 即通过
    full_generate(&repo, &root);

    // 空 AGENTS.md 被跳过 → 无 Spec 卡落盘（输入驱动防幻觉，不与实现耦合过多）
    let spec_path = repo
        .join(".code-repo-wiki")
        .join("cards")
        .join("zh")
        .join("project")
        .join("spec.md");
    assert!(!spec_path.exists(), "空 AGENTS.md 被跳过时应不产 Spec 卡");

    let _ = std::fs::remove_dir_all(&repo);
}

/// 场景 4：wiki.language = "en" → 卡片落 cards/en/，而非 cards/zh/。
///
/// primary_language 返回 config.wiki.language 单值；render_all 与卡片写盘
/// 均按该值构造目录，故非 zh 语言走独立目录。断言产物路径含 cards/en/。
#[test]
fn test_non_zh_language_writes_en_cards() {
    let repo = std::env::temp_dir().join(format!(
        "code_repo_wiki_edge_lang_en_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    build_repo(&repo, "en", true, None);
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname=\"demo\"\n\n[dependencies]\nserde=\"1.0\"\n",
    )
    .unwrap();

    let root = code_repo_wiki::project::ProjectRoot::new(repo.clone());
    full_generate(&repo, &root);

    let cards_en = repo.join(".code-repo-wiki").join("cards").join("en");
    let cards_zh = repo.join(".code-repo-wiki").join("cards").join("zh");
    assert!(
        cards_en.exists(),
        "language=en 时卡片应落卡片目录 cards/en/"
    );
    assert!(!cards_zh.exists(), "language=en 时不应产生 cards/zh/ 目录");

    // 卡片文件确实落盘（含 project/ 子目录的 tech-stack 卡）
    let en_files = list_card_files(&repo, "en");
    assert!(
        !en_files.is_empty(),
        "cards/en/ 应至少产出一张卡, 实际: {:?}",
        en_files
    );
    let stack_path = cards_en.join("project").join("tech-stack.md");
    assert!(
        stack_path.exists(),
        "language=en 时 tech-stack.md 应落盘于 cards/en/project/"
    );

    let _ = std::fs::remove_dir_all(&repo);
}
