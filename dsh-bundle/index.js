// code-repo-wiki DSH bundle entry point
// This bundle registers the code-repo-wiki MCP server for DeepSeek Harness sessions.
// The MCP server provides wiki_search, wiki_ast_search, wiki_status, wiki_read_page,
// and wiki_read_card tools for code intelligence.

export const name = 'code-repo-wiki'

export function apply(ctx) {
  // Bundle-only: configuration layer only, no runtime code needed.
  // The @deepseek-ai/dsh-mcp-client plugin handles the actual MCP connection.
  console.log('[code-repo-wiki] bundle loaded')
}
