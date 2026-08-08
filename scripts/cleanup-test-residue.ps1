# code-repo-wiki 测试残留清理脚本
#
# 背景（v33 生产审计 ①）：历史测试（key 命令注入隔离修复前）在真实
# 用户目录留下三类残留，均为无害但不卫生的目录：
#   1. %APPDATA%\code-repo-wiki\key-test-*   —— key 命令测试夹具目录
#      （每目录仅一个假 config.toml，无真实密钥）
#   2. %TEMP%\repo_wiki* / code_repo_wiki*   —— 各测试的临时仓库目录
#      （为空目录，≈0B 占用；repo_wiki* 为 v37 改名前的历史命名）
#   3. %TEMP%\rw_desc_*                       —— generate/wiki.rs 描述缓存测试残留
#
# 用法：
#   powershell -File scripts/cleanup-test-residue.ps1         # 预览（不删除）
#   powershell -File scripts/cleanup-test-residue.ps1 -Apply  # 确认后删除
#
# 编码说明：本文件以 UTF-8 BOM 保存——Windows PowerShell 5.1 默认按
# ANSI/GBK 解码无 BOM 的 UTF-8 文件，中文注释会导致 ParserError；
# BOM 让 5.1 正确识别 UTF-8（PowerShell 7+ 无此问题）。
#
# 安全约束：
#   - 默认只预览，加 -Apply 才真正删除（防误删正在运行的测试目录）
#   - 只匹配命名模式（key-test-* / repo_wiki* / rw_desc_*），绝不触碰其他目录
#   - 测试运行期间请勿执行（正在使用的临时目录会被删掉）

param(
    [switch]$Apply
)

$ErrorActionPreference = 'Stop'

function Preview-Deletes {
    param([string]$Scope, [System.Collections.Generic.List[string]]$Targets)
    if ($Targets.Count -eq 0) {
        Write-Host "  $Scope : 无匹配残留"
        return
    }
    Write-Host "  $Scope : $($Targets.Count) 个目录待删除"
    if (-not $Apply) {
        $Targets | Select-Object -First 5 | ForEach-Object { Write-Host "    - $_" }
        if ($Targets.Count -gt 5) { Write-Host "    ... 其余略" }
    }
}

# ---- 1. APPDATA 下的 key 测试夹具（新名 code-repo-wiki 与历史旧名 repo-wiki 双清） ----
$keyResidue = [System.Collections.Generic.List[string]]::new()
foreach ($appName in @('code-repo-wiki', 'repo-wiki')) {
    $appData = Join-Path $env:APPDATA $appName
    if (Test-Path -LiteralPath $appData) {
        Get-ChildItem -LiteralPath $appData -Directory -Filter 'key-test-*' -ErrorAction SilentlyContinue |
            ForEach-Object { $keyResidue.Add($_.FullName) }
    }
}

# ---- 2. TEMP 下的测试仓库目录（新旧命名双清） ----
$tempResidue = [System.Collections.Generic.List[string]]::new()
foreach ($filter in @('repo_wiki*', 'code_repo_wiki*')) {
    Get-ChildItem -LiteralPath $env:TEMP -Directory -Filter $filter -ErrorAction SilentlyContinue |
        ForEach-Object { $tempResidue.Add($_.FullName) }
}

# ---- 3. TEMP 下的描述缓存测试残留（generate/wiki.rs） ----
Get-ChildItem -LiteralPath $env:TEMP -Directory -Filter 'rw_desc_*' -ErrorAction SilentlyContinue |
    ForEach-Object { $tempResidue.Add($_.FullName) }

Write-Host 'code-repo-wiki 测试残留扫描结果：'
Preview-Deletes 'key-test-* 残留' $keyResidue
Preview-Deletes 'repo_wiki*/code_repo_wiki*/rw_desc_* 临时目录' $tempResidue

if (-not $Apply) {
    Write-Host ''
    Write-Host '以上为预览。确认删除请加 -Apply 参数重新执行。'
    exit 0
}

# ---- 实际删除 ----
$total = 0
foreach ($dir in $keyResidue) {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction Stop
    $total++
}
foreach ($dir in $tempResidue) {
    Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction Stop
    $total++
}
Write-Host "已删除 $total 个残留目录。"

