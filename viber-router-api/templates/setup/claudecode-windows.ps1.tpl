# Viber Router — Claude Code Setup Script (Windows PowerShell 5.1+)
# Auto-generated — configures Claude Code to use your API key

$ErrorActionPreference = "Stop"

$ENDPOINT_URL = "{{ENDPOINT_URL}}"
$API_KEY = '{{API_KEY}}'
$HAIKU_MODEL = "{{HAIKU}}"
$OPUS_MODEL = "{{OPUS}}"
$SONNET_MODEL = "{{SONNET}}"
$SUBAGENT_MODEL = "{{SUBAGENT}}"
$TRACKING_URL = ""

Write-Host "================================" -ForegroundColor Blue
Write-Host "  Viber Router Claude Code Setup" -ForegroundColor Blue
Write-Host "================================" -ForegroundColor Blue
Write-Host ""

if ([string]::IsNullOrEmpty($ENDPOINT_URL)) {
    Write-Host "Error: Endpoint URL not configured" -ForegroundColor Red
    exit 1
}
if ([string]::IsNullOrEmpty($API_KEY)) {
    Write-Host "Error: API key not configured" -ForegroundColor Red
    exit 1
}

$MASKED_KEY = $API_KEY.Substring(0, [Math]::Min(10, $API_KEY.Length))
Write-Host "Endpoint URL: " -NoNewline
Write-Host "$ENDPOINT_URL" -ForegroundColor Green
Write-Host "API Key:      " -NoNewline
Write-Host "$MASKED_KEY..." -ForegroundColor Green
Write-Host ""

Write-Host "Cleaning up legacy environment variables..." -ForegroundColor Blue

# Claude Code reads these from the process environment and they take precedence
# over ~/.claude/settings.json. Stale values from older installs would silently
# override the configuration written below, so remove them everywhere.
$LEGACY_ENV_VARS = @(
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "CLAUDE_CODE_SUBAGENT_MODEL"
)

$cleanedAny = $false
$machineScopeBlocked = @()

foreach ($legacyName in $LEGACY_ENV_VARS) {
    # User scope
    $userValue = $null
    try {
        $userValue = [Environment]::GetEnvironmentVariable($legacyName, "User")
    } catch {
        $userValue = $null
    }
    # $null means "not defined"; "" means "defined but empty", which still
    # overrides settings.json and must be cleaned up too.
    if ($null -ne $userValue) {
        try {
            [Environment]::SetEnvironmentVariable($legacyName, $null, "User")
            Write-Host "  " -NoNewline
            Write-Host "OK" -ForegroundColor Green -NoNewline
            Write-Host " Removed $legacyName (User scope)"
            $cleanedAny = $true
        } catch {
            Write-Host "  " -NoNewline
            Write-Host "Warning" -ForegroundColor Yellow -NoNewline
            Write-Host " Could not remove $legacyName (User scope): $($_.Exception.Message)"
        }
    }

    # Machine scope (needs an elevated shell)
    $machineValue = $null
    try {
        $machineValue = [Environment]::GetEnvironmentVariable($legacyName, "Machine")
    } catch {
        $machineValue = $null
    }
    if ($null -ne $machineValue) {
        try {
            [Environment]::SetEnvironmentVariable($legacyName, $null, "Machine")
            Write-Host "  " -NoNewline
            Write-Host "OK" -ForegroundColor Green -NoNewline
            Write-Host " Removed $legacyName (Machine scope)"
            $cleanedAny = $true
        } catch {
            $machineScopeBlocked += $legacyName
        }
    }

    # Current process session. PowerShell drops empty environment variables from
    # the Env: drive, so Test-Path is enough to detect anything still set here.
    try {
        if (Test-Path "Env:\$legacyName") {
            Remove-Item "Env:\$legacyName" -ErrorAction Stop
            Write-Host "  " -NoNewline
            Write-Host "OK" -ForegroundColor Green -NoNewline
            Write-Host " Removed $legacyName (current session)"
            $cleanedAny = $true
        }
    } catch {
        Write-Host "  " -NoNewline
        Write-Host "Warning" -ForegroundColor Yellow -NoNewline
        Write-Host " Could not remove $legacyName from the current session"
    }
}

if ($machineScopeBlocked.Count -gt 0) {
    Write-Host ""
    Write-Host "  Warning: these machine-wide variables could not be removed (needs Administrator):" -ForegroundColor Yellow
    foreach ($blockedName in $machineScopeBlocked) {
        Write-Host "    - $blockedName" -ForegroundColor Yellow
    }
    Write-Host "  They override ~/.claude/settings.json." -ForegroundColor Yellow
    Write-Host "  Open PowerShell as Administrator and run:" -ForegroundColor Yellow
    foreach ($blockedName in $machineScopeBlocked) {
        Write-Host "    [Environment]::SetEnvironmentVariable('$blockedName', `$null, 'Machine')" -ForegroundColor Yellow
    }
}

if (-not $cleanedAny) {
    Write-Host "  No legacy environment variables to clean up"
}

$settingsDir = Join-Path $env:USERPROFILE ".claude"
$settingsPath = Join-Path $settingsDir "settings.json"
$statuslinePath = Join-Path $settingsDir "statusline.ps1"

if (-not (Test-Path $settingsDir)) {
    New-Item -ItemType Directory -Path $settingsDir -Force | Out-Null
}

try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 -bor [Net.SecurityProtocolType]::Tls13
} catch {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
}

# Install statusline script
$statuslineInstalled = $false
if (-not [string]::IsNullOrEmpty($TRACKING_URL)) {
    Write-Host ""
    Write-Host "Installing statusline script..." -ForegroundColor Blue

    try {
        $statuslineContent = @'
# Viber Router Statusline Script for Claude Code (Windows PowerShell 5.1+)

$ErrorActionPreference = "SilentlyContinue"

$TRACKING_URL = "PLACEHOLDER_TRACKING_URL"
$API_KEY = 'PLACEHOLDER_API_KEY'

# ESC character compatible with PowerShell 5.1+ (`e only works in PS 7+)
$ESC = [char]27

$CacheDir = Join-Path $env:TEMP "viberrouter-statusline"
$CacheFile = Join-Path $CacheDir "tracking_cache.json"
$CacheTTL = 30

if (-not (Test-Path $CacheDir)) {
    New-Item -ItemType Directory -Path $CacheDir -Force | Out-Null
}

$InputJson = ""
if ([Console]::IsInputRedirected) {
    try { $InputJson = [Console]::In.ReadToEnd() } catch {}
}

function Parse-Context {
    param([string]$Json)
    $script:CWD = ""
    $script:Model = "unknown"
    $script:ModelId = ""
    $script:ConversationTokens = 0
    $script:MaxTokens = 200000
    $script:UsedPercentage = 0
    $script:LinesAdded = 0
    $script:LinesRemoved = 0
    $script:GitNumFiles = 0
    $script:GitBranch = ""
    if ([string]::IsNullOrEmpty($Json)) { return }
    try {
        $ctx = $Json | ConvertFrom-Json
        if ($ctx.cwd) { $script:CWD = $ctx.cwd }
        if ($ctx.gitNumStagedOrUnstagedFilesChanged) { $script:GitNumFiles = [int]$ctx.gitNumStagedOrUnstagedFilesChanged }
        if ($ctx.model.display_name) { $script:Model = $ctx.model.display_name }
        if ($ctx.model.id) { $script:ModelId = $ctx.model.id }
        # context_window contains current_usage and context_window_size
        $cw = $ctx.context_window
        if ($cw) {
            $cu = $cw.current_usage
            if ($cu) {
                $inputTok = if ($cu.input_tokens) { [int64]$cu.input_tokens } else { 0 }
                $cacheCreate = if ($cu.cache_creation_input_tokens) { [int64]$cu.cache_creation_input_tokens } else { 0 }
                $cacheRead = if ($cu.cache_read_input_tokens) { [int64]$cu.cache_read_input_tokens } else { 0 }
                $script:ConversationTokens = $inputTok + $cacheCreate + $cacheRead
                if ($cu.output_tokens) { $script:OutputTokens = [int64]$cu.output_tokens }
            }
            if ($cw.context_window_size) { $script:MaxTokens = [int64]$cw.context_window_size }
            if ($cw.used_percentage) { $script:UsedPercentage = [int]$cw.used_percentage }
        }
        if ($ctx.cost.total_lines_added) { $script:LinesAdded = [int]$ctx.cost.total_lines_added }
        if ($ctx.cost.total_lines_removed) { $script:LinesRemoved = [int]$ctx.cost.total_lines_removed }
        if ($script:CWD -and (Test-Path $script:CWD) -and (Get-Command git -ErrorAction SilentlyContinue)) {
            $branch = git -C $script:CWD branch --show-current 2>$null
            if ($branch) { $script:GitBranch = $branch.Trim() }
        }
    } catch {}
}

function Shorten-Path {
    param([string]$Path)
    $home = $env:USERPROFILE
    if ($Path.StartsWith($home)) { $Path = "~" + $Path.Substring($home.Length) }
    if ($Path.Length -gt 40) {
        $parts = $Path -split '[/\\]'
        if ($parts.Count -ge 2) { $Path = $parts[-2] + "/" + $parts[-1] }
    }
    return $Path
}

function Format-Model {
    param([string]$Model)
    if ($Model -match '(?i)(opus|sonnet|haiku)') {
        $tier = $Matches[1]
        $tier = $tier.Substring(0,1).ToUpper() + $tier.Substring(1).ToLower()
        if ($Model -match '(\d+[\.\-]\d+)') {
            $version = $Matches[1] -replace '-','.'
            return "${tier}-${version}"
        }
        return $tier
    }
    if ($Model.Length -gt 18) { return $Model.Substring(0, 18) }
    return $Model
}

function Format-Tokens {
    param([int64]$Num)
    if ($Num -ge 1000000) { return "{0:F1}M" -f ($Num / 1000000) }
    elseif ($Num -ge 1000) { return "{0}k" -f [math]::Floor($Num / 1000) }
    return "$Num"
}

function Format-VND {
    param([double]$Num)
    $intVal = [math]::Round($Num)
    $str = $intVal.ToString()
    $result = ""
    $count = 0
    for ($i = $str.Length - 1; $i -ge 0; $i--) {
        if ($count -gt 0 -and $count % 3 -eq 0) { $result = "." + $result }
        $result = $str[$i] + $result
        $count++
    }
    return $result
}

function Get-Tracking {
    $now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
    if (Test-Path $CacheFile) {
        $cacheTime = (Get-Item $CacheFile).LastWriteTimeUtc
        $cacheEpoch = [DateTimeOffset]::new($cacheTime).ToUnixTimeSeconds()
        $age = $now - $cacheEpoch
        if ($age -lt $CacheTTL) { return Get-Content $CacheFile -Raw }
    }
    if (-not [string]::IsNullOrEmpty($TRACKING_URL) -and -not [string]::IsNullOrEmpty($API_KEY)) {
        try {
            $headers = @{ "Authorization" = "Bearer $API_KEY" }
            $response = Invoke-RestMethod -Uri $TRACKING_URL -Headers $headers -TimeoutSec 5 -UseBasicParsing -ErrorAction Stop
            $jsonStr = $response | ConvertTo-Json -Depth 5 -Compress
            [System.IO.File]::WriteAllText($CacheFile, $jsonStr)
            return $jsonStr
        } catch {}
    }
    if (Test-Path $CacheFile) { return Get-Content $CacheFile -Raw }
    return ""
}

function Progress-Bar {
    param([int64]$Current, [int64]$Max)
    $width = 5
    if ($Max -eq 0) { return "$($ESC)[90m.....$($ESC)[0m" }
    $pct = [math]::Floor($Current * 100 / $Max)
    $filled = [math]::Floor($pct * $width / 100)
    $empty = $width - $filled
    $bar = ""
    for ($i = 0; $i -lt $filled; $i++) { $bar += "$($ESC)[32m#$($ESC)[0m" }
    for ($i = 0; $i -lt $empty; $i++) { $bar += "$($ESC)[90m.$($ESC)[0m" }
    return $bar
}

function Main {
    Parse-Context $InputJson
    $W = "$($ESC)[97m"; $G = "$($ESC)[32m"; $Y = "$($ESC)[33m"; $R = "$($ESC)[31m"; $C = "$($ESC)[36m"; $DM = "$($ESC)[90m"; $D = "$($ESC)[0m"

    $line1 = ""
    $shortCwd = if ($script:CWD) { Shorten-Path $script:CWD } else { Shorten-Path (Get-Location).Path }
    $line1 += "${C}${shortCwd}${D}"
    if ($script:GitBranch) {
        $line1 += "  ${DM}@${D} ${W}$($script:GitBranch)${D}"
        if ($script:GitNumFiles -gt 0) { $line1 += " ${Y}($($script:GitNumFiles))${D}" }
    }
    if ($script:LinesAdded -gt 0 -or $script:LinesRemoved -gt 0) {
        $line1 += "  ${G}+$($script:LinesAdded)${D} ${R}-$($script:LinesRemoved)${D}"
    }

    $line2 = ""
    # Use used_percentage from Claude Code if available, otherwise calculate
    $ctxPct = $script:UsedPercentage
    if ($ctxPct -eq 0 -and $script:MaxTokens -gt 0 -and $script:ConversationTokens -gt 0) {
        $ctxPct = [math]::Floor($script:ConversationTokens * 100 / $script:MaxTokens)
    }
    $ctxBar = Progress-Bar $ctxPct 100
    $line2 += "$ctxBar ${W}${ctxPct}%${D}"

    $modelDisplay = Format-Model $script:Model
    $line2 += " ${DM}|${D} ${W}${modelDisplay}${D}"

    $trackingData = Get-Tracking
    if (-not [string]::IsNullOrEmpty($trackingData)) {
        try {
            $tracking = $trackingData | ConvertFrom-Json
            $balance = if ($tracking.balance) { [double]$tracking.balance } else { 0 }
            if ($balance -gt 0) {
                $balFmt = Format-VND $balance
                $line2 += " ${DM}|${D} ${G}${balFmt}d${D}"
            }
            $lastTokens = if ($tracking.last_request.total_tokens) { [int64]$tracking.last_request.total_tokens } else { 0 }
            $lastCost = if ($tracking.last_request.total_cost) { [double]$tracking.last_request.total_cost } else { 0 }
            if ($lastTokens -gt 0) {
                $lastTokFmt = Format-Tokens $lastTokens
                $lastCostFmt = Format-VND $lastCost
                $line2 += " ${DM}|${D} ${DM}Cost:${D} ${W}${lastTokFmt} tokens${D} ${Y}${lastCostFmt}d${D}"
            }
        } catch {}
    }

    Write-Output $line1
    Write-Output $line2
}

Main
'@
        $statuslineContent = $statuslineContent.Replace("PLACEHOLDER_TRACKING_URL", $TRACKING_URL).Replace("PLACEHOLDER_API_KEY", $API_KEY)
        [System.IO.File]::WriteAllText($statuslinePath, $statuslineContent, [System.Text.UTF8Encoding]::new($true))
        Write-Host "  " -NoNewline
        Write-Host "OK" -ForegroundColor Green -NoNewline
        Write-Host " Installed $statuslinePath"
        $statuslineInstalled = $true
    } catch {
        Write-Host "  " -NoNewline
        Write-Host "Warning" -ForegroundColor Yellow -NoNewline
        Write-Host " Could not create statusline script: $($_.Exception.Message)"
    }
} else {
    Write-Host ""
    Write-Host "  Statusline: Skipped (no tracking URL)" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "Configuring Claude Code settings..." -ForegroundColor Blue

$settings = $null

if (Test-Path $settingsPath) {
    $backupPath = "$settingsPath.backup.$(Get-Date -Format 'yyyyMMddHHmmss')"
    Copy-Item $settingsPath $backupPath
    Write-Host "  Backed up: $settingsPath" -ForegroundColor Yellow

    try {
        $content = Get-Content $settingsPath -Raw -ErrorAction Stop
        if ($content) {
            $settings = ConvertFrom-Json $content
        }
    } catch {
        Write-Host "  Warning: Could not parse existing settings.json, creating new one" -ForegroundColor Yellow
        $settings = $null
    }
}

if ($null -eq $settings) {
    $settings = New-Object PSObject
}

if (-not (Get-Member -InputObject $settings -Name "env" -MemberType Properties)) {
    $settings | Add-Member -MemberType NoteProperty -Name "env" -Value (New-Object PSObject)
}

$envVars = @{
    "ANTHROPIC_BASE_URL" = $ENDPOINT_URL
    "ANTHROPIC_AUTH_TOKEN" = $API_KEY
    "ANTHROPIC_DEFAULT_HAIKU_MODEL" = $HAIKU_MODEL
    "ANTHROPIC_DEFAULT_OPUS_MODEL" = $OPUS_MODEL
    "ANTHROPIC_DEFAULT_SONNET_MODEL" = $SONNET_MODEL
    "CLAUDE_CODE_SUBAGENT_MODEL" = $SUBAGENT_MODEL
}

foreach ($kvp in $envVars.GetEnumerator()) {
    if (Get-Member -InputObject $settings.env -Name $kvp.Key -MemberType Properties) {
        $settings.env.($kvp.Key) = $kvp.Value
    } else {
        $settings.env | Add-Member -MemberType NoteProperty -Name $kvp.Key -Value $kvp.Value
    }
}

if (Get-Member -InputObject $settings -Name "disableLoginPrompt" -MemberType Properties) {
    $settings.disableLoginPrompt = $true
} else {
    $settings | Add-Member -MemberType NoteProperty -Name "disableLoginPrompt" -Value $true
}

if (Get-Member -InputObject $settings -Name "includeCoAuthoredBy" -MemberType Properties) {
    $settings.includeCoAuthoredBy = $false
} else {
    $settings | Add-Member -MemberType NoteProperty -Name "includeCoAuthoredBy" -Value $false
}

# Set statusLine if statusline script was installed
if ($statuslineInstalled) {
    if (Get-Command pwsh -ErrorAction SilentlyContinue) {
        $statuslineCommand = "pwsh -NoProfile -ExecutionPolicy Bypass -File `"$statuslinePath`""
    } else {
        $statuslineCommand = "powershell -NoProfile -ExecutionPolicy Bypass -File `"$statuslinePath`""
    }

    $statusLineObj = New-Object PSObject
    $statusLineObj | Add-Member -MemberType NoteProperty -Name "type" -Value "command"
    $statusLineObj | Add-Member -MemberType NoteProperty -Name "command" -Value $statuslineCommand

    if (Get-Member -InputObject $settings -Name "statusLine" -MemberType Properties) {
        $settings.statusLine = $statusLineObj
    } else {
        $settings | Add-Member -MemberType NoteProperty -Name "statusLine" -Value $statusLineObj
    }
}

$jsonContent = $settings | ConvertTo-Json -Depth 10
[System.IO.File]::WriteAllText($settingsPath, $jsonContent, [System.Text.UTF8Encoding]::new($false))

Write-Host "  " -NoNewline
Write-Host "OK" -ForegroundColor Green -NoNewline
Write-Host " Updated $settingsPath"

Write-Host ""
Write-Host "================================" -ForegroundColor Green
Write-Host "  Configuration Complete!" -ForegroundColor Green
Write-Host "================================" -ForegroundColor Green
Write-Host ""
Write-Host "Claude Code is now configured:"
Write-Host "  Endpoint:   " -NoNewline
Write-Host "$ENDPOINT_URL" -ForegroundColor Blue
Write-Host "  API Key:    " -NoNewline
Write-Host "$MASKED_KEY..." -ForegroundColor Blue
Write-Host "  Config:     " -NoNewline
Write-Host "$settingsPath" -ForegroundColor Blue
if ($statuslineInstalled) {
    Write-Host "  Statusline: " -NoNewline
    Write-Host "Enabled (balance, tokens, cost)" -ForegroundColor Green
}
Write-Host ""
Write-Host "All settings live in settings.json - no environment variables are set."
Write-Host ""
Write-Host "Next steps:" -ForegroundColor Yellow
Write-Host "  1. Open a new terminal (existing ones may still hold the removed variables)"
Write-Host "  2. Run: " -NoNewline
Write-Host "claude" -ForegroundColor Blue
Write-Host ""
