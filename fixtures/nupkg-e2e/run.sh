#!/usr/bin/env bash
# `just nupkg-e2e`: build nupkgs that use `scripts = "generate"`, the plain
# style and neither, and check them on a Windows rig VM under real
# Chocolatey and as SYSTEM without it. Prints one PASS or FAIL line per
# check and exits non-zero if any check fails or reports nothing.
#
# Prerequisites: run inside `devenv shell` (mingw for `just fixtures`), the
# windows-test-rig script (RIG) with a VM config for RIG_E2E_VM whose
# `clean` snapshot has no Chocolatey, and guest internet for the Chocolatey
# bootstrap. The VM is reset first and reset and shut down at the end, on
# failure too. Full guest output goes to target/nupkg-e2e/guest.log.
set -euo pipefail

RIG=${RIG:-$HOME/.claude/skills/windows-test-rig/scripts/rig}
VM=${RIG_E2E_VM:-win11-test}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
work=$repo/target/nupkg-e2e
log=$work/guest.log
checks=(1-install 2-upgrade 3-lock 4-rename 5-uninstall 6-system 7-shim-regression 8-plain-guard)

rig() { "$RIG" -v "$VM" "$@"; }

cd "$repo"
just fixtures
cargo build -q -p embala
embala=$repo/target/debug/embala

rm -rf "$work"
mkdir -p "$work/payload" "$work/pkgs"
cp fixtures/hello/dist/hello.exe fixtures/hello/dist/hello-0.2.0.exe "$work/payload/"
echo 'extra file, shipped by e2e-gen 0.1.0 only' >"$work/payload/extra.txt"
echo 'shipped by e2e-gen 0.2.1, which never installs' >"$work/payload/v021.txt"

# pkg NAME VERSION DISPLAY-NAME IDENTIFIER NUPKG-BODY: build one package into
# $work/pkgs as NAME.VERSION.nupkg, the name a folder source expects.
pkg() {
	local name=$1 version=$2 display=$3 identifier=$4 body=$5 cfg
	cfg=$work/$name-$version.toml
	cat >"$cfg" <<EOF
[package]
name = "$name"
display-name = "$display"
version = "$version"
identifier = "$identifier"
publisher = "Carlos Hernandez"
description = "embala nupkg-e2e test package"

[nupkg]
$body
EOF
	"$embala" build --config "$cfg" --formats nupkg --out-dir "$work/out"
	mv "$work/out/$name-$version.nupkg" "$work/pkgs/$name.$version.nupkg"
}

gen_id=ca.hrndz.embala.e2e.gen
pkg e2e-gen 0.1.0 "Embala E2E Gen" $gen_id 'style = "embedded"
scripts = "generate"
files = [
  { src = "payload/hello.exe", dest = "hello-gen.exe" },
  { src = "payload/extra.txt", dest = "doc/extra.txt" },
]'
pkg e2e-gen 0.2.0 "Embala E2E Gen" $gen_id 'style = "embedded"
scripts = "generate"
files = [{ src = "payload/hello-0.2.0.exe", dest = "hello-gen.exe" }]'
pkg e2e-gen 0.2.1 "Embala E2E Gen" $gen_id 'style = "embedded"
scripts = "generate"
files = [
  { src = "payload/hello-0.2.0.exe", dest = "hello-gen.exe" },
  { src = "payload/v021.txt", dest = "v021.txt" },
]'
pkg e2e-gen 0.3.0 "Embala E2E Gen Renamed" $gen_id 'style = "embedded"
scripts = "generate"
files = [{ src = "payload/hello-0.2.0.exe", dest = "hello-gen.exe" }]'
pkg e2e-sys 0.1.0 "Embala E2E Système [o'k]" ca.hrndz.embala.e2e.sys 'style = "embedded"
scripts = "generate"
files = [
  { src = "payload/hello.exe", dest = "hello-sys.exe" },
  { src = "payload/extra.txt", dest = "doc/readme.txt" },
]'
pkg e2e-plain 0.1.0 "Embala E2E Plain" ca.hrndz.embala.e2e.plain 'style = "plain"
scripts = "generate"
files = [{ src = "payload/hello.exe", dest = "hello-plain.exe" }]'
pkg e2e-shim 0.1.0 "Embala E2E Shim" ca.hrndz.embala.e2e.shim 'style = "embedded"
files = [{ src = "payload/hello.exe", dest = "hello-shim.exe" }]'
pkg e2e-guard 0.1.0 "Embala E2E Guard" ca.hrndz.embala.e2e.guard 'style = "plain"
scripts = "generate"
files = [{ src = "payload/hello.exe", dest = "hello-guard.exe" }]'

finish() {
	local status=$?
	echo "resetting and shutting down $VM"
	rig reset >>"$log" 2>&1 || echo "WARN rig reset failed"
	rig down >>"$log" 2>&1 || echo "WARN rig down failed"
	exit "$status"
}
trap finish EXIT

: >"$log"
results=$work/results.txt
: >"$results"
# guest CMD...: run a rig command, keep its full output in the log, and pass
# its PASS/FAIL/INFO lines through. Its exit status is ignored: the verdict
# comes from the lines alone.
guest() {
	{ rig "$@" 2>&1 || true; } | tr -d '\r' | tee -a "$log" | grep -E '^(PASS|FAIL|INFO) ' | tee -a "$results" || true
}

echo "resetting $VM"
rig reset >>"$log" 2>&1
rig ssh 'New-Item -ItemType Directory -Force C:\e2e\pkgs | Out-Null' >>"$log" 2>&1
for f in lib.ps1 system-checks.ps1; do rig put "$here/$f" C:/e2e/ >>"$log" 2>&1; done
for f in "$work"/pkgs/*.nupkg; do rig put "$f" C:/e2e/pkgs/ >>"$log" 2>&1; done

# Check 6 first, while the machine has never seen Chocolatey.
guest ps "$here/system.ps1"
echo "installing Chocolatey"
{ rig ps "$here/install-choco.ps1" 2>&1 || true; } | tr -d '\r' >>"$log"
echo "INFO $(grep -E '^Chocolatey [0-9]' "$log" | tail -1 || echo 'Chocolatey did not install')"
guest ps "$here/choco.ps1"

failed=0
for c in "${checks[@]}"; do
	if grep -qx "PASS $c" "$results"; then
		continue
	fi
	failed=1
	grep -q "^FAIL $c:" "$results" || echo "FAIL $c: no result (see $log)"
done
if ((failed)); then
	echo "nupkg-e2e: FAILED"
	exit 1
fi
echo "nupkg-e2e: all ${#checks[@]} checks passed"
