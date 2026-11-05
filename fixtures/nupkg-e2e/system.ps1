# nupkg-e2e check 6 driver, run as the rig's admin: seeds the PATH probe,
# runs system-checks.ps1 as SYSTEM through a one-shot scheduled task, and
# prints its result line.
. C:\e2e\lib.ps1

Set-PathProbe
# SYSTEM runs scripts from C:\e2e, which inherits Authenticated Users modify
# rights; restrict it to SYSTEM and Administrators first.
icacls $e2e /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null

$out = Join-Path $e2e 'system-out.txt'
Remove-Item -LiteralPath $out -Force -ErrorAction SilentlyContinue
$task = 'rig-embala-nupkg-e2e'
schtasks /create /tn $task /ru SYSTEM /sc once /st 23:59 /f /tr "powershell -NoProfile -ExecutionPolicy Bypass -File $e2e\system-checks.ps1" | Out-Null
schtasks /run /tn $task | Out-Null
for ($i = 0; $i -lt 300 -and -not (Test-Path -LiteralPath $out); $i++) { Start-Sleep 1 }
schtasks /delete /tn $task /f | Out-Null
if (Test-Path -LiteralPath $out) {
    Get-Content -LiteralPath $out -Encoding UTF8
} else {
    'FAIL 6-system: the SYSTEM task wrote no result within 300 s'
}
