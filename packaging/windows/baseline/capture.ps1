# MonkeyFence Windows 旧交付生命周期基线采集脚本(GitHub Issue #15 / T0d)
#
# 目的:以只读方式采集「当前交付形态」的机器可读事实——cargo/GPUI 启动形态、
# 二进制、用户与项目数据目录、安装残留、现有备份能力;缺失的交付能力
# (MSI/bootstrapper/uninstaller/updater/side-by-side/Service/自启动/在线备份等)
# 必须显式输出 status=absent,不得省略,也不得把 spec 附录 A8 的未来设计误报为现状。
#
# 只读契约(不可违反):
#   - 只允许:读文件/目录元数据(名称、类型、存在性)、读注册表、查询进程/服务/
#     计划任务/命令(PATH 解析)、读仓库 Cargo.toml 元数据。
#   - 禁止:写注册表、写自启动项、写用户数据库、写应用目录、读取任何数据库文件
#     内容、读取/输出 Secret/Provider/API Key。
#   - 唯一允许的写:-OutputPath 指定的输出文件(且不创建父目录);默认输出 stdout。
#
# 兼容性:Windows PowerShell 5.1 与 PowerShell 7+。确定性:同一输入(含注入路径)
# 在同一 PowerShell 版本下连跑两次输出字节一致;枚举按 OrdinalIgnoreCase 排序;
# 不输出时间戳、PID、文件大小、随机值。
#
# 用法示例:
#   powershell -NoProfile -ExecutionPolicy Bypass -File capture.ps1
#   pwsh -NoProfile -File capture.ps1 -UserHome <dir> -ProjectRoot <dir> -RepoRoot <dir> `
#       -LocalAppData <dir> -AppDataRoaming <dir> -OutputPath <file.json>

[CmdletBinding()]
param(
    # 用户主目录(默认真实 %USERPROFILE%);派生 ~/.monkeyfence 用户数据目录探测。
    [string]$UserHome = '',
    # 项目根(默认 = RepoRoot);派生 <project>/.mf-agent 与 <project>/.monkeyfence 探测。
    [string]$ProjectRoot = '',
    # 仓库根(默认 = 脚本所在目录向上三级);用于二进制与 Cargo.toml 元数据探测。
    [string]$RepoRoot = '',
    # Per-user AppData\Local(默认真实 %LOCALAPPDATA%);用于未来安装位置残留探测。
    [string]$LocalAppData = '',
    # Roaming AppData(默认真实 %APPDATA%);用于启动文件夹自启动残留探测。
    [string]$AppDataRoaming = '',
    # 输出文件(UTF-8 无 BOM)。不指定则输出 stdout。
    [string]$OutputPath = ''
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

$script:OrdinalIgnoreCase = [System.StringComparer]::OrdinalIgnoreCase

# ---------------------------------------------------------------------------
# 基础只读工具
# ---------------------------------------------------------------------------

function Convert-ToFullPath {
    param([string]$Path)
    return [System.IO.Path]::GetFullPath($Path)
}

function Test-FileExists {
    param([string]$Path)
    return [bool](Test-Path -LiteralPath $Path -PathType Leaf -ErrorAction SilentlyContinue)
}

function Test-DirExists {
    param([string]$Path)
    return [bool](Test-Path -LiteralPath $Path -PathType Container -ErrorAction SilentlyContinue)
}

function Get-FilePresenceProbe {
    param([string]$Path)
    try {
        return @{
            probe_ok = $true
            present = [bool](Test-Path -LiteralPath $Path -PathType Leaf -ErrorAction Stop)
        }
    } catch {
        return @{ probe_ok = $false; present = $false }
    }
}

function Get-DirectoryPresenceProbe {
    param([string]$Path)
    try {
        return @{
            probe_ok = $true
            present = [bool](Test-Path -LiteralPath $Path -PathType Container -ErrorAction Stop)
        }
    } catch {
        return @{ probe_ok = $false; present = $false }
    }
}

function Get-ApplicationCommandProbe {
    param([string]$Name)
    try {
        $candidates = @(Get-Command -Name $Name -CommandType Application -All -ErrorAction Stop)
        if ($candidates.Count -eq 0) {
            return @{ probe_ok = $true; found = $false; source = $null }
        }
        return @{ probe_ok = $true; found = $true; source = "$($candidates[0].Source)" }
    } catch [System.Management.Automation.CommandNotFoundException] {
        return @{ probe_ok = $true; found = $false; source = $null }
    } catch {
        return @{ probe_ok = $false; found = $false; source = $null }
    }
}

function Test-PathWithinOrEqual {
    param([string]$Candidate, [string]$Root)
    $candidateFull = Convert-ToFullPath -Path $Candidate
    $rootFull = Convert-ToFullPath -Path $Root
    $trimChars = [char[]]@(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
    $rootTrimmed = $rootFull.TrimEnd($trimChars)
    if ([string]::Equals($candidateFull, $rootTrimmed, [System.StringComparison]::OrdinalIgnoreCase)) {
        return $true
    }
    $prefix = $rootTrimmed + [System.IO.Path]::DirectorySeparatorChar
    return $candidateFull.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)
}

function Test-OutputPathSyntaxSupported {
    param([string]$Path)
    # 仅接受普通本地 DOS/相对路径。拒绝 UNC、extended/device namespace
    # 与 alternate data stream/8.3 short name,避免同一文件的别名绕过
    # protectedRoots。输出路径不需要兼容 short name,宁可 fail-closed。
    if ($Path.StartsWith('\\') -or $Path.StartsWith('//')) { return $false }
    if ($Path -match '(?i)(^|[\\/])[^\\/]*~[0-9]+') { return $false }
    $firstColon = $Path.IndexOf(':')
    if ($firstColon -ge 0) {
        if ($firstColon -ne 1 -or $Path.Length -lt 3 -or $Path[0] -notmatch '[A-Za-z]') {
            return $false
        }
        if ($Path.IndexOf(':', 2) -ge 0) { return $false }
    }
    return $true
}

# 拒绝经 symlink/junction/reparse-point 把看似安全的输出目录重定向进
# 用户数据库或应用目录。父目录必须已经存在,所以可以逐级检查。
function Test-ParentChainHasReparsePoint {
    param([string]$ParentPath)
    $item = Get-Item -LiteralPath $ParentPath -Force -ErrorAction Stop
    while ($null -ne $item) {
        if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            return $true
        }
        $item = $item.Parent
    }
    return $false
}

# OrdinalIgnoreCase 比较字符串(与 culture 无关,保证跨机器/跨区域设置字节稳定)。
function Compare-OrdinalIgnoreCase {
    param([string]$Left, [string]$Right)
    return [string]::Compare($Left, $Right, [System.StringComparison]::OrdinalIgnoreCase)
}

# 按指定属性对对象数组做稳定插入排序(OrdinalIgnoreCase;避免 Sort-Object 的
# culture 依赖)。输入规模很小(目录条目/探测结果),O(n^2) 足够。
function Sort-OrdinalBy {
    param([object[]]$Rows, [string]$Property)
    $list = New-Object System.Collections.ArrayList
    foreach ($row in $Rows) {
        $key = "$($row.$Property)"
        $inserted = $false
        for ($i = 0; $i -lt $list.Count; $i++) {
            $existing = "$($list[$i].$Property)"
            if ((Compare-OrdinalIgnoreCase -Left $key -Right $existing) -lt 0) {
                $null = $list.Insert($i, $row)
                $inserted = $true
                break
            }
        }
        if (-not $inserted) { $null = $list.Add($row) }
    }
    return @($list.ToArray())
}

# 浅层列出目录条目(仅名称+类型;不读内容、不读大小、不递归),OrdinalIgnoreCase 排序。
function Get-SortedDirEntries {
    param([string]$DirectoryPath)
    $dirProbe = Get-DirectoryPresenceProbe -Path $DirectoryPath
    if (-not $dirProbe['probe_ok']) { return @{ probe_ok = $false; entries = @() } }
    if (-not $dirProbe['present']) { return @{ probe_ok = $true; entries = @() } }
    $kinds = @{}
    try {
        $items = @(Get-ChildItem -LiteralPath $DirectoryPath -Force -ErrorAction Stop)
    } catch {
        return @{ probe_ok = $false; entries = @() }
    }
    foreach ($item in $items) {
        if ($item.PSIsContainer) { $kinds[$item.Name] = 'directory' } else { $kinds[$item.Name] = 'file' }
    }
    # 字符串键排序:[Array]::Sort + StringComparer(OrdinalIgnoreCase)。
    $names = @($kinds.Keys)
    [Array]::Sort($names, $script:OrdinalIgnoreCase)
    $sorted = @()
    foreach ($name in $names) {
        $sorted += [pscustomobject]@{ name = $name; kind = $kinds[$name] }
    }
    return @{ probe_ok = $true; entries = @($sorted) }
}

# 对固定 well-known 名单逐项探测存在性(名单顺序即声明顺序,确定性)。
function Get-WellKnownEntries {
    param([string]$BaseDir, [object[]]$Specs)
    $result = @()
    $probeOk = $true
    foreach ($spec in $Specs) {
        $path = Join-Path $BaseDir $spec['name']
        if ($spec['kind'] -eq 'directory') { $probe = Get-DirectoryPresenceProbe -Path $path }
        else { $probe = Get-FilePresenceProbe -Path $path }
        if (-not $probe['probe_ok']) { $probeOk = $false }
        $present = $probe['present']
        $result += [pscustomobject]@{ name = $spec['name']; kind = $spec['kind']; present = $present }
    }
    return @{ probe_ok = $probeOk; entries = @($result) }
}

# ---------------------------------------------------------------------------
# 注册表只读工具(.NET API,显式只读打开;探测失败以 ok=false 上报,不冒充 absent)
# ---------------------------------------------------------------------------

function Get-RegistrySubKeyNames {
    param([bool]$Hkcu, [string]$SubKeyPath)
    try {
        $root = [Microsoft.Win32.Registry]::LocalMachine
        if ($Hkcu) { $root = [Microsoft.Win32.Registry]::CurrentUser }
        $key = $root.OpenSubKey($SubKeyPath)
        if ($null -eq $key) { return @{ ok = $true; names = @() } }
        $names = @($key.GetSubKeyNames())
        $key.Close()
        [Array]::Sort($names, [System.StringComparer]::OrdinalIgnoreCase)
        return @{ ok = $true; names = $names }
    } catch {
        return @{ ok = $false; names = @() }
    }
}

function Get-RegistryValueNames {
    param([bool]$Hkcu, [string]$SubKeyPath)
    try {
        $root = [Microsoft.Win32.Registry]::LocalMachine
        if ($Hkcu) { $root = [Microsoft.Win32.Registry]::CurrentUser }
        $key = $root.OpenSubKey($SubKeyPath)
        if ($null -eq $key) { return @{ ok = $true; names = @() } }
        $names = @($key.GetValueNames())
        $key.Close()
        [Array]::Sort($names, [System.StringComparer]::OrdinalIgnoreCase)
        return @{ ok = $true; names = $names }
    } catch {
        return @{ ok = $false; names = @() }
    }
}

function Get-RegistryStringValue {
    param([bool]$Hkcu, [string]$KeyPath, [string]$ValueName)
    try {
        $root = [Microsoft.Win32.Registry]::LocalMachine
        if ($Hkcu) { $root = [Microsoft.Win32.Registry]::CurrentUser }
        $key = $root.OpenSubKey($KeyPath)
        if ($null -eq $key) { return @{ ok = $true; value = $null } }
        $value = $key.GetValue($ValueName)
        $key.Close()
        if ($null -eq $value) { return @{ ok = $true; value = $null } }
        return @{ ok = $true; value = "$value" }
    } catch {
        return @{ ok = $false; value = $null }
    }
}

# ---------------------------------------------------------------------------
# 输入解析(默认值 → 全部规范化为绝对路径)
# ---------------------------------------------------------------------------

if ($UserHome -eq '') {
    $UserHome = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
}
if ($RepoRoot -eq '') {
    $RepoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..\..')).Path
}
if ($ProjectRoot -eq '') { $ProjectRoot = $RepoRoot }
if ($LocalAppData -eq '') {
    $LocalAppData = [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
}
if ($AppDataRoaming -eq '') {
    $AppDataRoaming = [Environment]::GetFolderPath([Environment+SpecialFolder]::ApplicationData)
}

if ($UserHome -eq '' -or $RepoRoot -eq '' -or $LocalAppData -eq '' -or $AppDataRoaming -eq '') {
    throw '无法解析默认系统目录(用户主目录/AppData),请显式传入 -UserHome 等参数'
}

$UserHome = Convert-ToFullPath -Path $UserHome
$ProjectRoot = Convert-ToFullPath -Path $ProjectRoot
$RepoRoot = Convert-ToFullPath -Path $RepoRoot
$LocalAppData = Convert-ToFullPath -Path $LocalAppData
$AppDataRoaming = Convert-ToFullPath -Path $AppDataRoaming

# ---------------------------------------------------------------------------
# runtime_facts:当前 cargo/GPUI 启动形态与二进制
# ---------------------------------------------------------------------------

function Get-WorkspaceMeta {
    param([string]$Root)
    $version = 'unknown'
    $members = @()
    $tomlPath = Join-Path $Root 'Cargo.toml'
    if (Test-FileExists -Path $tomlPath) {
        $text = [System.IO.File]::ReadAllText($tomlPath)
        $versionMatch = [regex]::Match($text, '(?ms)\[workspace\.package\].*?version\s*=\s*"([^"]+)"')
        if ($versionMatch.Success) { $version = $versionMatch.Groups[1].Value }
        # 只解析 [workspace] 的 members 块(default-members 是其子集,不重复计入)。
        $membersMatch = [regex]::Match($text, '(?ms)^members\s*=\s*\[(.*?)\]')
        if ($membersMatch.Success) {
            foreach ($m in [regex]::Matches($membersMatch.Groups[1].Value, '"(crates/[^"]+)"')) {
                $members += $m.Groups[1].Value
            }
        }
    }
    return @{ version = $version; members = $members }
}

$workspaceMeta = Get-WorkspaceMeta -Root $RepoRoot

$binNames = @(
    'mf-broker', 'mf-install-host', 'mf-root-host', 'mfctl', 'monkeyfence',
    'monkeyfence-bootstrapper', 'monkeyfence-core', 'monkeyfence-launcher',
    'monkeyfence-picker', 'monkeyfence-tray', 'monkeyfence-updater'
)
$targetDirs = @('target', 'target-dev')
$profiles = @('debug', 'release')

# 仓库 target 目录内的产品二进制存在性(路径以 RepoRoot 相对形式输出,避免机器绝对路径)。
$repoTargetRows = @()
foreach ($bin in $binNames) {
    foreach ($targetDir in $targetDirs) {
        foreach ($profile in $profiles) {
            $relative = ($targetDir + '\' + $profile + '\' + $bin + '.exe')
            $fileProbe = Get-FilePresenceProbe -Path (Join-Path $RepoRoot $relative)
            $repoTargetRows += [pscustomobject]@{
                bin = $bin; target_dir = $targetDir; profile = $profile
                probe_ok = $fileProbe['probe_ok']
                exists = $fileProbe['present']
                relative_path = $relative
            }
        }
    }
}
$repoTargetBins = @(Sort-OrdinalBy -Rows $repoTargetRows -Property 'relative_path')

# PATH 查找(机器相关,仅供人工诊断;契约测试不对其值做严格断言)。
$pathLookupRows = @()
foreach ($bin in $binNames) {
    $commandProbe = Get-ApplicationCommandProbe -Name $bin
    $pathLookupRows += [pscustomobject]@{
        bin = $bin
        probe_ok = $commandProbe['probe_ok']
        found = $commandProbe['found']
        source = $commandProbe['source']
    }
}
$pathLookup = @(Sort-OrdinalBy -Rows $pathLookupRows -Property 'bin')

$gpuiPlatformDirPresent = Test-DirExists -Path (Join-Path $RepoRoot 'vendor\gpui_platform')

$startupForm = [pscustomobject]@{
    form = 'cargo-run-gpui-desktop'
    process_model = 'single-process'
    run_command = 'cargo run / cargo build(根 Cargo.toml default-members 默认目标为主程序)'
    main_bin = [pscustomobject]@{
        name = 'monkeyfence'
        crate = 'crates/mf'
        ui = 'gpui-desktop(gpui 为本地 path 依赖 + vendor/gpui_platform)'
    }
    cli_bin = [pscustomobject]@{
        name = 'mfctl'
        crate = 'crates/mfctl'
        transport = 'named-pipe-per-process(能力令牌 MF_RUN_TOKEN)'
    }
    workspace_version = $workspaceMeta['version']
    workspace_members = @($workspaceMeta['members'])
    gpui = [pscustomobject]@{
        dependency_kind = 'path'
        vendored_platform_dir_present = $gpuiPlatformDirPresent
    }
}

$runtimeFacts = [pscustomobject]@{
    startup_form = $startupForm
    binaries = [pscustomobject]@{
        repo_target = $repoTargetBins
        path_lookup = @($pathLookup)
    }
}

# ---------------------------------------------------------------------------
# data_locations:用户与项目数据目录(只探测名称/存在性,绝不读取文件内容)
# ---------------------------------------------------------------------------

$mfUserDir = Join-Path $UserHome '.monkeyfence'
$mfAgentDir = Join-Path $ProjectRoot '.mf-agent'
$projectMfDir = Join-Path $ProjectRoot '.monkeyfence'

$userDirProbe = Get-DirectoryPresenceProbe -Path $mfUserDir
$userWellKnownProbe = Get-WellKnownEntries -BaseDir $mfUserDir -Specs @(
    @{ name = 'config.toml'; kind = 'file' },
    @{ name = 'catalog-v1.db'; kind = 'file' },
    @{ name = 'session.json'; kind = 'file' },
    @{ name = 'ui-prefs.json'; kind = 'file' },
    @{ name = 'skills'; kind = 'directory' }
)
$userEntriesProbe = Get-SortedDirEntries -DirectoryPath $mfUserDir

$projectDirProbe = Get-DirectoryPresenceProbe -Path $mfAgentDir
$projectWellKnownProbe = Get-WellKnownEntries -BaseDir $mfAgentDir -Specs @(
    @{ name = 'workflow-v1.db'; kind = 'file' },
    @{ name = 'work-items.json'; kind = 'file' }
)

$projectLegacyProbe = Get-WellKnownEntries -BaseDir $mfAgentDir -Specs @(
    @{ name = 'orchestration.db'; kind = 'file' },
    @{ name = 'workspaces.json'; kind = 'file' }
)
$projectEntriesProbe = Get-SortedDirEntries -DirectoryPath $mfAgentDir
$projectMfDirProbe = Get-DirectoryPresenceProbe -Path $projectMfDir
$projectMfEntriesProbe = Get-SortedDirEntries -DirectoryPath $projectMfDir

$dataLocations = [pscustomobject]@{
    user = [pscustomobject]@{
        dir = $mfUserDir
        probe_ok = ($userDirProbe['probe_ok'] -and $userWellKnownProbe['probe_ok'] -and $userEntriesProbe['probe_ok'])
        exists = $userDirProbe['present']
        catalog_schema_version_current = 1
        redirection_env_vars = @('MF_CATALOG_DB', 'MONKEYFENCE_SESSION_PATH')
        well_known = @($userWellKnownProbe['entries'])
        entries = @($userEntriesProbe['entries'])
    }
    project = [pscustomobject]@{
        dir = $mfAgentDir
        probe_ok = ($projectDirProbe['probe_ok'] -and $projectWellKnownProbe['probe_ok'] -and $projectLegacyProbe['probe_ok'] -and $projectEntriesProbe['probe_ok'])
        exists = $projectDirProbe['present']
        project_schema_version_current = 6
        well_known = @($projectWellKnownProbe['entries'])
        legacy_residue = @($projectLegacyProbe['entries'])
        entries = @($projectEntriesProbe['entries'])
    }
    project_monkeyfence = [pscustomobject]@{
        dir = $projectMfDir
        probe_ok = ($projectMfDirProbe['probe_ok'] -and $projectMfEntriesProbe['probe_ok'])
        exists = $projectMfDirProbe['present']
        purpose = 'project 级技能目录 <project>/.monkeyfence/skills(mf-skills)'
        entries = @($projectMfEntriesProbe['entries'])
    }
}

# ---------------------------------------------------------------------------
# residue:注册表/服务/计划任务/启动文件夹/进程/CLI 配置备份/未来安装位置
# ---------------------------------------------------------------------------

$uninstallKeySpecs = @(
    @{ hive = 'HKCU'; path = 'Software\Microsoft\Windows\CurrentVersion\Uninstall' },
    @{ hive = 'HKLM'; path = 'Software\Microsoft\Windows\CurrentVersion\Uninstall' },
    @{ hive = 'HKLM'; path = 'Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall' }
)

$uninstallMatchRows = @()
$uninstallProbeOk = $true
$uninstallWithUninstallString = 0
$msiUninstallEntries = 0
foreach ($spec in $uninstallKeySpecs) {
    $probe = Get-RegistrySubKeyNames -Hkcu ($spec['hive'] -eq 'HKCU') -SubKeyPath $spec['path']
    if (-not $probe['ok']) { $uninstallProbeOk = $false; continue }
    foreach ($subName in $probe['names']) {
        $subKeyPath = ($spec['path'] + '\' + $subName)
        $canonicalPath = ($spec['hive'] + '\' + $spec['path'] + '\' + $subName)
        $displayProbe = Get-RegistryStringValue -Hkcu ($spec['hive'] -eq 'HKCU') -KeyPath $subKeyPath -ValueName 'DisplayName'
        if (-not $displayProbe['ok']) { $uninstallProbeOk = $false; continue }
        $displayName = $displayProbe['value']
        $isMatch = $false
        if ("$displayName" -match 'monkeyfence') { $isMatch = $true }
        if ($subName -match 'monkeyfence') { $isMatch = $true }
        if ($isMatch) {
            $uninstallProbe = Get-RegistryStringValue -Hkcu ($spec['hive'] -eq 'HKCU') -KeyPath $subKeyPath -ValueName 'UninstallString'
            $windowsInstallerProbe = Get-RegistryStringValue -Hkcu ($spec['hive'] -eq 'HKCU') -KeyPath $subKeyPath -ValueName 'WindowsInstaller'
            if (-not $uninstallProbe['ok'] -or -not $windowsInstallerProbe['ok']) {
                $uninstallProbeOk = $false
            }
            $uninstallString = $uninstallProbe['value']
            if (-not [string]::IsNullOrEmpty($uninstallString)) { $uninstallWithUninstallString += 1 }
            $windowsInstaller = $windowsInstallerProbe['value']
            if ("$windowsInstaller" -eq '1') { $msiUninstallEntries += 1 }
            $uninstallMatchRows += [pscustomobject]@{
                key_path = $canonicalPath
                display_name = "$displayName"
            }
        }
    }
}
$uninstallMatches = @(Sort-OrdinalBy -Rows $uninstallMatchRows -Property 'key_path')

$runKeySpecs = @(
    @{ hive = 'HKCU'; path = 'Software\Microsoft\Windows\CurrentVersion\Run' },
    @{ hive = 'HKCU'; path = 'Software\Microsoft\Windows\CurrentVersion\RunOnce' },
    @{ hive = 'HKLM'; path = 'Software\Microsoft\Windows\CurrentVersion\Run' },
    @{ hive = 'HKLM'; path = 'Software\Microsoft\Windows\CurrentVersion\RunOnce' }
)

$runKeyMatchRows = @()
$runKeyProbeOk = $true
foreach ($spec in $runKeySpecs) {
    $probe = Get-RegistryValueNames -Hkcu ($spec['hive'] -eq 'HKCU') -SubKeyPath $spec['path']
    if (-not $probe['ok']) { $runKeyProbeOk = $false; continue }
    foreach ($valueName in $probe['names']) {
        if ($valueName -match 'monkeyfence') {
            $runKeyMatchRows += [pscustomobject]@{
                key_path = ($spec['hive'] + '\' + $spec['path'])
                value_name = $valueName
            }
        }
    }
}
$runKeyMatches = @(Sort-OrdinalBy -Rows $runKeyMatchRows -Property 'value_name')

$startupFolderPath = Join-Path $AppDataRoaming 'Microsoft\Windows\Start Menu\Programs\Startup'
$startupFolderMatches = @()
$startupFolderProbeOk = $true
try {
    if (Test-Path -LiteralPath $startupFolderPath -PathType Container -ErrorAction Stop) {
        foreach ($item in @(Get-ChildItem -LiteralPath $startupFolderPath -Force -ErrorAction Stop)) {
            if ($item.Name -match 'monkeyfence') { $startupFolderMatches += $item.Name }
        }
        [Array]::Sort($startupFolderMatches, [System.StringComparer]::OrdinalIgnoreCase)
    }
} catch {
    $startupFolderProbeOk = $false
}

$serviceMatches = @()
$autoStartServiceMatches = @()
$serviceProbeOk = $true
try {
    foreach ($svc in @(Get-Service -ErrorAction Stop)) {
        if ($svc.Name -match 'monkeyfence' -or "$($svc.DisplayName)" -match 'monkeyfence') {
            $serviceMatches += $svc.Name
            if ("$($svc.StartType)" -match '^Automatic') {
                $autoStartServiceMatches += $svc.Name
            }
        }
    }
    [Array]::Sort($serviceMatches, [System.StringComparer]::OrdinalIgnoreCase)
    [Array]::Sort($autoStartServiceMatches, [System.StringComparer]::OrdinalIgnoreCase)
} catch {
    $serviceProbeOk = $false
}

$taskMatches = @()
$taskProbeOk = $true
try {
    foreach ($task in @(Get-ScheduledTask -ErrorAction Stop)) {
        if ($task.TaskName -match 'monkeyfence' -or "$($task.TaskPath)" -match 'monkeyfence') {
            $taskMatches += ($task.TaskPath + $task.TaskName)
        }
    }
    [Array]::Sort($taskMatches, [System.StringComparer]::OrdinalIgnoreCase)
} catch {
    $taskProbeOk = $false
}

# 进程探测:只输出名称级结果(不输出 PID/计数,二者在两次运行间不稳定)。
$candidateProcessNames = @($binNames)
$runningProcesses = @()
foreach ($name in $candidateProcessNames) {
    $procs = @(Get-Process -Name $name -ErrorAction SilentlyContinue)
    if ($procs.Count -gt 0) { $runningProcesses += $name }
}

# hooks 写入 Agent CLI 配置前生成的 *.monkeyfence-backup-* 文件残留
# (仅探测常见 CLI 配置目录;完整目录清单见 README 限制说明)。
$cliConfigBackupDirNames = @('.claude', '.codex', '.config', '.gemini')
$cliConfigBackupDirsProbed = @()
$cliConfigBackupMatchRows = @()
$cliConfigBackupProbeOk = $true
foreach ($dirName in $cliConfigBackupDirNames) {
    $dirPath = Join-Path $UserHome $dirName
    $cliConfigBackupDirsProbed += $dirPath
    $dirProbe = Get-DirectoryPresenceProbe -Path $dirPath
    if (-not $dirProbe['probe_ok']) {
        $cliConfigBackupProbeOk = $false
        continue
    }
    if ($dirProbe['present']) {
        try {
            foreach ($item in @(Get-ChildItem -LiteralPath $dirPath -Force -Filter '*.monkeyfence-backup-*' -ErrorAction Stop)) {
                $cliConfigBackupMatchRows += [pscustomobject]@{ dir = $dirPath; name = $item.Name }
            }
        } catch {
            $cliConfigBackupProbeOk = $false
        }
    }
}
$cliConfigBackupMatches = @(Sort-OrdinalBy -Rows $cliConfigBackupMatchRows -Property 'name')

# 未来(附录 A8 / spec §13.4 / §11.1)的 per-user 安装位置;当前基线必须全部 absent。
$bundleRoot = Join-Path $LocalAppData 'Programs\MonkeyFence'
$versionsDir = Join-Path $bundleRoot 'versions'
$currentJson = Join-Path $bundleRoot 'current.json'
$discoveryDir = Join-Path $LocalAppData 'MonkeyFence'
$discoveryJson = Join-Path $discoveryDir 'discovery.json'
$serviceV1Db = Join-Path $mfUserDir 'service-v1.db'

$bundleRootProbe = Get-DirectoryPresenceProbe -Path $bundleRoot
$versionsDirProbe = Get-DirectoryPresenceProbe -Path $versionsDir
$currentJsonProbe = Get-FilePresenceProbe -Path $currentJson
$discoveryDirProbe = Get-DirectoryPresenceProbe -Path $discoveryDir
$discoveryJsonProbe = Get-FilePresenceProbe -Path $discoveryJson
$serviceV1DbProbe = Get-FilePresenceProbe -Path $serviceV1Db
$bundleRootPresent = $bundleRootProbe['present']
$versionsDirPresent = $versionsDirProbe['present']
$currentJsonPresent = $currentJsonProbe['present']
$discoveryJsonPresent = $discoveryJsonProbe['present']
$serviceV1DbPresent = $serviceV1DbProbe['present']

$versionDirNames = @()
$versionDirEnumerationOk = $versionsDirProbe['probe_ok']
if ($versionsDirPresent) {
    try {
        foreach ($item in @(Get-ChildItem -LiteralPath $versionsDir -Force -ErrorAction Stop)) {
            if ($item.PSIsContainer) { $versionDirNames += $item.Name }
        }
        [Array]::Sort($versionDirNames, [System.StringComparer]::OrdinalIgnoreCase)
    } catch {
        $versionDirEnumerationOk = $false
    }
}

$residue = [pscustomobject]@{
    registry = [pscustomobject]@{
        uninstall_keys = [pscustomobject]@{
            probe_ok = $uninstallProbeOk
            probed_paths = @(
                'HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall',
                'HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall',
                'HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
            )
            matches = $uninstallMatches
        }
        run_keys = [pscustomobject]@{
            probe_ok = $runKeyProbeOk
            probed_paths = @(
                'HKCU\Software\Microsoft\Windows\CurrentVersion\Run',
                'HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce',
                'HKLM\Software\Microsoft\Windows\CurrentVersion\Run',
                'HKLM\Software\Microsoft\Windows\CurrentVersion\RunOnce'
            )
            matches = $runKeyMatches
        }
    }
    startup_folder = [pscustomobject]@{
        path = $startupFolderPath
        probe_ok = $startupFolderProbeOk
        matches = @($startupFolderMatches)
    }
    services = [pscustomobject]@{
        probe_ok = $serviceProbeOk
        matches = @($serviceMatches)
        auto_start_matches = @($autoStartServiceMatches)
    }
    scheduled_tasks = [pscustomobject]@{ probe_ok = $taskProbeOk; matches = @($taskMatches) }
    processes = [pscustomobject]@{
        names_probed = @($candidateProcessNames)
        running = @($runningProcesses)
    }
    agent_cli_config_backups = [pscustomobject]@{
        probe_ok = $cliConfigBackupProbeOk
        dirs_probed = @($cliConfigBackupDirsProbed)
        matches = $cliConfigBackupMatches
    }
    install_locations = [pscustomobject]@{
        per_user_bundle_root = [pscustomobject]@{
            path = $bundleRoot; probe_ok = $bundleRootProbe['probe_ok']; present = $bundleRootPresent
        }
        versions_dir = [pscustomobject]@{
            path = $versionsDir; probe_ok = $versionDirEnumerationOk
            present = $versionsDirPresent; version_dir_names = @($versionDirNames)
        }
        current_json = [pscustomobject]@{
            path = $currentJson; probe_ok = $currentJsonProbe['probe_ok']; present = $currentJsonPresent
        }
        discovery_dir = [pscustomobject]@{
            path = $discoveryDir; probe_ok = $discoveryDirProbe['probe_ok']; present = $discoveryDirProbe['present']
        }
        discovery_json = [pscustomobject]@{
            path = $discoveryJson; probe_ok = $discoveryJsonProbe['probe_ok']; present = $discoveryJsonPresent
        }
    }
}

# ---------------------------------------------------------------------------
# delivery_capabilities:当前不存在的交付能力必须显式 status=absent
# status ∈ present | absent | probe_failed(探测本身失败时不得谎报 absent)
# ---------------------------------------------------------------------------

function Get-BinProbeStatus {
    param([string]$BinName)
    $probeFailed = $false
    foreach ($row in $repoTargetBins) {
        if ($row.bin -ne $BinName) { continue }
        if ($row.exists) { return 'present' }
        if (-not $row.probe_ok) { $probeFailed = $true }
    }
    foreach ($row in $pathLookup) {
        if ($row.bin -ne $BinName) { continue }
        if ($row.found) { return 'present' }
        if (-not $row.probe_ok) { $probeFailed = $true }
    }
    if ($probeFailed) { return 'probe_failed' }
    return 'absent'
}

function Test-ProcessRunning {
    param([string]$Name)
    foreach ($n in $runningProcesses) {
        if ($n -eq $Name) { return $true }
    }
    return $false
}

# 未来伴生/核心二进制:仓库 target、PATH 两处探测(进程仅作旁证)。
$futureBins = @(
    @{ cap = 'bootstrapper_exe'; bin = 'monkeyfence-bootstrapper' },
    @{ cap = 'launcher'; bin = 'monkeyfence-launcher' },
    @{ cap = 'tray'; bin = 'monkeyfence-tray' },
    @{ cap = 'picker'; bin = 'monkeyfence-picker' },
    @{ cap = 'core_service_bin'; bin = 'monkeyfence-core' },
    @{ cap = 'elevated_broker'; bin = 'mf-broker' },
    @{ cap = 'updater'; bin = 'monkeyfence-updater' }
)
$futureBinStatus = @{}
foreach ($fb in $futureBins) {
    $futureBinStatus[$fb['cap']] = (Get-BinProbeStatus -BinName $fb['bin'])
}

$autoStartMatchesTotal = $runKeyMatches.Count + $startupFolderMatches.Count + $taskMatches.Count + $autoStartServiceMatches.Count
$autoStartProbeOk = $runKeyProbeOk -and $startupFolderProbeOk -and $taskProbeOk -and $serviceProbeOk

$deliveryCapabilities = [pscustomobject]@{
    msi_installer = [pscustomobject]@{
        status = $(if ($msiUninstallEntries -gt 0) { 'present' } elseif ($uninstallProbeOk) { 'absent' } else { 'probe_failed' })
        evidence = @(
            ('uninstall_key_matches=' + $uninstallMatches.Count),
            ('windows_installer_entries=' + $msiUninstallEntries),
            ('per_user_bundle_root_present=' + $(if ($bundleRootPresent) { 'true' } else { 'false' }))
        )
    }
    bootstrapper_exe = [pscustomobject]@{
        status = $futureBinStatus['bootstrapper_exe']
        evidence = @(
            ('bootstrapper_bin_status=' + $futureBinStatus['bootstrapper_exe']),
            ('per_user_bundle_root_present=' + $(if ($bundleRootPresent) { 'true' } else { 'false' })),
            'code_fact=当前仓库无 bootstrapper 可执行目标'
        )
    }
    uninstaller = [pscustomobject]@{
        status = $(if ($uninstallWithUninstallString -gt 0) { 'present' } elseif ($uninstallProbeOk) { 'absent' } else { 'probe_failed' })
        evidence = @('uninstall_entries_with_uninstall_string=' + $uninstallWithUninstallString)
    }
    updater = [pscustomobject]@{
        status = $futureBinStatus['updater']
        evidence = @(
            ('updater_bin_status=' + $futureBinStatus['updater']),
            ('versions_dir_present=' + $(if ($versionsDirPresent) { 'true' } else { 'false' })),
            ('current_json_present=' + $(if ($currentJsonPresent) { 'true' } else { 'false' })),
            'code_fact=无更新器入口(当前交付仅 cargo 构建)'
        )
    }
    side_by_side_versions = [pscustomobject]@{
        status = $(
            if (-not $versionDirEnumerationOk -or -not $currentJsonProbe['probe_ok']) { 'probe_failed' }
            elseif ($versionDirNames.Count -gt 0 -and $currentJsonPresent) { 'present' }
            else { 'absent' }
        )
        evidence = @(
            ('versions_dir_present=' + $(if ($versionsDirPresent) { 'true' } else { 'false' })),
            ('version_dir_count=' + $versionDirNames.Count)
        )
    }
    current_json_pointer = [pscustomobject]@{
        status = $(if ($currentJsonPresent) { 'present' } elseif ($currentJsonProbe['probe_ok']) { 'absent' } else { 'probe_failed' })
        evidence = @(('current_json_present=' + $(if ($currentJsonPresent) { 'true' } else { 'false' })))
    }
    windows_service = [pscustomobject]@{
        status = $(if ($serviceMatches.Count -gt 0) { 'present' } elseif ($serviceProbeOk) { 'absent' } else { 'probe_failed' })
        evidence = @(('service_matches=' + $serviceMatches.Count))
    }
    autostart = [pscustomobject]@{
        status = $(if ($autoStartMatchesTotal -gt 0) { 'present' } elseif ($autoStartProbeOk) { 'absent' } else { 'probe_failed' })
        evidence = @(
            ('run_key_matches=' + $runKeyMatches.Count),
            ('startup_folder_matches=' + $startupFolderMatches.Count),
            ('scheduled_task_matches=' + $taskMatches.Count),
            ('auto_start_service_matches=' + $autoStartServiceMatches.Count)
        )
    }
    launcher = [pscustomobject]@{
        status = $futureBinStatus['launcher']
        evidence = @(
            ('bin_status=' + $futureBinStatus['launcher']),
            'code_fact=spec §11.2/T6 未来能力(当前不存在 launcher)'
        )
    }
    tray = [pscustomobject]@{
        status = $futureBinStatus['tray']
        evidence = @(
            ('bin_status=' + $futureBinStatus['tray']),
            'code_fact=spec §11.3/T6 未来能力(当前不存在 tray)'
        )
    }
    picker = [pscustomobject]@{
        status = $futureBinStatus['picker']
        evidence = @(
            ('bin_status=' + $futureBinStatus['picker']),
            'code_fact=spec §11.3/T6 未来能力(当前不存在 picker)'
        )
    }
    core_service_bin = [pscustomobject]@{
        status = $futureBinStatus['core_service_bin']
        evidence = @(
            ('bin_status=' + $futureBinStatus['core_service_bin']),
            ('process_running=' + $(if (Test-ProcessRunning -Name 'monkeyfence-core') { 'true' } else { 'false' })),
            'code_fact=spec §2/T6 未来能力(当前为单进程 GPUI,无独立 core bin)'
        )
    }
    elevated_broker = [pscustomobject]@{
        status = $futureBinStatus['elevated_broker']
        evidence = @(
            ('bin_status=' + $futureBinStatus['elevated_broker']),
            'code_fact=spec §10/T9 未来能力(当前无 Root Mode/Broker)'
        )
    }
    discovery_file = [pscustomobject]@{
        status = $(if ($discoveryJsonPresent) { 'present' } elseif ($discoveryJsonProbe['probe_ok']) { 'absent' } else { 'probe_failed' })
        evidence = @(
            ('discovery_json_present=' + $(if ($discoveryJsonPresent) { 'true' } else { 'false' })),
            'code_fact=spec §11.1/T6 未来能力(%LOCALAPPDATA%\MonkeyFence\discovery.json 当前不存在)'
        )
    }
    user_data_migration = [pscustomobject]@{
        status = 'absent'
        evidence = @(
            ('service_v1_db_present=' + $(if ($serviceV1DbPresent) { 'true' } else { 'false' })),
            'code_fact=spec §3.4/T1 未来能力(session.json→project_registry 幂等迁移未实现)'
        )
    }
}

# ---------------------------------------------------------------------------
# backup_capabilities:现有备份能力
# ---------------------------------------------------------------------------

$backupCapabilities = [pscustomobject]@{
    online_sqlite_backup = [pscustomobject]@{
        status = 'absent'
        evidence = @(
            'code_fact=根 Cargo.toml 中 rusqlite 仅启用 bundled feature(未启用 backup,SQLite Backup API 未编译)',
            'code_fact=crates/mf-agent/src/schema.rs 链式迁移(user_version 逐版升级)前无一致备份步骤',
            'spec_target=§3.1/T1 计划:schema 升级前用 SQLite Backup API 生成一致备份+manifest'
        )
    }
    user_db_backup_routine = [pscustomobject]@{
        status = 'absent'
        evidence = @(
            'code_fact=无任何定时/在线的项目库与目录库备份例程',
            'current_practice=手工离线拷贝(关闭应用后复制 .mf-agent / ~/.monkeyfence)'
        )
    }
    agent_cli_config_backup = [pscustomobject]@{
        status = 'present'
        evidence = @(
            'code_fact=crates/mf-plugins/src/hooks.rs 写入 Agent CLI 配置前生成 <file>.monkeyfence-backup-<ts> 整文件备份',
            'scope=仅覆盖 CLI 配置文件;不覆盖项目库 workflow-v1.db、目录库 catalog-v1.db 或 Secret'
        )
    }
}

# ---------------------------------------------------------------------------
# 输出(schema 字段固定;schema id 变更必须升版本)
# ---------------------------------------------------------------------------

$doc = [pscustomobject]@{
    schema = 'monkeyfence.windows-baseline.v1'
    capture = [pscustomobject]@{
        inputs = [pscustomobject]@{
            user_home = $UserHome
            project_root = $ProjectRoot
            repo_root = $RepoRoot
            local_app_data = $LocalAppData
            app_data_roaming = $AppDataRoaming
        }
        read_only = $true
    }
    runtime_facts = $runtimeFacts
    data_locations = $dataLocations
    residue = $residue
    delivery_capabilities = $deliveryCapabilities
    backup_capabilities = $backupCapabilities
}

$json = ConvertTo-Json -InputObject $doc -Depth 64
if (-not $json.EndsWith("`n")) { $json = $json + "`n" }

if ($OutputPath -ne '') {
    if (-not (Test-OutputPathSyntaxSupported -Path $OutputPath)) {
        throw "OutputPath 仅支持普通本地路径,拒绝 UNC/device/extended/ADS 别名: $OutputPath"
    }
    $fullOut = Convert-ToFullPath -Path $OutputPath
    if ($fullOut.StartsWith('\\')) {
        throw "OutputPath 规范化后成为 UNC/device/extended 路径,拒绝写入: $fullOut"
    }
    $parent = [System.IO.Path]::GetDirectoryName($fullOut)
    if (-not (Test-DirExists -Path $parent)) {
        throw "输出目录不存在,本脚本不创建目录: $parent"
    }
    if (Test-Path -LiteralPath $fullOut -ErrorAction SilentlyContinue) {
        throw "输出文件已存在,为避免覆盖用户数据本脚本拒绝写入: $fullOut"
    }
    $protectedRoots = @(
        @{ name = '用户数据目录'; path = $mfUserDir },
        @{ name = '项目运行数据目录'; path = $mfAgentDir },
        @{ name = '项目 MonkeyFence 目录'; path = $projectMfDir },
        @{ name = '源码/应用目录'; path = $RepoRoot },
        @{ name = '未来 per-user bundle 目录'; path = $bundleRoot },
        @{ name = '未来 Core discovery/data 目录'; path = $discoveryDir }
    )
    foreach ($protected in $protectedRoots) {
        if (Test-PathWithinOrEqual -Candidate $fullOut -Root $protected['path']) {
            throw ('输出路径位于受保护的' + $protected['name'] + ',拒绝写入: ' + $fullOut)
        }
    }
    if (Test-ParentChainHasReparsePoint -ParentPath $parent) {
        throw "输出目录链包含 symlink/junction/reparse-point,拒绝写入: $parent"
    }
    # CreateNew 把“不覆盖现有文件”落实为原子文件系统约束,消除
    # Test-Path 与实际写入之间的 TOCTOU 覆盖窗口。
    $encoding = New-Object System.Text.UTF8Encoding($false)
    $bytes = $encoding.GetBytes($json)
    $stream = [System.IO.File]::Open(
        $fullOut,
        [System.IO.FileMode]::CreateNew,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )
    try {
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
} else {
    Write-Output $json
}
