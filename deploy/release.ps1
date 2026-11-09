# S8 step 7: release build with SHA-256 checksums (docs/ADMIN.md).
#
#   powershell -NoProfile -File deploy/release.ps1 [-Version 0.1.2] [options]
#   powershell -NoProfile -File deploy/release.ps1 -Verify -Out <release dir>
#
# Builds the Windows client (launcher + client, version baked in), its installer
# package and per-user installer (ADR 0017), the signing tool, and the Linux server
# bundle (install-server.sh + src.tar.gz of the committed source; the server is built on
# the host by install-server.sh). Optionally signs the client as an update (ADR 0016).
# Writes release.json and SHA256SUMS (sha256sum format, so `sha256sum -c SHA256SUMS`
# works on Linux too) and verifies them before it finishes.
#
# Layout of <Out>:
#   windows/turbo-pancake-setup-<v>.exe   installer (embeds the package)
#   windows/client/turbo-pancake.exe      launcher  } the package contents (the client
#   windows/client/turbo-pancake-desktop.exe client } alone is signed as the update)
#   tools/turbo-pancake-update.exe        offline signing tool (owner's machine only)
#   server/install-server.sh, server/src.tar.gz
#   update/manifest.json, update/files/<v>/...   only with -UpdateKey
#   release.json, SHA256SUMS
#
# Options (nothing of them is committed; the address and keys are build inputs only):
#   -DefaultServer wss://<address>:7878/v1/signal  and  -DefaultServerCa <ca.pem>
#       a client for one server (docs/DEPLOY-TLS.md section 5)
#   -UpdatePubkeys "<id>:<base64>,..."   update keys pinned into the client
#   -UpdateKey <key.pk8> -UpdateKeyId <id>   sign the client as an update (the id must
#       be among -UpdatePubkeys)
#   -AllowDirty   build uncommitted tracked changes (recorded as dirty; untracked files
#       are never part of src.tar.gz)
#   -Report <file.md>   a summary without paths or addresses (for benchmarks/results)
param(
    [string]$Version = '',
    [string]$Out = '',
    [string]$CargoWrapper = '',
    [string]$TargetDir = '',
    [string]$DefaultServer = '',
    [string]$DefaultServerCa = '',
    [string]$UpdatePubkeys = '',
    [string]$UpdateKey = '',
    [string]$UpdateKeyId = '',
    [int]$UpdateExpiresDays = 90,
    [string]$Notes = '',
    [string]$Report = '',
    [switch]$AllowDirty,
    [switch]$Verify
)
$ErrorActionPreference = 'Stop'
$workspacePath = Split-Path $PSScriptRoot -Parent
if (-not $CargoWrapper) { $CargoWrapper = Join-Path $workspacePath 'benchmarks/scripts/apm-env.ps1' }
if (-not $TargetDir) { $TargetDir = Join-Path $workspacePath 't/r' }  # short: APM paths < MAX_PATH

function Fail([string]$message) { Write-Host "release: $message"; exit 1 }

# Every file under $dir except SHA256SUMS, relative with forward slashes, sorted.
function Files-Of([string]$dir) {
    $root = (Resolve-Path $dir).Path.TrimEnd('\') + '\'
    Get-ChildItem -Path $dir -Recurse -File |
        ForEach-Object { $_.FullName.Substring($root.Length) -replace '\\', '/' } |
        Where-Object { $_ -ne 'SHA256SUMS' } | Sort-Object
}

function Sha256([string]$path) { (Get-FileHash -Algorithm SHA256 -Path $path).Hash.ToLowerInvariant() }

# Checks SHA256SUMS against the files: every listed file matches, nothing unlisted.
function Test-Sums([string]$dir) {
    $sums = Join-Path $dir 'SHA256SUMS'
    if (-not (Test-Path $sums)) { return @($false, 'SHA256SUMS missing') }
    $listed = @{}
    foreach ($line in [IO.File]::ReadAllLines($sums)) {
        if ($line -eq '') { continue }
        if ($line -notmatch '^([0-9a-f]{64})  (\S.*)$') { return @($false, "bad line: $line") }
        $listed[$Matches[2]] = $Matches[1]
    }
    $bad = @()
    foreach ($rel in $listed.Keys) {
        $p = Join-Path $dir $rel
        if (-not (Test-Path $p -PathType Leaf)) { $bad += "missing $rel"; continue }
        if ((Sha256 $p) -ne $listed[$rel]) { $bad += "mismatch $rel" }
    }
    foreach ($rel in (Files-Of $dir)) { if (-not $listed.ContainsKey($rel)) { $bad += "unlisted $rel" } }
    if ($bad.Count -gt 0) { return @($false, ($bad -join '; ')) }
    return @($true, "$($listed.Count) files match")
}

if ($Verify) {
    if (-not $Out) { Fail '-Verify needs -Out <release dir>' }
    $r = Test-Sums $Out
    Write-Host "release verify: $($r[1])"
    if ($r[0]) { exit 0 } else { exit 1 }
}

# ---- inputs ----
$cargoToml = Get-Content -Raw (Join-Path $workspacePath 'Cargo.toml')
if ($cargoToml -notmatch '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"') { Fail 'workspace version not found in Cargo.toml' }
$workspaceVersion = $Matches[1]
if (-not $Version) { $Version = $workspaceVersion }
if ($Version -notmatch '^\d+\.\d+\.\d+$') { Fail "version ${Version}: expected major.minor.patch" }
if (-not $Out) { $Out = Join-Path $workspacePath "t/dist/turbo-pancake-$Version" }
if ((Test-Path $Out) -and (Get-ChildItem $Out -Force | Select-Object -First 1)) { Fail "$Out exists and is not empty; choose another -Out" }
if ($DefaultServer -and $DefaultServer -notmatch '^wss://') { Fail '-DefaultServer must be a wss:// address (accounts need TLS, ADR 0014)' }
if ($DefaultServer -and -not $DefaultServerCa) { Write-Host 'release: no -DefaultServerCa: the client will trust only the web PKI for this server' }
if ($DefaultServerCa -and -not (Test-Path $DefaultServerCa -PathType Leaf)) { Fail "CA file not found: $DefaultServerCa" }
if ($DefaultServerCa -and -not $DefaultServer) { Fail '-DefaultServerCa needs -DefaultServer' }
$keyIds = @()
if ($UpdatePubkeys) {
    foreach ($entry in ($UpdatePubkeys -split ',')) {
        if ($entry -notmatch '^([A-Za-z0-9._-]+):[A-Za-z0-9+/=]+$') { Fail "bad -UpdatePubkeys entry: $entry" }
        $keyIds += $Matches[1]
    }
}
if ($UpdateKey) {
    if (-not (Test-Path $UpdateKey -PathType Leaf)) { Fail "update key not found: $UpdateKey" }
    if (-not $UpdateKeyId) { Fail '-UpdateKey needs -UpdateKeyId' }
    if ($keyIds -notcontains $UpdateKeyId) { Fail "key id $UpdateKeyId is not among -UpdatePubkeys: clients of this build could not verify the update" }
}

Push-Location $workspacePath
try {
    $commit = (& git rev-parse HEAD).Trim()
    $dirtyList = @(& git status --porcelain --untracked-files=no)
    $dirty = $dirtyList.Count -gt 0
    if ($dirty -and -not $AllowDirty) { Fail "uncommitted changes in tracked files (commit them or pass -AllowDirty):`n$($dirtyList -join "`n")" }
    # Tree to archive: HEAD, or a stash commit of the tracked working tree when dirty.
    $tree = 'HEAD'
    if ($dirty) { $tree = (& git stash create).Trim() }
} finally { Pop-Location }

$logs = Join-Path $TargetDir 'release-logs'
New-Item -ItemType Directory -Path $logs, $Out -Force | Out-Null
$stamp = [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss')
function Cargo([string[]]$cargoArgs, [string]$name) {
    $log = Join-Path $logs "$Version-$stamp-$name.log"
    $old = $ErrorActionPreference; $ErrorActionPreference = 'Continue'
    & powershell -NoProfile -File $CargoWrapper cargo @cargoArgs *> $log
    $code = $LASTEXITCODE; $ErrorActionPreference = $old
    if ($code -ne 0) { Fail "cargo $name failed ($code), see $log" }
}

# Build inputs only from the parameters: stray variables of the caller never leak in.
$buildVars = [ordered]@{
    TURBO_PANCAKE_BUILD_VERSION      = $Version
    TURBO_PANCAKE_DEFAULT_SERVER     = $DefaultServer
    TURBO_PANCAKE_DEFAULT_SERVER_CA  = $(if ($DefaultServerCa) { (Resolve-Path $DefaultServerCa).Path } else { '' })
    TURBO_PANCAKE_UPDATE_PUBKEYS     = $UpdatePubkeys
    TURBO_PANCAKE_SETUP_PAYLOAD      = ''
}
$saved = @{}
foreach ($k in $buildVars.Keys) { $saved[$k] = [Environment]::GetEnvironmentVariable($k) }
function Set-Var([string]$k, [string]$v) { if ($v) { [Environment]::SetEnvironmentVariable($k, $v) } else { [Environment]::SetEnvironmentVariable($k, $null) } }

$t0 = [DateTime]::UtcNow
try {
    foreach ($k in $buildVars.Keys) { Set-Var $k $buildVars[$k] }
    Write-Host "== build $Version (client, launcher, signing tool)"
    Cargo @('build', '--release', '--locked', '--target-dir', $TargetDir,
            '-p', 'turbo-pancake-launcher', '-p', 'turbo-pancake-desktop', '-p', 'turbo-pancake-update-tool') 'client'
    $bin = Join-Path $TargetDir 'release'
    $win = Join-Path $Out 'windows'; $client = Join-Path $win 'client'; $tools = Join-Path $Out 'tools'
    New-Item -ItemType Directory -Path $client, $tools -Force | Out-Null
    Copy-Item (Join-Path $bin 'turbo-pancake.exe'), (Join-Path $bin 'turbo-pancake-desktop.exe') $client
    Copy-Item (Join-Path $bin 'turbo-pancake-update.exe') $tools
    $tool = Join-Path $tools 'turbo-pancake-update.exe'

    Write-Host '== package and installer'
    $pkg = Join-Path $TargetDir "release-$Version.tppk"
    $packOut = & $tool pack --version $Version --input $client --out $pkg
    if ($LASTEXITCODE -ne 0) { Fail "pack failed: $packOut" }
    Set-Var 'TURBO_PANCAKE_SETUP_PAYLOAD' $pkg
    Cargo @('build', '--release', '--locked', '--target-dir', $TargetDir, '-p', 'turbo-pancake-installer') 'installer'
    $setup = Join-Path $win "turbo-pancake-setup-$Version.exe"
    Copy-Item (Join-Path $bin 'turbo-pancake-setup.exe') $setup
    $setupBytes = (Get-Item $setup).Length
    if ($setupBytes -gt 30MB) { Fail "installer is $setupBytes bytes (> 30 MB, spec section 13)" }
} finally {
    foreach ($k in $buildVars.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k]) }
}

Write-Host '== server bundle'
$server = Join-Path $Out 'server'
New-Item -ItemType Directory -Path $server -Force | Out-Null
Push-Location $workspacePath
try {
    & git archive --format=tar.gz -o (Join-Path $server 'src.tar.gz') $tree
    if ($LASTEXITCODE -ne 0) { Fail 'git archive failed' }
    # install-server.sh from the same tree (LF line endings as committed).
    $bytes = & git cat-file blob "${tree}:deploy/linux/install-server.sh" | Out-String
    if ($LASTEXITCODE -ne 0) { Fail 'install-server.sh not in the tree' }
} finally { Pop-Location }
[IO.File]::WriteAllText((Join-Path $server 'install-server.sh'), ($bytes -replace "`r`n", "`n").TrimEnd("`n") + "`n", (New-Object Text.UTF8Encoding($false)))

$signed = $false
if ($UpdateKey) {
    Write-Host "== signed update ($UpdateKeyId, expires in $UpdateExpiresDays days)"
    $update = Join-Path $Out 'update'
    # The client only: the launcher is replaced by the installer, not by updates (ADR 0016).
    $signIn = Join-Path $TargetDir "release-$Version-update-input"
    if (Test-Path $signIn) { Remove-Item -Recurse -Force $signIn }
    New-Item -ItemType Directory -Path $signIn | Out-Null
    Copy-Item (Join-Path $client 'turbo-pancake-desktop.exe') $signIn
    $signOut = & $tool sign --key $UpdateKey --id $UpdateKeyId --version $Version --input $signIn --out $update --expires-days $UpdateExpiresDays --notes $Notes
    if ($LASTEXITCODE -ne 0) { Fail "sign failed: $signOut" }
    $verifyOut = & $tool verify --manifest (Join-Path $update 'manifest.json') --pubkeys $UpdatePubkeys
    if ($LASTEXITCODE -ne 0) { Fail "manifest does not verify with the pinned keys: $verifyOut" }
    $signed = $true
}

$rustc = (& rustc --version 2>$null)
if (-not $rustc) { $rustc = 'unknown' }
$files = @(Files-Of $Out | Where-Object { $_ -ne 'release.json' })
$info = [ordered]@{
    product = 'turbo-pancake'
    version = $Version
    workspace_version = $workspaceVersion
    commit = $commit
    dirty = $dirty
    built_at_utc = $t0.ToString('yyyy-MM-ddTHH:mm:ssZ')
    rustc = "$rustc".Trim()
    client = [ordered]@{
        default_server_built_in = [bool]$DefaultServer
        private_ca_built_in = [bool]$DefaultServerCa
        update_key_ids = $keyIds
        installer_bytes = $setupBytes
    }
    server = [ordered]@{
        source = 'src.tar.gz (git archive), built on the host by install-server.sh'
        reports_version = $workspaceVersion
    }
    signed_update = $signed
    files = @($files | ForEach-Object { [ordered]@{ path = $_; bytes = (Get-Item (Join-Path $Out $_)).Length } })
}
[IO.File]::WriteAllText((Join-Path $Out 'release.json'), ($info | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding($false)))

$lines = foreach ($rel in (Files-Of $Out)) { "$(Sha256 (Join-Path $Out $rel))  $rel" }
[IO.File]::WriteAllText((Join-Path $Out 'SHA256SUMS'), (($lines -join "`n") + "`n"), (New-Object Text.UTF8Encoding($false)))
$check = Test-Sums $Out
if (-not $check[0]) { Fail "self-check of SHA256SUMS failed: $($check[1])" }

$seconds = [int]([DateTime]::UtcNow - $t0).TotalSeconds
Write-Host ("release {0} ({1}{2}): installer {3:N1} MiB, {4}, signed update: {5}, {6} s" -f `
    $Version, $commit.Substring(0, 7), $(if ($dirty) { ', dirty' } else { '' }), ($setupBytes / 1MB), $check[1], $signed, $seconds)
Write-Host "release: $Out"
if ($Version -ne $workspaceVersion) {
    Write-Host "release: note: the server reports $workspaceVersion (workspace version); bump [workspace.package] version for a server release"
}

if ($Report) {
    $md = @("# Release build $Version", '',
        "Date: $($t0.ToString('yyyy-MM-dd')). Script: ``deploy/release.ps1``. Commit ``$($commit.Substring(0, 7))``$(if ($dirty) { ' + uncommitted tracked changes' } else { '' }); $($info.rustc).", '',
        '| File | Bytes | SHA-256 |', '|---|---|---|')
    foreach ($rel in (Files-Of $Out)) {
        $md += "| ``$rel`` | $((Get-Item (Join-Path $Out $rel)).Length) | ``$(Sha256 (Join-Path $Out $rel))`` |"
    }
    $md += ''
    $md += "* Installer: $([math]::Round($setupBytes / 1MB, 1)) MiB (limit 30 MB)."
    $md += "* Default server built in: $([bool]$DefaultServer); private CA built in: $([bool]$DefaultServerCa); update keys pinned: $($keyIds.Count); signed update: $signed."
    $md += "* Checksums: $($check[1]) (``release.ps1 -Verify``)."
    $md += "* Build time: $seconds s."
    [IO.File]::WriteAllText($Report, (($md -join "`n") + "`n"), (New-Object Text.UTF8Encoding($false)))
}
exit 0
