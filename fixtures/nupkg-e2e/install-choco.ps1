# nupkg-e2e: install Chocolatey with the official bootstrap from
# community.chocolatey.org (the guest needs internet), then print its version.
$ErrorActionPreference = 'Stop'
Set-ExecutionPolicy Bypass -Scope Process -Force
[System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor 3072
Invoke-Expression ((New-Object System.Net.WebClient).DownloadString('https://community.chocolatey.org/install.ps1'))
$choco = Join-Path $env:ProgramData 'chocolatey\bin\choco.exe'
if (-not (Test-Path -LiteralPath $choco)) { throw "Chocolatey did not install: no $choco" }
'Chocolatey ' + (& $choco --version)
