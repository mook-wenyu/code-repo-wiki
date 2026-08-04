//! export --skip-generate 的 CLI 级测试（票 09-CLI）
//!
//! 覆盖：
//! 1. 正常路径：generate 落盘导出快照后，export --skip-generate 直接消费
//!    快照导出 index.html（不重跑生成流水线）
//! 2. 快照缺失：无产物目录时 export --skip-generate 非 0 退出且 stderr
//!    明确引导先运行 generate/update（不静默回退重生成）
//!
//! 复用 test_cli_smoke.rs 的配置构造模式（内置 mock provider，generate 不触网）；
//! 每个测试使用独立临时目录（进程 pid + 自增序号）避免并行冲突。

use std::path::{Path, PathBuf};

mod common;
use common::{copy_dir, mock_config, run_bin, unique_dir};

/// 复制 sample-repo 到唯一临时目录并改写 config.toml（mock provider，不触网），返回工作目录
/// （v19 t04：基于 common helper，output.dir 绝对路径化，不依赖 cwd；
/// export 用例无需 search 索引，helper 默认形态即可）
fn prepare_repo(tag: &str) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("sample-repo");
    let work_dir = unique_dir(tag);
    let _ = std::fs::remove_dir_all(&work_dir);
    copy_dir(&fixture, &work_dir);
    std::fs::write(
        work_dir.join("config.toml"),
        mock_config(&work_dir.join(".repo-wiki").to_string_lossy()),
    )
    .unwrap();
    work_dir
}

/// 正常路径：generate 落盘快照后，export --skip-generate 直接导出 index.html
#[test]
fn test_export_skip_generate_after_generate() {
    let work_dir = prepare_repo("after_generate");

    // 1. 全量 generate：render_all 同步写 .state/export_snapshot.json（导出快照契约）
    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let snapshot = work_dir.join(".repo-wiki").join(".state").join("export_snapshot.json");
    assert!(snapshot.exists(), "generate 应写导出快照 {}", snapshot.display());

    // 2. export --skip-generate：不重跑生成，直接从快照导出
    let out = run_bin(&work_dir, &["export", "--skip-generate", "-c", "config.toml"]);
    assert!(
        out.status.success(),
        "export --skip-generate 应成功，stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        work_dir.join(".repo-wiki").join("index.html").exists(),
        "应导出 index.html（目录页）"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// 快照缺失：无产物目录时 export --skip-generate 非 0 退出且明确引导
#[test]
fn test_export_skip_generate_missing_snapshot_errors() {
    let work_dir = prepare_repo("no_snapshot");

    let out = run_bin(&work_dir, &["export", "--skip-generate", "-c", "config.toml"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "快照缺失时应显式失败，输出: {combined}"
    );
    assert!(
        combined.contains("导出快照不存在"),
        "应提示导出快照缺失并引导先运行 generate/update，实际: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// 快照过期（票 04）：快照 mtime 早于最新 wiki 页时 export --skip-generate
/// 必须显式报错，不得静默导出过期内容（陈旧不可观测是数据正确性风险）。
#[test]
fn test_export_skip_generate_stale_snapshot_errors() {
    let work_dir = prepare_repo("stale_snapshot");

    // 1. 生成（快照落盘）
    let out = run_bin(&work_dir, &["generate", "-c", "config.toml"]);
    assert!(out.status.success(), "generate 应成功: {}", String::from_utf8_lossy(&out.stderr));

    // 2. 人为让快照变旧：把 wiki 页 mtime 推到未来（快照早于产物）
    let wiki_dir = work_dir.join(".repo-wiki").join("wiki").join("zh");
    let mut page_mtimes: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(&wiki_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "md") {
            page_mtimes.push((path.clone(), std::fs::metadata(&path).unwrap().modified().unwrap()));
        }
    }
    assert!(!page_mtimes.is_empty(), "生成后应有 wiki 页");
    let max_page = page_mtimes.iter().map(|(_, t)| *t).max().unwrap();
    let far_future = max_page + std::time::Duration::from_secs(3600);
    // 用文件写入让 mtime 前进（touch 语义）：追加一个字节再截断
    for (path, _) in &page_mtimes {
        std::fs::write(path, format!("{} ", std::fs::read_to_string(path).unwrap())).unwrap();
    }
    let _ = far_future; // 不需要精确控制 mtime——重写即前进

    // 3. export --skip-generate 应报"导出快照过期"
    let out = run_bin(&work_dir, &["export", "--skip-generate", "-c", "config.toml"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success(),
        "快照过期时应显式失败，输出: {combined}"
    );
    assert!(
        combined.contains("导出快照过期"),
        "应提示快照过期并引导重新生成，实际: {combined}"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}
