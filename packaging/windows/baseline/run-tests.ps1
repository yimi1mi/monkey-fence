# MonkeyFence Windows 交付基线契约测试(GitHub Issue #15 / T0d)
#
# 验证 capture.ps1 满足 Issue #15 的契约:
#   1. Windows PowerShell 5.1 与 PowerShell 7 均可执行(自动探测可用 shell,子进程调用)。
#   2. clean / existing 两种 fixture 输入;同一 fixture 在同一 shell 连跑两次输出字节一致。
#   3. 默认输出 stdout 的内容与 -OutputPath 文件内容一致,且均为合法 JSON。
#      OutputPath 拒绝覆盖现有文件/用户数据库,也拒绝写入源码应用目录。
#   4. 输出与 expected.json 的 golden 严格结构等价(注入路径以占位符替换后比较;
#      跨机器不稳定字段在 strict_compare_ignore_paths 中忽略)。
#   5. delivery/backup 全部能力字段齐全,status 与 capability_expectations 一致;
#      缺失能力显式 absent,不省略。
#   6. 输出不含时间戳;fixture 输入树(含文件哈希)采集前后不变;被读取的注册表键
#     (reg export 哈希)、服务与计划任务清单采集前后不变 —— 只读性验证。
#   7. expected.json 不绑定开发机(无绝对路径/用户名)。
#
# 用法:
#   pwsh -NoProfile -ExecutionPolicy Bypass -File run-tests.ps1              # 运行测试
#   pwsh -NoProfile -File run-tests.ps1 -UpdateGolden                        # 重新生成 golden
#
# 测试产物全部位于 %TEMP%\mf-baseline-tests-<guid>\,结束时自动清理。

[CmdletBinding()]
param([switch]$UpdateGolden)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0

$baselineDir = $PSScriptRoot
$captureScript = Join-Path $baselineDir 'capture.ps1'
$expectedPath = Join-Path $baselineDir 'expected.json'

$script:PassCount = 0
$script:FailCount = 0

function Write-Pass { param([string]$Name)
    $script:PassCount += 1
    Write-Output ('PASS  ' + $Name)
}
function Write-Fail { param([string]$Name)
    $script:FailCount += 1
    Write-Output ('FAIL  ' + $Name)
}
function Assert-True { param([bool]$Condition, [string]$Name)
    if ($Condition) { Write-Pass -Name $Name } else { Write-Fail -Name $Name }
}

# ---------------------------------------------------------------------------
# 通用工具
# ---------------------------------------------------------------------------

function Get-Sha256Text {
    param([string]$Text)
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($Text)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash($bytes)) -replace '-', '') }
    finally { $sha.Dispose() }
}

function Get-Sha256File {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
}

function Write-Utf8NoBom {
    param([string]$Path, [string]$Text)
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding($false)))
}

# `Get-Command` can return more than one executable when PATH contains both a
# system PowerShell and a bundled runtime.  Always select one concrete command
# instead of letting PowerShell stringify the array into an invalid path.
function Get-FirstApplicationCommand {
    param([string]$Name)
    $candidates = @(Get-Command -Name $Name -CommandType Application -All -ErrorAction SilentlyContinue)
    if ($candidates.Count -eq 0) { return $null }
    return $candidates[0]
}

# 递归指纹:目录/文件相对路径 + 文件 SHA-256,整体再哈希(输入树只读性验证)。
function Get-TreeFingerprint {
    param([string]$Root)
    $lines = @()
    foreach ($item in @(Get-ChildItem -LiteralPath $Root -Recurse -Force)) {
        $rel = $item.FullName.Substring($Root.Length).TrimStart('\')
        if ($item.PSIsContainer) {
            $lines += ('D ' + $rel)
        } else {
            $lines += ('F ' + $rel + ' ' + (Get-Sha256File -Path $item.FullName))
        }
    }
    [Array]::Sort($lines, [System.StringComparer]::Ordinal)
    return Get-Sha256Text -Text ($lines -join "`n")
}

# 注册表/服务/计划任务指纹(capture.ps1 读取范围内的只读性验证)。
function Get-RegistryFingerprint {
    param([string]$SnapDir, [string]$Tag)
    $regKeys = @(
        'HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKCU\Software\Microsoft\Windows\CurrentVersion\Run',
        'HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce',
        'HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall',
        'HKLM\Software\Microsoft\Windows\CurrentVersion\Run',
        'HKLM\Software\Microsoft\Windows\CurrentVersion\RunOnce',
        'HKLM\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
    )
    $parts = @()
    $i = 0
    foreach ($key in $regKeys) {
        $i += 1
        $file = Join-Path $SnapDir ($Tag + '-' + $i + '.reg')
        # EAP 暂时降为 Continue:PS5.1 下 native stderr 重定向在 EAP=Stop 时会抛
        # NativeCommandError;reg export 失败(键缺失)按退出码判断。
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $null = & reg.exe export $key $file /y 2>&1
        } finally {
            $ErrorActionPreference = $prevEap
        }
        if ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $file)) {
            $parts += ($key + '=' + (Get-Sha256File -Path $file))
        } else {
            $parts += ($key + '=ABSENT_OR_DENIED')
        }
    }
    try {
        $svcNames = @(Get-Service -ErrorAction Stop | ForEach-Object { $_.Name })
        [Array]::Sort($svcNames, [System.StringComparer]::OrdinalIgnoreCase)
        $parts += ('SERVICES=' + (Get-Sha256Text -Text ($svcNames -join '|')))
    } catch {
        $parts += 'SERVICES=UNAVAILABLE'
    }
    try {
        $taskNames = @(Get-ScheduledTask -ErrorAction Stop | ForEach-Object { ($_.TaskPath + $_.TaskName) })
        [Array]::Sort($taskNames, [System.StringComparer]::OrdinalIgnoreCase)
        $parts += ('TASKS=' + (Get-Sha256Text -Text ($taskNames -join '|')))
    } catch {
        $parts += 'TASKS=UNAVAILABLE'
    }
    return ($parts -join "`n")
}

# JSON 图工具:克隆 / 占位符替换 / 深比较
function Convert-JsonClone {
    param($Node)
    return (ConvertFrom-Json (ConvertTo-Json -InputObject $Node -Depth 64))
}

function Convert-ReplaceStrings {
    param($Node, [hashtable]$Map)
    if ($Node -is [string]) {
        $s = $Node
        foreach ($k in @($Map.Keys)) { $s = $s.Replace($k, $Map[$k]) }
        return $s
    }
    if ($Node -is [System.Array]) {
        for ($i = 0; $i -lt $Node.Count; $i++) { $Node[$i] = Convert-ReplaceStrings -Node $Node[$i] -Map $Map }
        # 逗号包裹防止空数组退化为空输出(→$null)、单元素数组被展开成标量
        return ,$Node
    }
    if ($Node -is [System.Management.Automation.PSCustomObject]) {
        foreach ($p in $Node.PSObject.Properties) { $p.Value = Convert-ReplaceStrings -Node $p.Value -Map $Map }
        return ,$Node
    }
    return $Node
}

function Get-JsonPrimitiveKind {
    param($Value)
    if ($Value -is [bool]) { return 'boolean' }
    if ($Value -is [string]) { return 'string' }
    if ($null -eq $Value) { return 'null' }
    $typeCode = [System.Type]::GetTypeCode($Value.GetType())
    if ($typeCode -in @(
        [System.TypeCode]::Byte, [System.TypeCode]::SByte,
        [System.TypeCode]::Int16, [System.TypeCode]::UInt16,
        [System.TypeCode]::Int32, [System.TypeCode]::UInt32,
        [System.TypeCode]::Int64, [System.TypeCode]::UInt64,
        [System.TypeCode]::Single, [System.TypeCode]::Double,
        [System.TypeCode]::Decimal
    )) { return 'number' }
    return $Value.GetType().FullName
}

function Compare-Graph {
    param($Expected, $Actual, [string]$Path, [string[]]$IgnorePaths, [System.Collections.ArrayList]$Diffs)
    foreach ($ig in $IgnorePaths) {
        if ($Path -eq $ig -or $Path.StartsWith($ig + '.') -or $Path.StartsWith($ig + '[')) { return }
    }
    if ($null -eq $Expected -and $null -eq $Actual) { return }
    if ($Expected -is [System.Array] -and $Actual -is [System.Array]) {
        if ($Expected.Count -ne $Actual.Count) {
            $null = $Diffs.Add(($Path + ' 数组长度 expected=' + $Expected.Count + ' actual=' + $Actual.Count))
            return
        }
        for ($i = 0; $i -lt $Expected.Count; $i++) {
            Compare-Graph -Expected $Expected[$i] -Actual $Actual[$i] -Path ($Path + '[' + $i + ']') -IgnorePaths $IgnorePaths -Diffs $Diffs
        }
        return
    }
    if ($Expected -is [System.Management.Automation.PSCustomObject] -and $Actual -is [System.Management.Automation.PSCustomObject]) {
        $ep = @($Expected.PSObject.Properties.Name)
        $ap = @($Actual.PSObject.Properties.Name)
        foreach ($name in $ep) { if ($ap -notcontains $name) { $null = $Diffs.Add(($Path + ' 缺少字段 ' + $name)) } }
        foreach ($name in $ap) { if ($ep -notcontains $name) { $null = $Diffs.Add(($Path + ' 多出字段 ' + $name)) } }
        foreach ($name in $ep) {
            if ($ap -contains $name) {
                # 根路径为空串,子路径不带前导点(与 strict_compare_ignore_paths 点分格式一致)
                $childPath = $name
                if ($Path -ne '') { $childPath = $Path + '.' + $name }
                Compare-Graph -Expected $Expected.$name -Actual $Actual.$name -Path $childPath -IgnorePaths $IgnorePaths -Diffs $Diffs
            }
        }
        return
    }
    # null 与数组/标量显式区分(避免 $null 与 @() 字符串化后都为空而漏检)
    if ($null -eq $Expected -or $null -eq $Actual) {
        if (-not ($null -eq $Expected -and $null -eq $Actual)) {
            $null = $Diffs.Add(($Path + ': null 不匹配(expectedIsNull=' + ($null -eq $Expected) + ', actualIsNull=' + ($null -eq $Actual) + ')'))
        }
        return
    }
    if ($Expected -is [System.Array] -or $Actual -is [System.Array]) {
        $null = $Diffs.Add(($Path + ' 类型不匹配(expected/actual 数组 vs 标量)'))
        return
    }
    $expectedKind = Get-JsonPrimitiveKind -Value $Expected
    $actualKind = Get-JsonPrimitiveKind -Value $Actual
    if ($expectedKind -ne $actualKind) {
        $null = $Diffs.Add(($Path + ' JSON 类型不匹配 expected=' + $expectedKind + ' actual=' + $actualKind))
        return
    }
    if ($expectedKind -eq 'string') {
        $different = ($Expected -cne $Actual)
    } else {
        $different = ($Expected -ne $Actual)
    }
    if ($different) {
        $null = $Diffs.Add(($Path + ': expected=<' + "$Expected" + '> actual=<' + "$Actual" + '>'))
    }
}

# 断言 JSON 图中指定点路径存在(schema 完整性)。
function Test-GraphPathExists {
    param($Node, [string]$DottedPath)
    $current = $Node
    foreach ($segment in $DottedPath.Split('.')) {
        if ($null -eq $current) { return $false }
        if ($current -is [System.Management.Automation.PSCustomObject]) {
            $names = @($current.PSObject.Properties.Name)
            if ($names -notcontains $segment) { return $false }
            $current = $current.$segment
        } else {
            return $false
        }
    }
    return $true
}

# 收集 JSON 图内所有字符串(机器独立性扫描用)。
function Get-GraphStrings {
    param($Node, [System.Collections.ArrayList]$Acc)
    if ($Node -is [string]) { $null = $Acc.Add($Node); return }
    if ($Node -is [System.Array]) {
        foreach ($item in $Node) { Get-GraphStrings -Node $item -Acc $Acc }
        return
    }
    if ($Node -is [System.Management.Automation.PSCustomObject]) {
        foreach ($p in $Node.PSObject.Properties) { Get-GraphStrings -Node $p.Value -Acc $Acc }
    }
}

# ---------------------------------------------------------------------------
# fixture 构造
# ---------------------------------------------------------------------------

$fixtureCargoToml = @'
[workspace]
resolver = "2"
members = [
    "crates/mf",
    "crates/mf-core",
    "crates/mf-agent",
    "crates/mf-vcs",
    "crates/mf-skills",
    "crates/mf-plugins",
    "crates/mfctl",
]
default-members = [
    "crates/mf",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "Apache-2.0"
'@

function New-Fixture {
    param([string]$Name, [bool]$Existing)
    $fxRoot = Join-Path $script:TestRoot $Name
    foreach ($d in @('home', 'project', 'local', 'roaming', 'repo')) {
        $null = New-Item -ItemType Directory -Path (Join-Path $fxRoot $d) -Force
    }
    $repo = Join-Path $fxRoot 'repo'
    Write-Utf8NoBom -Path (Join-Path $repo 'Cargo.toml') -Text $fixtureCargoToml
    $null = New-Item -ItemType Directory -Path (Join-Path $repo 'vendor\gpui_platform') -Force
    $null = New-Item -ItemType Directory -Path (Join-Path $fxRoot 'short-path-probe\HONGJI~1') -Force
    $null = New-Item -ItemType Directory -Path (Join-Path $fxRoot 'short-path-probe\PROJECT~1.DIR') -Force
    if ($Existing) {
        $mfUser = Join-Path $fxRoot 'home\.monkeyfence'
        $null = New-Item -ItemType Directory -Path $mfUser -Force
        Write-Utf8NoBom -Path (Join-Path $mfUser 'config.toml') -Text '# fixture config'
        Write-Utf8NoBom -Path (Join-Path $mfUser 'catalog-v1.db') -Text 'fixture-not-a-real-db'
        Write-Utf8NoBom -Path (Join-Path $mfUser 'catalog-v1.db-wal') -Text 'fixture-wal'
        Write-Utf8NoBom -Path (Join-Path $mfUser 'session.json') -Text '{}'
        Write-Utf8NoBom -Path (Join-Path $mfUser 'ui-prefs.json') -Text '{}'
        $null = New-Item -ItemType Directory -Path (Join-Path $mfUser 'skills') -Force
        $codex = Join-Path $fxRoot 'home\.codex'
        $null = New-Item -ItemType Directory -Path $codex -Force
        Write-Utf8NoBom -Path (Join-Path $codex 'config.toml') -Text '# fixture codex config'
        Write-Utf8NoBom -Path (Join-Path $codex 'config.toml.monkeyfence-backup-FIXTURE') -Text 'fixture backup payload'
        $mfAgent = Join-Path $fxRoot 'project\.mf-agent'
        $null = New-Item -ItemType Directory -Path $mfAgent -Force
        Write-Utf8NoBom -Path (Join-Path $mfAgent 'workflow-v1.db') -Text 'fixture-not-a-real-db'
        Write-Utf8NoBom -Path (Join-Path $mfAgent 'workflow-v1.db-wal') -Text 'fixture-wal'
        Write-Utf8NoBom -Path (Join-Path $mfAgent 'work-items.json') -Text '{}'
        Write-Utf8NoBom -Path (Join-Path $mfAgent 'orchestration.db') -Text 'legacy fixture db'
        Write-Utf8NoBom -Path (Join-Path $mfAgent 'workspaces.json') -Text '{}'
        Write-Utf8NoBom -Path (Join-Path $mfAgent 'step-run-1.md') -Text '# fixture handoff'
        $null = New-Item -ItemType Directory -Path (Join-Path $fxRoot 'project\.monkeyfence\skills') -Force
        $null = New-Item -ItemType Directory -Path (Join-Path $repo 'target\debug') -Force
        Write-Utf8NoBom -Path (Join-Path $repo 'target\debug\monkeyfence.exe') -Text 'MZ-fixture'
        Write-Utf8NoBom -Path (Join-Path $repo 'target\debug\mfctl.exe') -Text 'MZ-fixture'
    }
    return @{
        root    = $fxRoot
        home    = (Join-Path $fxRoot 'home')
        project = (Join-Path $fxRoot 'project')
        repo    = (Join-Path $fxRoot 'repo')
        local   = (Join-Path $fxRoot 'local')
        roaming = (Join-Path $fxRoot 'roaming')
    }
}

function Get-Placeholders {
    param([hashtable]$Fx)
    # 以编程方式构造(哈希字面量中以索引表达式作键不可靠)。
    $map = @{}
    $map[$Fx['home']] = '<USER_HOME>'
    $map[$Fx['project']] = '<PROJECT_ROOT>'
    $map[$Fx['repo']] = '<REPO_ROOT>'
    $map[$Fx['local']] = '<LOCAL_APP_DATA>'
    $map[$Fx['roaming']] = '<APP_DATA_ROAMING>'
    return $map
}

function Invoke-CaptureProcess {
    param([string]$ShellExe, [hashtable]$Fx, [string]$OutFile)
    $childArgs = @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $captureScript,
        '-UserHome', $Fx['home'], '-ProjectRoot', $Fx['project'], '-RepoRoot', $Fx['repo'],
        '-LocalAppData', $Fx['local'], '-AppDataRoaming', $Fx['roaming'],
        '-OutputPath', $OutFile
    )
    # EAP 暂时降为 Continue,避免 PS5.1 把子进程 stderr 当 NativeCommandError 抛出;
    # 失败统一按退出码处理并带回输出内容。
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $output = $null
    try {
        $output = & $ShellExe @childArgs 2>&1
    } finally {
        $ErrorActionPreference = $prevEap
    }
    return @{ exit_code = $LASTEXITCODE; output = ($output -join ' ') }
}

function Invoke-CaptureChild {
    param([string]$ShellExe, [hashtable]$Fx, [string]$OutFile)
    $result = Invoke-CaptureProcess -ShellExe $ShellExe -Fx $Fx -OutFile $OutFile
    if ($result['exit_code'] -ne 0) {
        throw ('capture 子进程失败(' + $ShellExe + '): ' + $result['output'])
    }
}

function Invoke-CaptureExpectFailure {
    param([string]$ShellExe, [hashtable]$Fx, [string]$OutFile)
    return Invoke-CaptureProcess -ShellExe $ShellExe -Fx $Fx -OutFile $OutFile
}

function Invoke-CaptureStdout {
    param([hashtable]$Fx)
    $lines = & $captureScript `
        -UserHome $Fx['home'] -ProjectRoot $Fx['project'] -RepoRoot $Fx['repo'] `
        -LocalAppData $Fx['local'] -AppDataRoaming $Fx['roaming']
    return ($lines -join "`n")
}

# ---------------------------------------------------------------------------
# expected.json 骨架(-UpdateGolden 重建)
# ---------------------------------------------------------------------------

function New-ExpectedSkeleton {
    $capabilities = [ordered]@{
        msi_installer            = 'absent'
        bootstrapper_exe         = 'absent'
        uninstaller              = 'absent'
        updater                  = 'absent'
        side_by_side_versions    = 'absent'
        current_json_pointer     = 'absent'
        windows_service          = 'absent'
        autostart                = 'absent'
        launcher                 = 'absent'
        tray                     = 'absent'
        picker                   = 'absent'
        core_service_bin         = 'absent'
        elevated_broker          = 'absent'
        discovery_file           = 'absent'
        user_data_migration      = 'absent'
        online_sqlite_backup     = 'absent'
        user_db_backup_routine   = 'absent'
        agent_cli_config_backup  = 'present'
    }
    return [pscustomobject]@{
        schema = 'monkeyfence.windows-baseline.v1'
        description = 'capture.ps1 输出的机器可读契约期望。fixture 注入路径以占位符表示,不绑定开发机用户名或绝对路径;由 run-tests.ps1 -UpdateGolden 生成 fixtures.*.expected。'
        placeholders = [pscustomobject]@{
            '<USER_HOME>'        = 'capture.ps1 -UserHome 注入的 fixture 用户主目录'
            '<PROJECT_ROOT>'     = 'capture.ps1 -ProjectRoot 注入的 fixture 项目根'
            '<REPO_ROOT>'        = 'capture.ps1 -RepoRoot 注入的 fixture 仓库根'
            '<LOCAL_APP_DATA>'   = 'capture.ps1 -LocalAppData 注入的 fixture AppData\Local'
            '<APP_DATA_ROAMING>' = 'capture.ps1 -AppDataRoaming 注入的 fixture Roaming AppData'
        }
        capability_expectations = [pscustomobject]$capabilities
        strict_compare_ignore_paths = @(
            'runtime_facts.binaries.path_lookup',
            'residue.processes.running'
        )
        invariants = [pscustomobject]@{
            byte_stable_same_fixture_same_shell = '同一 fixture 同一 shell 连跑两次输出字节一致'
            sorted_ordinal_ignore_case          = '全部枚举按 OrdinalIgnoreCase 排序'
            no_timestamps_no_pids_no_sizes      = '不输出时间戳/PID/文件大小/随机值'
            strict_json_primitive_types         = 'golden 比较区分 boolean/number/string JSON 类型'
            output_path_non_overwrite           = 'OutputPath 原子拒绝现有文件、受保护目录及 extended/device/UNC/ADS/8.3 short-name 别名'
            fixture_tree_unchanged              = '采集前后 fixture 输入树(含哈希)不变'
            registry_unchanged                  = '采集前后被读取的注册表键/服务/计划任务状态不变'
        }
        fixtures = [pscustomobject]@{
            clean = [pscustomobject]@{
                scenario = '干净用户环境:home/project/local/roaming 无任何 MonkeyFence 数据;repo 仅最小 Cargo.toml 与 vendor/gpui_platform,无构建产物'
                expected = $null
            }
            existing = [pscustomobject]@{
                scenario = '已有数据环境:~/.monkeyfence(config/catalog+wal/session/ui-prefs/skills)、~/.codex 含 monkeyfence-backup 残留、Project/.mf-agent(workflow-v1.db+wal、work-items.json、旧库 orchestration.db/workspaces.json、step-run-*.md)、Project/.monkeyfence/skills、repo target/debug 含 monkeyfence.exe 与 mfctl.exe;仍无安装器/服务/自启动'
                expected = $null
            }
        }
    }
}

# ---------------------------------------------------------------------------
# 主流程
# ---------------------------------------------------------------------------

$script:TestRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('mf-baseline-tests-' + [System.Guid]::NewGuid().ToString('N'))
$outDir = Join-Path $script:TestRoot 'out'
$snapDir = Join-Path $script:TestRoot 'regsnap'
$null = New-Item -ItemType Directory -Path $outDir -Force
$null = New-Item -ItemType Directory -Path $snapDir -Force

$exitCode = 1
try {
    # shell 探测
    $shells = @()
    $ps51 = Get-FirstApplicationCommand -Name 'powershell.exe'
    if ($null -ne $ps51) { $shells += @{ name = 'powershell-5.1'; exe = $ps51.Source } }
    $ps7 = Get-FirstApplicationCommand -Name 'pwsh.exe'
    if ($null -ne $ps7) { $shells += @{ name = 'powershell-7'; exe = $ps7.Source } }
    Assert-True -Condition ($shells.Count -ge 1) -Name ('shell 探测: ' + (($shells | ForEach-Object { $_['name'] }) -join ', '))

    $primitiveTypeDiffs = New-Object System.Collections.ArrayList
    Compare-Graph -Expected $true -Actual 'True' -Path 'boolean_type_guard' -IgnorePaths @() -Diffs $primitiveTypeDiffs
    Compare-Graph -Expected 1 -Actual '1' -Path 'number_type_guard' -IgnorePaths @() -Diffs $primitiveTypeDiffs
    Assert-True -Condition ($primitiveTypeDiffs.Count -eq 2) -Name 'golden 比较严格区分 boolean/number 与 string'

    # fixture 构造 + 采集前指纹
    $fxClean = New-Fixture -Name 'clean' -Existing $false
    $fxExisting = New-Fixture -Name 'existing' -Existing $true
    $fixtures = @(
        @{ name = 'clean'; fx = $fxClean },
        @{ name = 'existing'; fx = $fxExisting }
    )
    $fingerprintsBefore = @{}
    foreach ($f in $fixtures) { $fingerprintsBefore[$f['name']] = Get-TreeFingerprint -Root $f['fx']['root'] }
    $regBefore = Get-RegistryFingerprint -SnapDir $snapDir -Tag 'before'

    # OutputPath 是 capture 唯一允许的写,但不能因此覆盖任意现有文件或写入
    # 用户数据库/源码应用目录。每个可用 shell 都冻结这条安全边界。
    foreach ($shell in $shells) {
        $existingOut = Join-Path $outDir ('existing-output-' + $shell['name'] + '.json')
        Write-Utf8NoBom -Path $existingOut -Text 'sentinel-output-must-survive'
        $existingHash = Get-Sha256File -Path $existingOut
        $existingResult = Invoke-CaptureExpectFailure -ShellExe $shell['exe'] -Fx $fxExisting -OutFile $existingOut
        Assert-True -Condition (
            $existingResult['exit_code'] -ne 0 -and
            (Get-Sha256File -Path $existingOut) -eq $existingHash
        ) -Name ('OutputPath 拒绝覆盖现有文件: shell=' + $shell['name'])

        $fixtureDb = Join-Path $fxExisting['home'] '.monkeyfence\catalog-v1.db'
        $fixtureDbHash = Get-Sha256File -Path $fixtureDb
        $dbResult = Invoke-CaptureExpectFailure -ShellExe $shell['exe'] -Fx $fxExisting -OutFile $fixtureDb
        Assert-True -Condition (
            $dbResult['exit_code'] -ne 0 -and
            (Get-Sha256File -Path $fixtureDb) -eq $fixtureDbHash
        ) -Name ('OutputPath 拒绝覆盖用户数据库: shell=' + $shell['name'])

        $extendedDb = '\\?\' + $fixtureDb
        $extendedResult = Invoke-CaptureExpectFailure -ShellExe $shell['exe'] -Fx $fxExisting -OutFile $extendedDb
        Assert-True -Condition (
            $extendedResult['exit_code'] -ne 0 -and
            (Get-Sha256File -Path $fixtureDb) -eq $fixtureDbHash
        ) -Name ('OutputPath 拒绝 extended-path 数据库别名: shell=' + $shell['name'])

        $forwardExtendedDb = '//?/' + ($fixtureDb -replace '\\', '/')
        $forwardExtendedResult = Invoke-CaptureExpectFailure -ShellExe $shell['exe'] -Fx $fxExisting -OutFile $forwardExtendedDb
        Assert-True -Condition (
            $forwardExtendedResult['exit_code'] -ne 0 -and
            (Get-Sha256File -Path $fixtureDb) -eq $fixtureDbHash
        ) -Name ('OutputPath 拒绝正斜杠 extended-path 别名: shell=' + $shell['name'])

        $shortAliasOutput = Join-Path $fxExisting['root'] 'short-path-probe\HONGJI~1\output.json'
        $shortAliasResult = Invoke-CaptureExpectFailure -ShellExe $shell['exe'] -Fx $fxExisting -OutFile $shortAliasOutput
        $shortExtensionOutput = Join-Path $fxExisting['root'] 'short-path-probe\PROJECT~1.DIR\output.json'
        $shortExtensionResult = Invoke-CaptureExpectFailure -ShellExe $shell['exe'] -Fx $fxExisting -OutFile $shortExtensionOutput
        Assert-True -Condition (
            $shortAliasResult['exit_code'] -ne 0 -and
            $shortExtensionResult['exit_code'] -ne 0 -and
            -not (Test-Path -LiteralPath $shortAliasOutput) -and
            -not (Test-Path -LiteralPath $shortExtensionOutput)
        ) -Name ('OutputPath fail-closed 拒绝带/不带扩展名的 8.3 short-name: shell=' + $shell['name'])

        $repoOutput = Join-Path $fxExisting['repo'] ('forbidden-output-' + $shell['name'] + '.json')
        $repoResult = Invoke-CaptureExpectFailure -ShellExe $shell['exe'] -Fx $fxExisting -OutFile $repoOutput
        Assert-True -Condition (
            $repoResult['exit_code'] -ne 0 -and
            -not (Test-Path -LiteralPath $repoOutput)
        ) -Name ('OutputPath 拒绝写入源码应用目录: shell=' + $shell['name'])
    }

    # 每个 fixture × 每个 shell 跑两次,断言字节稳定;记录首个 shell 的输出做 golden 比较
    $goldenActual = @{}
    foreach ($f in $fixtures) {
        foreach ($shell in $shells) {
            $tag = ($f['name'] + '-' + $shell['name'])
            $out1 = Join-Path $outDir ($tag + '-run1.json')
            $out2 = Join-Path $outDir ($tag + '-run2.json')
            Invoke-CaptureChild -ShellExe $shell['exe'] -Fx $f['fx'] -OutFile $out1
            Invoke-CaptureChild -ShellExe $shell['exe'] -Fx $f['fx'] -OutFile $out2
            Assert-True -Condition ((Get-Sha256File -Path $out1) -eq (Get-Sha256File -Path $out2)) `
                -Name ('字节稳定: fixture=' + $f['name'] + ' shell=' + $shell['name'])
            if (-not $goldenActual.ContainsKey($f['name'])) {
                $goldenActual[$f['name']] = @{ path = $out1; shell = $shell['name'] }
            }
        }
    }

    # stdout 默认输出:内容可解析且与文件输出一致(在当前 shell 内执行;
    # 与当前 shell 同版本的 -OutputPath 输出逐字比较,不同版本只做规范化比较)
    foreach ($f in $fixtures) {
        $stdoutText = Invoke-CaptureStdout -Fx $f['fx']
        $stdoutParses = $true
        try { $null = $stdoutText | ConvertFrom-Json } catch { $stdoutParses = $false }
        Assert-True -Condition $stdoutParses -Name ('默认 stdout 为合法 JSON: fixture=' + $f['name'])
        $sameShellPath = $null
        $currentEdition = "$($PSVersionTable.PSEdition)"
        foreach ($shell in $shells) {
            $isSame = ($currentEdition -eq 'Core' -and $shell['name'] -eq 'powershell-7') -or
                      ($currentEdition -eq 'Desktop' -and $shell['name'] -eq 'powershell-5.1')
            if ($isSame) { $sameShellPath = Join-Path $outDir ($f['name'] + '-' + $shell['name'] + '-run1.json') }
        }
        if ($null -ne $sameShellPath) {
            $fileText = [System.IO.File]::ReadAllText($sameShellPath)
            Assert-True -Condition ($stdoutText.TrimEnd("`r`n") -ceq $fileText.TrimEnd("`r`n")) `
                -Name ('stdout 与 -OutputPath 内容一致: fixture=' + $f['name'] + ' shell=' + $currentEdition)
        } else {
            $normStdout = ($stdoutText | ConvertFrom-Json) | ConvertTo-Json -Depth 64 -Compress
            $normFile = (([System.IO.File]::ReadAllText($goldenActual[$f['name']]['path'])) | ConvertFrom-Json) | ConvertTo-Json -Depth 64 -Compress
            Assert-True -Condition ($normStdout -ceq $normFile) `
                -Name ('stdout 与 -OutputPath 规范化一致: fixture=' + $f['name'])
        }
    }

    # 跨 shell 解析等价(两个 shell 都可用时)
    if ($shells.Count -ge 2) {
        foreach ($f in $fixtures) {
            $outA = Join-Path $outDir ($f['name'] + '-' + $shells[0]['name'] + '-run1.json')
            $outB = Join-Path $outDir ($f['name'] + '-' + $shells[1]['name'] + '-run1.json')
            $normA = ([System.IO.File]::ReadAllText($outA) | ConvertFrom-Json) | ConvertTo-Json -Depth 64 -Compress
            $normB = ([System.IO.File]::ReadAllText($outB) | ConvertFrom-Json) | ConvertTo-Json -Depth 64 -Compress
            Assert-True -Condition ($normA -ceq $normB) -Name ('跨 shell 解析等价: fixture=' + $f['name'])
        }
    }

    # 能力断言 + golden 严格比较 + 无时间戳
    $expectedDoc = $null
    try { $expectedDoc = [System.IO.File]::ReadAllText($expectedPath) | ConvertFrom-Json } catch { $expectedDoc = $null }
    if ($null -eq $expectedDoc -and $UpdateGolden) {
        Write-Output 'SKIP  expected.json 尚不存在,-UpdateGolden 将在本轮生成后重跑验证'
    } else {
        Assert-True -Condition ($null -ne $expectedDoc) -Name 'expected.json 存在且可解析'
    }
    if ($null -ne $expectedDoc) {
        $capExpect = @($expectedDoc.capability_expectations.PSObject.Properties)
        Assert-True -Condition ($capExpect.Count -ge 18) -Name ('capability_expectations 覆盖 ' + $capExpect.Count + ' 项能力')

        $ignorePaths = @()
        if ($null -ne $expectedDoc.strict_compare_ignore_paths) { $ignorePaths = @($expectedDoc.strict_compare_ignore_paths) }

        foreach ($f in $fixtures) {
            $actualRaw = [System.IO.File]::ReadAllText($goldenActual[$f['name']]['path'])
            $actualDoc = $actualRaw | ConvertFrom-Json

            # 1) 全部能力字段齐全且 status 符合期望(交付能力在 delivery_capabilities,
            #    备份能力在 backup_capabilities;capability_expectations 同时覆盖两段)
            $capIssues = @()
            $dcNames = @($actualDoc.delivery_capabilities.PSObject.Properties.Name)
            $bcNames = @($actualDoc.backup_capabilities.PSObject.Properties.Name)
            foreach ($prop in $capExpect) {
                # StrictMode 下先判定属性存在,再动态取值
                $capNode = $null
                if ($dcNames -contains $prop.Name) { $capNode = $actualDoc.delivery_capabilities.($prop.Name) }
                elseif ($bcNames -contains $prop.Name) { $capNode = $actualDoc.backup_capabilities.($prop.Name) }
                if ($null -eq $capNode) { $capIssues += ('缺少能力字段 ' + $prop.Name); continue }
                if ("$($capNode.status)" -ne "$($prop.Value)") {
                    $capIssues += ($prop.Name + ' status=' + "$($capNode.status)" + ' 期望 ' + "$($prop.Value)")
                }
            }
            Assert-True -Condition ($capIssues.Count -eq 0) `
                -Name ('能力 status 与期望一致: fixture=' + $f['name'] + $(if ($capIssues.Count -gt 0) { ' [' + ($capIssues -join '; ') + ']' } else { '' }))

            # 2) golden 严格结构比较(占位符替换 + 忽略跨机器字段)
            $goldenDoc = Convert-JsonClone -Node $expectedDoc.fixtures.($f['name']).expected
            $substituted = Convert-JsonClone -Node $actualDoc
            $null = Convert-ReplaceStrings -Node $substituted -Map (Get-Placeholders -Fx $f['fx'])
            $diffs = New-Object System.Collections.ArrayList
            Compare-Graph -Expected $goldenDoc -Actual $substituted -Path '' -IgnorePaths $ignorePaths -Diffs $diffs
            Assert-True -Condition ($diffs.Count -eq 0) `
                -Name ('golden 结构等价: fixture=' + $f['name'] + $(if ($diffs.Count -gt 0) { ' [' + (($diffs | Select-Object -First 5) -join '; ') + '...共' + $diffs.Count + '处]' } else { '' }))

            # 3) schema 关键字段齐全(absent 不得以省略方式出现)
            $requiredPaths = @(
                'schema', 'capture.inputs', 'capture.read_only',
                'runtime_facts.startup_form.main_bin', 'runtime_facts.startup_form.cli_bin',
                'runtime_facts.binaries.repo_target', 'runtime_facts.binaries.path_lookup',
                'data_locations.user.probe_ok', 'data_locations.project.probe_ok',
                'data_locations.project_monkeyfence.probe_ok',
                'residue.registry.uninstall_keys', 'residue.registry.run_keys',
                'residue.startup_folder', 'residue.services', 'residue.scheduled_tasks',
                'residue.processes.names_probed', 'residue.processes.running',
                'residue.agent_cli_config_backups.probe_ok', 'residue.install_locations',
                'delivery_capabilities', 'backup_capabilities.online_sqlite_backup',
                'backup_capabilities.user_db_backup_routine', 'backup_capabilities.agent_cli_config_backup'
            )
            $missing = @()
            foreach ($rp in $requiredPaths) {
                if (-not (Test-GraphPathExists -Node $actualDoc -DottedPath $rp)) { $missing += $rp }
            }
            Assert-True -Condition ($missing.Count -eq 0) -Name ('schema 关键字段齐全: fixture=' + $f['name'] + $(if ($missing.Count -gt 0) { ' 缺[' + ($missing -join ', ') + ']' } else { '' }))

            # 4) 无时间戳(ISO 日期 / YYYYMMDDThhmmss 备份命名均不得由脚本生成)
            $hasTs = $false
            if ($actualRaw -match '\d{4}-\d{2}-\d{2}T\d{2}:\d{2}') { $hasTs = $true }
            if ($actualRaw -match '(19|20)\d{6}T\d{6}') { $hasTs = $true }
            Assert-True -Condition (-not $hasTs) -Name ('输出不含时间戳: fixture=' + $f['name'])
        }

        # expected.json 机器独立性:无绝对路径、无当前用户名
        $strings = New-Object System.Collections.ArrayList
        Get-GraphStrings -Node $expectedDoc -Acc $strings
        $absPaths = @()
        foreach ($s in $strings) { if ($s -match '^[A-Za-z]:[\\/]') { $absPaths += $s } }
        Assert-True -Condition ($absPaths.Count -eq 0) -Name ('expected.json 无绝对路径' + $(if ($absPaths.Count -gt 0) { ': ' + ($absPaths -join '; ') } else { '' }))
        $username = [Environment]::UserName
        $userLeak = @()
        foreach ($s in $strings) { if ($username -ne '' -and $s -like ('*' + $username + '*')) { $userLeak += $s } }
        Assert-True -Condition ($userLeak.Count -eq 0) -Name 'expected.json 不含当前用户名'
    }

    # 只读性:fixture 输入树与注册表状态前后不变
    foreach ($f in $fixtures) {
        $after = Get-TreeFingerprint -Root $f['fx']['root']
        Assert-True -Condition ($after -eq $fingerprintsBefore[$f['name']]) `
            -Name ('fixture 输入树采集前后不变: ' + $f['name'])
    }
    $regAfter = Get-RegistryFingerprint -SnapDir $snapDir -Tag 'after'
    Assert-True -Condition ($regAfter -ceq $regBefore) -Name '注册表/服务/计划任务采集前后不变'

    # golden 重建
    if ($UpdateGolden) {
        $skeleton = New-ExpectedSkeleton
        foreach ($f in $fixtures) {
            $doc = ([System.IO.File]::ReadAllText($goldenActual[$f['name']]['path']) | ConvertFrom-Json)
            $null = Convert-ReplaceStrings -Node $doc -Map (Get-Placeholders -Fx $f['fx'])
            # 跨机器不稳定字段以规范化空值写入 golden
            $doc.runtime_facts.binaries.path_lookup = @()
            $doc.residue.processes.running = @()
            $skeleton.fixtures.($f['name']).expected = $doc
        }
        $json = ConvertTo-Json -InputObject $skeleton -Depth 64
        if (-not $json.EndsWith("`n")) { $json = $json + "`n" }
        Write-Utf8NoBom -Path $expectedPath -Text $json
        Write-Output ('GOLDEN 已更新: ' + $expectedPath)
    }

    $exitCode = 0
    if ($script:FailCount -gt 0) { $exitCode = 1 }
} catch {
    Write-Fail -Name ('异常: ' + $_.Exception.Message)
    $exitCode = 1
} finally {
    if (Test-Path -LiteralPath $script:TestRoot) {
        Remove-Item -LiteralPath $script:TestRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Output ('==== ' + $script:PassCount + ' passed, ' + $script:FailCount + ' failed ====')
exit $exitCode
