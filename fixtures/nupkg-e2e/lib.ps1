# Helpers shared by the nupkg-e2e guest scripts. Dot-sourced from C:\e2e.
# Windows PowerShell 5.1. Filesystem and registry work goes through .NET so
# display names containing [ ] or ' are never read as wildcards.
$ErrorActionPreference = 'Stop'

$e2e = 'C:\e2e'
$pkgs = Join-Path $e2e 'pkgs'
$programFiles = $env:ProgramW6432
$chocoRoot = Join-Path $env:ProgramData 'chocolatey'
$choco = Join-Path $chocoRoot 'bin\choco.exe'
$hklm = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
$environmentKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
# An unexpanded entry the generated scripts must keep as text. The clean VM's
# PATH is REG_SZ with no %...% entry, so Set-PathProbe seeds one.
$probe = '%SystemRoot%\embala-e2e-probe'

function Get-RawPath {
    $key = $hklm.OpenSubKey($environmentKey)
    try {
        [pscustomobject]@{
            Kind  = $key.GetValueKind('Path')
            Value = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        }
    } finally {
        $key.Close()
    }
}

function Set-PathProbe {
    $raw = Get-RawPath
    $value = $raw.Value
    if (@($value -split ';') -cnotcontains $probe) { $value = $value.TrimEnd(';') + ';' + $probe }
    $key = $hklm.OpenSubKey($environmentKey, $true)
    try {
        $key.SetValue('Path', $value, [Microsoft.Win32.RegistryValueKind]::ExpandString)
    } finally {
        $key.Close()
    }
}

function Test-SamePath([string]$a, [string]$b) {
    [string]::Equals($a.TrimEnd('\'), $b.TrimEnd('\'), [System.StringComparison]::OrdinalIgnoreCase)
}

function Get-PathCount([string]$dir) {
    $n = 0
    foreach ($entry in ((Get-RawPath).Value -split ';')) {
        if ($entry -and (Test-SamePath $entry $dir)) { $n++ }
    }
    $n
}

# Adds a problem unless PATH is still REG_EXPAND_SZ with the probe unexpanded.
function Assert-PathIntact($problems, [string]$when) {
    $raw = Get-RawPath
    if ($raw.Kind -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) {
        $problems.Add("$when PATH is $($raw.Kind), not ExpandString")
    }
    if (@($raw.Value -split ';') -cnotcontains $probe) {
        $problems.Add("$when PATH lost the unexpanded $probe entry")
    }
}

function Assert-PathCount($problems, [string]$dir, [int]$want, [string]$when) {
    $n = Get-PathCount $dir
    if ($n -ne $want) { $problems.Add("$when PATH has $n entries for $dir, want $want") }
}

function Get-Marker([string]$identifier) {
    $key = $hklm.OpenSubKey('SOFTWARE\embala\nupkg\' + $identifier)
    if ($null -eq $key) { return $null }
    try { [string]$key.GetValue('InstallDir') } finally { $key.Close() }
}

function Assert-Marker($problems, [string]$identifier, $want, [string]$when) {
    $got = Get-Marker $identifier
    if ($got -ne $want) { $problems.Add("$when marker is '$got', want '$want'") }
}

# Sorted relative paths of every file and directory under $dir, '/'-separated.
function Get-Listing([string]$dir) {
    if (-not [System.IO.Directory]::Exists($dir)) { return '<missing>' }
    $names = foreach ($entry in [System.IO.Directory]::GetFileSystemEntries($dir, '*', [System.IO.SearchOption]::AllDirectories)) {
        $entry.Substring($dir.Length + 1).Replace('\', '/')
    }
    (@($names) | Sort-Object) -join ','
}

function Assert-Listing($problems, [string]$dir, [string]$want, [string]$when) {
    $got = Get-Listing $dir
    if ($got -ne $want) { $problems.Add("$when $dir holds [$got], want [$want]") }
}

function Assert-Missing($problems, [string]$path, [string]$when) {
    if ([System.IO.Directory]::Exists($path) -or [System.IO.File]::Exists($path)) {
        $problems.Add("$when $path exists")
    }
}

# Runs an exe and returns its exit code and combined output. Native stderr
# must not trip the caller's 'Stop' preference.
function Invoke-Native([string]$exe, [string[]]$arguments) {
    $ErrorActionPreference = 'Continue'
    $out = & $exe @arguments 2>&1 | Out-String
    [pscustomobject]@{ Code = $LASTEXITCODE; Out = $out }
}

function Invoke-Choco([string[]]$arguments) {
    Invoke-Native $choco ($arguments + @('-y', '--no-progress', '--source', $pkgs))
}

function Invoke-Script([string]$script) {
    Invoke-Native 'powershell.exe' @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $script)
}

# The last few non-empty lines of some output, on one line, for FAIL details.
function Get-Tail([string]$out) {
    ((@($out -split "`r?`n") | Where-Object { $_.Trim() }) | Select-Object -Last 4) -join ' | '
}

function New-Problems { New-Object System.Collections.Generic.List[string] }

function Write-Result([string]$check, $problems) {
    if ($problems.Count -eq 0) { "PASS $check" } else { "FAIL ${check}: " + ($problems -join '; ') }
}
