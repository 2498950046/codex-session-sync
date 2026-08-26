[CmdletBinding(SupportsShouldProcess)]
param(
    [ValidateNotNullOrEmpty()]
    [string]$Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = Read-Host "请输入发布版本号（例如 0.1.1）"
}

# Git tag 使用 v0.1.1；配置文件只保存不带 v 的标准语义化版本号。
$semVerPattern = '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$'
if ($Version -notmatch $semVerPattern) {
    throw "版本号 '$Version' 不是有效的语义化版本。请输入例如 0.1.1 或 0.1.1-rc.1，且不要带 v 前缀。"
}

$repositoryRoot = Split-Path -Parent $PSScriptRoot

function New-VersionChange {
    param(
        [Parameter(Mandatory)] [string]$RelativePath,
        [Parameter(Mandatory)] [string]$Pattern,
        [Parameter(Mandatory)] [string]$Description
    )

    $path = Join-Path $repositoryRoot $RelativePath
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "找不到要更新的文件：$RelativePath"
    }

    $original = [System.IO.File]::ReadAllText($path)
    $regex = [regex]::new($Pattern, [System.Text.RegularExpressions.RegexOptions]::Multiline)
    $matches = $regex.Matches($original)
    if ($matches.Count -ne 1) {
        throw "$RelativePath 中应恰好有一个可更新的版本字段，实际找到 $($matches.Count) 个。"
    }

    $updated = $regex.Replace(
        $original,
        [System.Text.RegularExpressions.MatchEvaluator] {
            param($match)
            "$($match.Groups['prefix'].Value)$Version$($match.Groups['suffix'].Value)"
        },
        1
    )

    [pscustomobject]@{
        Path = $path
        RelativePath = $RelativePath
        Description = $Description
        Changed = $updated -cne $original
        Content = $updated
    }
}

# 每一个替换都先在内存中完成；任何文件不符合预期时不会写入任何文件。
$changes = @(
    New-VersionChange -RelativePath "Cargo.toml" -Description "Rust workspace" -Pattern '(?s)(?<prefix>^\[workspace\.package\].*?^version\s*=\s*")[^"]+(?<suffix>")'
    New-VersionChange -RelativePath "apps/desktop/package.json" -Description "Desktop npm package" -Pattern '(?<prefix>^\s*"version"\s*:\s*")[^"]+(?<suffix>")'
    New-VersionChange -RelativePath "apps/desktop/src-tauri/tauri.conf.json" -Description "Tauri bundle" -Pattern '(?<prefix>^\s*"version"\s*:\s*")[^"]+(?<suffix>")'
    New-VersionChange -RelativePath "deploy/server/.env.ghcr.example" -Description "GHCR deployment example" -Pattern '(?<prefix>^SYNC_SERVER_IMAGE=ghcr\.io/[^/\r\n]+/codex-session-sync-server:)[^\r\n]+(?<suffix>\r?$)'
)

$utf8WithoutBom = [System.Text.UTF8Encoding]::new($false)
foreach ($change in $changes) {
    if (-not $change.Changed) {
        Write-Host "无需修改：$($change.RelativePath) 已是 $Version"
        continue
    }

    if ($PSCmdlet.ShouldProcess($change.RelativePath, "将 $($change.Description) 版本改为 $Version")) {
        [System.IO.File]::WriteAllText($change.Path, $change.Content, $utf8WithoutBom)
        Write-Host "已更新：$($change.RelativePath) -> $Version"
    }
}

Write-Host ""
Write-Host "下一步：运行 npm test、cargo test --workspace，然后创建 tag v$Version。"
