

// 交叉引用渲染（U08/N13：CrossRefIndex/validate 双套检查删除——lint 的
// broken 检查（磁盘产物级，CI 门禁）已覆盖引用目标存在性，生成期的
// 内存级 validate 冗余且与 lint 规则漂移；find_references 仅测试调用，
// 一并清理为死代码。本模块只保留渲染辅助。）

/// 渲染交叉引用为 Markdown 链接
///
/// 纯 Markdown 环境不使用 HTML <cite> 标签（会引起渲染问题），改为标准链接格式。
pub fn render_cite_link(symbol: &str, target_path: &str) -> String {
    format!("[`{symbol}`]({target_path})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_cite_link() {
        assert_eq!(
            render_cite_link("Net", "wiki/Net.md"),
            "[`Net`](wiki/Net.md)"
        );
    }
}
