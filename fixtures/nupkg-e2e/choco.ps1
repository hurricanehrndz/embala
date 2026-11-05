# nupkg-e2e checks 1 to 5, 7 and 8: the packages under real Chocolatey, run
# as the rig's elevated admin. Prints one PASS or FAIL line per check, and
# INFO lines for facts the report must carry.
. C:\e2e\lib.ps1

$bin = Join-Path $chocoRoot 'bin'
$lib = Join-Path $chocoRoot 'lib'

# Chocolatey's own install may rewrite PATH; seed the probe again so the
# checks below measure only the generated scripts.
Set-PathProbe

# --- e2e-gen: embedded, scripts = "generate"; 0.1.0 -> 0.2.0 -> (0.2.1
# locked) -> 0.3.0 renamed -> uninstall.
$genId = 'ca.hrndz.embala.e2e.gen'
$genDir = Join-Path $programFiles 'Embala E2E Gen (nupkg)'
$genRenamed = Join-Path $programFiles 'Embala E2E Gen Renamed (nupkg)'
$genExe = Join-Path $genDir 'hello-gen.exe'

# 1. Install 0.1.0.
$p = New-Problems
$r = Invoke-Choco @('install', 'e2e-gen', '--version', '0.1.0')
if ($r.Code -ne 0) { $p.Add("choco install exited $($r.Code): $(Get-Tail $r.Out)") }
Assert-Listing $p $genDir 'doc,doc/extra.txt,hello-gen.exe' 'after install'
Assert-PathCount $p $genDir 1 'after install'
Assert-PathIntact $p 'after install'
Assert-Marker $p $genId $genDir 'after install'
Assert-Missing $p (Join-Path $bin 'hello-gen.exe') 'after install'
if ([System.IO.File]::Exists($genExe)) {
    $v = Invoke-Native $genExe @()
    if ($v.Out -notmatch 'fixture 0\.1\.0') { $p.Add("installed exe printed '$($v.Out.Trim())'") }
}
Write-Result '1-install' $p

# 2. Upgrade to 0.2.0, which drops doc/extra.txt.
$p = New-Problems
$r = Invoke-Choco @('upgrade', 'e2e-gen', '--version', '0.2.0')
if ($r.Code -ne 0) { $p.Add("choco upgrade exited $($r.Code): $(Get-Tail $r.Out)") }
Assert-Listing $p $genDir 'hello-gen.exe' 'after upgrade'
Assert-PathCount $p $genDir 1 'after upgrade'
Assert-PathIntact $p 'after upgrade'
Assert-Marker $p $genId $genDir 'after upgrade'
foreach ($leftover in @('.new', '.old', '.removing')) { Assert-Missing $p ($genDir + $leftover) 'after upgrade' }
if ([System.IO.File]::Exists($genExe)) {
    $v = Invoke-Native $genExe @()
    if ($v.Out -notmatch 'fixture 0\.2\.0') { $p.Add("installed exe printed '$($v.Out.Trim())'") }
}
Write-Result '2-upgrade' $p

# 3. Lock: with hello-gen.exe running from the install directory, upgrade
# (to 0.2.1, same display-name) and uninstall both fail and change nothing.
$p = New-Problems
$inUse = 'Embala E2E Gen is in use; close it and retry'
$proc = $null
try {
    $proc = Start-Process -FilePath $genExe -ArgumentList '--sleep', '600' -PassThru -WindowStyle Hidden
    Start-Sleep -Seconds 2
    if ($proc.HasExited) { $p.Add("hello-gen.exe --sleep exited early with $($proc.ExitCode)") }

    $r = Invoke-Choco @('upgrade', 'e2e-gen', '--version', '0.2.1')
    if ($r.Code -eq 0) { $p.Add('choco upgrade succeeded while locked') }
    if (-not $r.Out.Contains($inUse)) { $p.Add("choco upgrade output lacks '$inUse': $(Get-Tail $r.Out)") }
    Assert-Listing $p $genDir 'hello-gen.exe' 'after locked upgrade'
    Assert-PathCount $p $genDir 1 'after locked upgrade'
    Assert-Marker $p $genId $genDir 'after locked upgrade'
    Assert-Missing $p ($genDir + '.new') 'after locked upgrade'
    Assert-PathIntact $p 'after locked upgrade'

    $r = Invoke-Choco @('uninstall', 'e2e-gen')
    if ($r.Code -eq 0) { $p.Add('choco uninstall succeeded while locked') }
    if (-not $r.Out.Contains($inUse)) { $p.Add("choco uninstall output lacks '$inUse': $(Get-Tail $r.Out)") }
    Assert-Listing $p $genDir 'hello-gen.exe' 'after locked uninstall'
    Assert-PathCount $p $genDir 1 'after locked uninstall'
    Assert-PathIntact $p 'after locked uninstall'
    Assert-Marker $p $genId $genDir 'after locked uninstall'

    # Why the scripts probe with an exclusive open, not the rename alone:
    # Windows lets a directory be renamed while an exe inside runs, unless
    # the process's working directory is inside it. Recorded, not asserted.
    try {
        [System.IO.Directory]::Move($genDir, $genDir + '.probe')
        [System.IO.Directory]::Move($genDir + '.probe', $genDir)
        'INFO 3-lock: renaming the install directory succeeded while hello-gen.exe ran'
    } catch {
        'INFO 3-lock: renaming the install directory failed while hello-gen.exe ran'
    }
} finally {
    if ($proc -and -not $proc.HasExited) {
        $proc.Kill()
        $proc.WaitForExit()
    }
}
Write-Result '3-lock' $p

# 4. Upgrade to 0.3.0, whose display-name changed: the old directory and its
# PATH entry go.
$p = New-Problems
$r = Invoke-Choco @('upgrade', 'e2e-gen', '--version', '0.3.0')
if ($r.Code -ne 0) { $p.Add("choco upgrade exited $($r.Code): $(Get-Tail $r.Out)") }
Assert-Missing $p $genDir 'after rename'
Assert-Missing $p ($genDir + '.removing') 'after rename'
Assert-PathCount $p $genDir 0 'after rename'
Assert-Listing $p $genRenamed 'hello-gen.exe' 'after rename'
Assert-PathCount $p $genRenamed 1 'after rename'
Assert-PathIntact $p 'after rename'
Assert-Marker $p $genId $genRenamed 'after rename'
Write-Result '4-rename' $p

# 5. Uninstall.
$p = New-Problems
$r = Invoke-Choco @('uninstall', 'e2e-gen')
if ($r.Code -ne 0) { $p.Add("choco uninstall exited $($r.Code): $(Get-Tail $r.Out)") }
Assert-Missing $p $genRenamed 'after uninstall'
Assert-Missing $p ($genRenamed + '.removing') 'after uninstall'
Assert-PathCount $p $genRenamed 0 'after uninstall'
Assert-PathIntact $p 'after uninstall'
Assert-Marker $p $genId $null 'after uninstall'
Assert-Missing $p (Join-Path $lib 'e2e-gen') 'after uninstall'
Write-Result '5-uninstall' $p

# 7. Regression: an embedded package without the new keys is still shimmed.
$p = New-Problems
$r = Invoke-Choco @('install', 'e2e-shim')
if ($r.Code -ne 0) { $p.Add("choco install exited $($r.Code): $(Get-Tail $r.Out)") }
$shim = Join-Path $bin 'hello-shim.exe'
if ([System.IO.File]::Exists($shim)) {
    $v = Invoke-Native $shim @()
    if ($v.Out -notmatch 'fixture 0\.1\.0') { $p.Add("shim printed '$($v.Out.Trim())'") }
} else {
    $p.Add("no shim at $shim")
}
Write-Result '7-shim-regression' $p

# 8. A plain package fails under choco with the guard's message and leaves
# no lib\<name> and no shim. Everything else it leaves is recorded (OQ1).
$p = New-Problems
$guard = 'Embala E2E Guard is not a Chocolatey package'
$r = Invoke-Choco @('install', 'e2e-guard')
if ($r.Code -eq 0) { $p.Add('choco install succeeded') }
if (-not $r.Out.Contains($guard)) { $p.Add("choco output lacks '$guard': $(Get-Tail $r.Out)") }
Assert-Missing $p (Join-Path $lib 'e2e-guard') 'after install'
foreach ($name in @('hello-guard.exe', 'hello-guard.exe.ignore')) { Assert-Missing $p (Join-Path $bin $name) 'after install' }
$left = foreach ($entry in Get-ChildItem -LiteralPath $chocoRoot -Recurse -Force -ErrorAction SilentlyContinue) {
    if ($entry.Name -like '*e2e-guard*' -or $entry.Name -like '*hello-guard*') {
        $entry.FullName.Substring($chocoRoot.Length + 1)
    }
}
$left = @($left | Where-Object { $_ -notlike 'logs\*' })
"INFO 8-plain-guard: choco exit code $($r.Code)"
"INFO 8-plain-guard: Chocolatey left behind: " + $(if ($left.Count) { $left -join ', ' } else { 'nothing' })
Write-Result '8-plain-guard' $p
