# nupkg-e2e check 6, run as SYSTEM by system.ps1 before Chocolatey exists on
# the machine: extract a generated embedded package and a generated plain
# package and run their scripts with plain powershell.exe -File, as a
# script-only installer would. Writes its result lines to C:\e2e\system-out.txt.
. C:\e2e\lib.ps1

$out = Join-Path $e2e 'system-out.txt'
$p = New-Problems
try {
    if (-not [System.Security.Principal.WindowsIdentity]::GetCurrent().IsSystem) {
        $p.Add('not running as SYSTEM: ' + [System.Security.Principal.WindowsIdentity]::GetCurrent().Name)
    }
    # The scripts must work with no Chocolatey on the machine at all (R15).
    if ($env:ChocolateyInstall -or (Test-Path -LiteralPath (Join-Path $env:ProgramData 'chocolatey'))) {
        $p.Add('Chocolatey is present before check 6')
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem

    # A display name with a non-ASCII letter, a quote and brackets (R15).
    $sysName = 'Embala E2E Syst' + [char]0x00E8 + 'me [o''k]'
    $cases = @(
        [pscustomobject]@{ Name = 'e2e-sys'; Id = 'ca.hrndz.embala.e2e.sys'; Display = $sysName; Install = 'chocolateyInstall.ps1'; Uninstall = 'chocolateyUninstall.ps1'; Listing = 'doc,doc/readme.txt,hello-sys.exe' },
        [pscustomobject]@{ Name = 'e2e-plain'; Id = 'ca.hrndz.embala.e2e.plain'; Display = 'Embala E2E Plain'; Install = 'install.ps1'; Uninstall = 'uninstall.ps1'; Listing = 'hello-plain.exe' }
    )
    foreach ($c in $cases) {
        $tools = Join-Path $e2e "x\$($c.Name)\tools"
        Remove-Item -LiteralPath (Join-Path $e2e "x\$($c.Name)") -Recurse -Force -ErrorAction SilentlyContinue
        [System.IO.Compression.ZipFile]::ExtractToDirectory((Join-Path $pkgs "$($c.Name).0.1.0.nupkg"), (Join-Path $e2e "x\$($c.Name)"))
        $install = Join-Path $tools $c.Install
        $uninstall = Join-Path $tools $c.Uninstall
        $dir = [System.IO.Path]::Combine($programFiles, $c.Display + ' (nupkg)')
        $tag = $c.Name

        $r = Invoke-Script $install
        if ($r.Code -ne 0) { $p.Add("$tag install exited $($r.Code): $(Get-Tail $r.Out)") }
        Assert-Listing $p $dir $c.Listing "$tag after install"
        Assert-PathCount $p $dir 1 "$tag after install"
        Assert-PathIntact $p "$tag after install"
        Assert-Marker $p $c.Id $dir "$tag after install"

        $r = Invoke-Script $uninstall
        if ($r.Code -ne 0) { $p.Add("$tag uninstall exited $($r.Code): $(Get-Tail $r.Out)") }
        Assert-Missing $p $dir "$tag after uninstall"
        Assert-PathCount $p $dir 0 "$tag after uninstall"
        Assert-PathIntact $p "$tag after uninstall"
        Assert-Marker $p $c.Id $null "$tag after uninstall"

        $r = Invoke-Script $uninstall
        if ($r.Code -ne 0) { $p.Add("$tag second uninstall exited $($r.Code): $(Get-Tail $r.Out)") }
        Assert-Missing $p $dir "$tag after second uninstall"
        Assert-PathCount $p $dir 0 "$tag after second uninstall"
        Assert-PathIntact $p "$tag after second uninstall"
        Assert-Marker $p $c.Id $null "$tag after second uninstall"

        # Interrupted install: a run stopped after renaming <dir> to
        # <dir>.old, with a stale <dir>.new beside it. The next install
        # completes.
        $r = Invoke-Script $install
        if ($r.Code -ne 0) { $p.Add("$tag reinstall exited $($r.Code): $(Get-Tail $r.Out)") }
        [System.IO.Directory]::Move($dir, $dir + '.old')
        [void][System.IO.Directory]::CreateDirectory($dir + '.new')
        [System.IO.File]::WriteAllText([System.IO.Path]::Combine($dir + '.new', 'stale.txt'), 'stale')
        $r = Invoke-Script $install
        if ($r.Code -ne 0) { $p.Add("$tag interrupted install exited $($r.Code): $(Get-Tail $r.Out)") }
        Assert-Listing $p $dir $c.Listing "$tag after interrupted install"
        Assert-Missing $p ($dir + '.old') "$tag after interrupted install"
        Assert-Missing $p ($dir + '.new') "$tag after interrupted install"
        Assert-PathCount $p $dir 1 "$tag after interrupted install"
        Assert-PathIntact $p "$tag after interrupted install"
        Assert-Marker $p $c.Id $dir "$tag after interrupted install"

        $r = Invoke-Script $uninstall
        if ($r.Code -ne 0) { $p.Add("$tag final uninstall exited $($r.Code): $(Get-Tail $r.Out)") }
        Assert-Missing $p $dir "$tag after final uninstall"
        Assert-PathCount $p $dir 0 "$tag after final uninstall"
    }
} catch {
    $p.Add('unexpected error: ' + $_.Exception.Message)
}
# Written aside and renamed, so system.ps1 never reads a half-written file.
[System.IO.File]::WriteAllText($out + '.tmp', (Write-Result '6-system' $p))
[System.IO.File]::Move($out + '.tmp', $out)
