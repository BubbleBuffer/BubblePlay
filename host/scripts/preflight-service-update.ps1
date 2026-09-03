[CmdletBinding()]
param(
    [ValidateRange(5, 120)]
    [int]$ManifestLifetimeMinutes = 30,
    [ValidateRange(60, 3600)]
    [int]$RollbackTimeoutSeconds = 2400
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$hostDir = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$projectRoot = [IO.Path]::GetFullPath((Join-Path $hostDir '..'))
$workerDir = Join-Path $projectRoot 'worker'
$sourceDir = Join-Path $hostDir 'target-privileged\release'
$runId = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [Guid]::NewGuid().ToString('N')
$runDir = Join-Path $hostDir "target-privileged\update-preflight\$runId"
$manifestPath = Join-Path $runDir 'manifest.json'
$checks = [Collections.Generic.List[object]]::new()
$requiredBinaries = @('bubbleplay_broker.exe', 'bubbleplay_transport.exe', 'bubbleplay_agent.exe', 'bubbleplay_input_proxy.exe', 'bubbleplay_lab_helper.exe', 'bubbleplay-host.exe')

New-Item -ItemType Directory -Path $runDir -Force | Out-Null

function Add-Check([string]$Name, [string]$Evidence) {
    $checks.Add([ordered]@{ name = $Name; status = 'pass'; evidence = $Evidence })
}

function Invoke-Gate([string]$Name, [string]$WorkingDirectory, [string]$Executable, [string[]]$Arguments) {
    $log = Join-Path $runDir ($Name + '.log')
    Push-Location $WorkingDirectory
    try {
        $gateErrorActionPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $output = @(& $Executable @Arguments 2>&1)
            $exit = $LASTEXITCODE
        }
        finally {
            $ErrorActionPreference = $gateErrorActionPreference
        }
        [IO.File]::WriteAllLines($log, [string[]]$output, [Text.UTF8Encoding]::new($false))
        if ($null -eq $exit) { throw "$Name failed to start. See $log" }
        if ($exit -ne 0) { throw "$Name failed with exit code $exit. See $log" }
    }
    finally { Pop-Location }
    Add-Check $Name $log
}

function Test-ScriptSyntax([string]$Engine, [string]$Path) {
    $escapedPath = ([IO.Path]::GetFullPath($Path)).Replace("'", "''")
    $command = "`$tokens=`$null; `$errors=`$null; [Management.Automation.Language.Parser]::ParseFile('$escapedPath',[ref]`$tokens,[ref]`$errors)|Out-Null; if(`$errors.Count){`$errors|ForEach-Object{`$_.Message}|Write-Error; exit 1}"
    Invoke-Gate ("syntax-" + [IO.Path]::GetFileNameWithoutExtension($Path) + '-' + [IO.Path]::GetFileNameWithoutExtension($Engine)) $projectRoot $Engine @('-NoProfile', '-NonInteractive', '-Command', $command)
}

try {
    # Mandatory gates: cargo test --all-targets; npm test; npm run check:syntax;
    # cargo build --release --bins. Keep these literals visible for review tooling.
    Invoke-Gate 'rust-tests' $hostDir 'cargo.exe' @('test', '--all-targets')
    Invoke-Gate 'worker-tests' $workerDir 'npm.cmd' @('test')
    Invoke-Gate 'worker-syntax' $workerDir 'npm.cmd' @('run', 'check:syntax')
    Invoke-Gate 'release-build' $hostDir 'cargo.exe' @('build', '--release', '--bins', '--target-dir', 'target-privileged')

    $deploymentScripts = @(
        (Join-Path $PSScriptRoot 'install-service.ps1'),
        (Join-Path $PSScriptRoot 'update-service.ps1'),
        (Join-Path $PSScriptRoot 'collect-diagnostics.ps1'),
        (Join-Path $PSScriptRoot 'publish-update-validation.ps1'),
        (Join-Path $PSScriptRoot 'validate-service-update.ps1'),
        $PSCommandPath
    )
    foreach ($script in $deploymentScripts) {
        Test-ScriptSyntax 'powershell.exe' $script
        Test-ScriptSyntax 'pwsh.exe' $script
    }
    $validatorEnvironment = & (Join-Path $PSScriptRoot 'validate-service-update.ps1') -EnvironmentOnly
    if (-not $validatorEnvironment.ShapeSafe) { throw 'External validator environment-shape probe failed.' }
    Add-Check 'external-validator-environment' ($validatorEnvironment | ConvertTo-Json -Compress -Depth 6)
    $parserFixture = Join-Path $runDir 'mixed-cli-output.fixture'
    [IO.File]::WriteAllLines($parserFixture, [string[]]@(
        'Stream Lab: progress line.',
        '{',
        '  "exitCode": 2,',
        '  "result": "unavailable",',
        '  "error": { "code": "fixture-transient" }',
        '}'
    ), [Text.UTF8Encoding]::new($false))
    $parsedFixture = & (Join-Path $PSScriptRoot 'validate-service-update.ps1') -ParseSmokeLog $parserFixture
    if ($parsedFixture.exitCode -ne 2 -or $parsedFixture.error.code -ne 'fixture-transient') { throw 'Mixed CLI output parser fixture failed.' }
    Remove-Item -Force -LiteralPath $parserFixture
    Add-Check 'external-validator-cli-parser' 'progress-prefixed JSON parsed'

    $services = @(Get-CimInstance Win32_Service -Filter "Name LIKE 'BubblePlay%'")
    foreach ($name in @('BubblePlayBroker', 'BubblePlayTransport')) {
        $service = @($services | Where-Object Name -eq $name)
        if ($service.Count -ne 1 -or $service[0].State -ne 'Running') { throw "$name is not running." }
    }
    if (@($services | Where-Object Name -eq 'BubblePlayLabHelper').Count -ne 1) { throw 'BubblePlayLabHelper is not installed.' }
    Add-Check 'service-baseline' (($services | Select-Object Name, State, StartMode | ConvertTo-Json -Compress))

    $dataDir = Join-Path $env:ProgramData 'BubblePlay'
    $dataItem = Get-Item -Force -LiteralPath $dataDir
    if (($dataItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'BubblePlay data root is a reparse point.' }
    $acl = Get-Acl -LiteralPath $dataDir
    if (-not $acl.AreAccessRulesProtected) { throw 'BubblePlay data ACL inheritance is not protected.' }
    $ownerSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $expectedSids = @(
        'S-1-5-18',
        'S-1-5-32-544',
        'S-1-5-80-36460983-23004156-1113575813-106319344-2112134846',
        $ownerSid
    )
    $rules = @($acl.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier]))
    $actualSids = @($rules | ForEach-Object { $_.IdentityReference.Value } | Sort-Object -Unique)
    if ($rules.Count -ne 4 -or (Compare-Object ($expectedSids | Sort-Object -Unique) $actualSids)) {
        throw "Unexpected BubblePlay data ACL: $($actualSids -join ', ')"
    }
    Add-Check 'data-acl' ($actualSids -join ',')

    $collector = & (Join-Path $PSScriptRoot 'collect-diagnostics.ps1') -TailLines 300
    if ($collector.AccessDenied -ne 0) { throw "Diagnostics collector encountered $($collector.AccessDenied) access-denied logs." }
    Add-Check 'non-admin-diagnostics' ([string]$collector.Summary)

    $hostLog = Join-Path $env:ProgramData 'BubblePlay\logs\transport\transport-host.stderr.log'
    if (-not (Test-Path -LiteralPath $hostLog -PathType Leaf)) { throw 'Transport host log is missing.' }
    $tail = (Get-Content -LiteralPath $hostLog -Tail 3000) -join "`n"
    $registrationEvidence = 'bounded-tail'
    if ($tail -notmatch 'signaling registered as host') {
        $findstr = Join-Path $env:SystemRoot 'System32\findstr.exe'
        & $findstr '/M' '/L' '/C:signaling registered as host' $hostLog | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Transport host log contains no successful signaling registration.' }
        $registrationEvidence = 'constant-memory-first-match'
    }
    Add-Check 'host-signaling-baseline' "$registrationEvidence $hostLog"

    $files = [Collections.Generic.List[object]]::new()
    foreach ($name in $requiredBinaries) {
        $path = Join-Path $sourceDir $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Required binary is missing: $path" }
        $item = Get-Item -LiteralPath $path
        $files.Add([ordered]@{ name = $name; path = $item.FullName; size = [int64]$item.Length; sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToUpperInvariant() })
    }
    Add-Check 'release-hashes' "$($files.Count) binaries"

    $receiptId = [Guid]::NewGuid().ToString('N')
    $nonceBytes = [byte[]]::new(32)
    $nonceGenerator = [Security.Cryptography.RandomNumberGenerator]::Create()
    try { $nonceGenerator.GetBytes($nonceBytes) } finally { $nonceGenerator.Dispose() }
    $validationNonce = -join ($nonceBytes | ForEach-Object { '{0:x2}' -f $_ })
    $createdAt = [DateTimeOffset]::UtcNow
    $manifest = [ordered]@{
        schemaVersion = 1
        runId = $runId
        receiptId = $receiptId
        createdAt = $createdAt.ToString('O')
        expiresAt = $createdAt.AddMinutes($ManifestLifetimeMinutes).ToString('O')
        ready = $true
        sourceDir = $sourceDir
        files = @($files)
        checks = @($checks)
        validationNonce = $validationNonce
        validationRequestPath = (Join-Path $runDir 'validation-request.json')
        validationResultPath = (Join-Path $runDir 'validation-result.json')
        receiptPath = (Join-Path $hostDir "target-privileged\update-receipts\$receiptId.json")
        rollbackTimeoutSeconds = $RollbackTimeoutSeconds
    }
    [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
    $updater = Join-Path $PSScriptRoot 'update-service.ps1'
    $validated = & $updater -ApplyManifest $manifestPath -ValidateManifestOnly
    if (-not $validated.Valid -or $validated.Files -ne $requiredBinaries.Count) { throw 'The updater rejected the generated ready manifest.' }
    Add-Check 'updater-valid-manifest' $manifestPath

    $tamperedPath = Join-Path $runDir 'tampered-manifest.json'
    $tampered = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
    $tampered.files[0].sha256 = ('0' * 64)
    [IO.File]::WriteAllText($tamperedPath, ($tampered | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
    $tamperRejected = $false
    try { $null = & $updater -ApplyManifest $tamperedPath -ValidateManifestOnly }
    catch { $tamperRejected = $_.Exception.Message -match 'SHA256 mismatch' }
    if (-not $tamperRejected) { throw 'The updater did not reject a hash-tampered manifest.' }
    Remove-Item -Force -LiteralPath $tamperedPath
    Add-Check 'updater-tampered-manifest' 'rejected'

    $manifest.checks = @($checks)
    [IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
    $null = & $updater -ApplyManifest $manifestPath -ValidateManifestOnly
    $adminCommand = "& '$((Join-Path $PSScriptRoot 'update-service.ps1').Replace("'", "''"))' -ApplyManifest '$($manifestPath.Replace("'", "''"))'"
    [pscustomobject]@{ Ready = $true; Manifest = $manifestPath; AdminCommand = $adminCommand; CheckCount = $checks.Count }
}
catch {
    $failurePath = Join-Path $runDir 'preflight-failure.json'
    $failure = [ordered]@{ ready = $false; runId = $runId; failedAt = [DateTimeOffset]::UtcNow.ToString('O'); error = $_.Exception.Message; checks = @($checks) }
    [IO.File]::WriteAllText($failurePath, ($failure | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
    throw "Update preflight failed; no ready manifest was issued. $($_.Exception.Message) Evidence: $failurePath"
}
