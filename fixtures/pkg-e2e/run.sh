#!/usr/bin/env bash
# `just pkg-e2e`: build a four-component .pkg (an .app tree with framework
# symlinks, a CLI under a path with a space, a LaunchAgent, a LaunchDaemon
# listed last) and install it with `installer` on a macos-test-rig host.
# Prints one PASS or FAIL line per check and exits non-zero if any fails.
#
# Prerequisites: the macos-test-rig script (MAC) and a host (MAC_E2E_HOST)
# with passwordless sudo. The install touches /Applications and /Library; the
# plists are Disabled and never bootstrapped, and everything installed plus
# the receipts is removed afterwards, on failure too.
set -euo pipefail

MAC=${MAC:-$HOME/.claude/skills/macos-test-rig/scripts/mac}
HOST=${MAC_E2E_HOST:-mac1}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
work=$repo/target/pkg-e2e
id=ca.hrndz.embala.e2e

cd "$repo"
cargo build -q -p embala
rm -rf "$work"
mkdir -p "$work"
cd "$work"

# The .app: a framework laid out the way real ones are, with Current and
# top-level links into Versions/A. Built here, not committed, so checkouts
# without symlink support still run the rest of the test suite.
fw=app/EmbalaE2E.app/Contents/Frameworks/Fake.framework
mkdir -p app/EmbalaE2E.app/Contents/MacOS "$fw/Versions/A/Resources"
printf '#!/bin/sh\necho embala-e2e app\n' >app/EmbalaE2E.app/Contents/MacOS/EmbalaE2E
chmod 755 app/EmbalaE2E.app/Contents/MacOS/EmbalaE2E
printf '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>%s.app</string><key>CFBundleExecutable</key><string>EmbalaE2E</string></dict></plist>\n' "$id" >app/EmbalaE2E.app/Contents/Info.plist
echo 'fake framework binary' >"$fw/Versions/A/Fake"
echo 'framework resource' >"$fw/Versions/A/Resources/res.txt"
echo dotfile >"$fw/Versions/A/Resources/.keep"
ln -s A "$fw/Versions/Current"
ln -s Versions/Current/Fake "$fw/Fake"
ln -s Versions/Current/Resources "$fw/Resources"

printf '#!/bin/sh\necho embala-e2e cli\n' >embala-e2e
chmod 755 embala-e2e
for kind in agent daemon; do
	printf '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict><key>Label</key><string>%s.%s</string><key>Program</key><string>/usr/bin/true</string><key>Disabled</key><true/></dict></plist>\n' "$id" "$kind" >"$kind.plist"
done

# Every component's postinstall appends its name, so the file records the
# order Installer ran them in. The daemon's also records whether everything
# the other components install was already in place.
for name in app cli agent daemon; do
	mkdir -p "scripts/$name"
	{
		echo '#!/bin/sh'
		echo "mkdir -p /tmp/embala-pkg-e2e; echo $name >>/tmp/embala-pkg-e2e/order"
	} >"scripts/$name/postinstall"
done
cat >>scripts/daemon/postinstall <<'EOF'
if [ -x /Applications/EmbalaE2E.app/Contents/MacOS/EmbalaE2E ] &&
	[ -x "/Library/Application Support/EmbalaE2E/embala-e2e" ] &&
	[ -f /Library/LaunchAgents/ca.hrndz.embala.e2e.agent.plist ]; then
	echo yes >/tmp/embala-pkg-e2e/daemon-saw-all
fi
EOF
chmod 755 scripts/*/postinstall

cat >embala.toml <<EOF
[package]
name = "embala-e2e"
display-name = "Embala E2E"
version = "1.0.0"
identifier = "$id"
publisher = "Carlos Hernandez"
description = "embala multi-component pkg end-to-end fixture"

[pkg]

[[pkg.component]]
name = "app"
install-location = "/Applications"
files = [{ src = "app", dest = "." }]
scripts = "scripts/app"

[[pkg.component]]
name = "cli"
install-location = "/Library/Application Support/EmbalaE2E"
files = [{ src = "embala-e2e", dest = "embala-e2e" }]
scripts = "scripts/cli"

[[pkg.component]]
name = "agent"
install-location = "/Library/LaunchAgents"
files = [{ src = "agent.plist", dest = "$id.agent.plist", mode = 0o644 }]
scripts = "scripts/agent"

[[pkg.component]]
name = "daemon"
identifier = "$id-daemon"
install-location = "/Library/LaunchDaemons"
files = [{ src = "daemon.plist", dest = "$id.daemon.plist", mode = 0o644 }]
scripts = "scripts/daemon"
EOF
"$repo/target/debug/embala" build --config embala.toml --formats pkg --no-sign --out-dir dist

pkg=/tmp/embala-e2e-1.0.0.pkg
receipts="$id.app $id.cli $id.agent $id-daemon"
cleanup() {
	"$MAC" ssh "$HOST" "sudo rm -rf /Applications/EmbalaE2E.app '/Library/Application Support/EmbalaE2E' \
		/Library/LaunchAgents/$id.agent.plist /Library/LaunchDaemons/$id.daemon.plist /tmp/embala-pkg-e2e $pkg;
		for r in $receipts; do sudo pkgutil --forget \$r >/dev/null 2>&1 || true; done"
}
"$MAC" wake "$HOST" >/dev/null
cleanup
trap cleanup EXIT
"$MAC" put "$HOST" dist/embala-e2e-1.0.0.pkg "$pkg" >/dev/null
"$MAC" ssh "$HOST" "sudo installer -pkg $pkg -target /" >"$work/installer.log" 2>&1 ||
	{ cat "$work/installer.log"; exit 1; }

# Each check runs on the host and passes when it prints exactly `ok`.
fail=0
check() {
	local name=$1 cmd=$2 out
	out=$("$MAC" ssh "$HOST" "$cmd" 2>&1 || true)
	if [ "$out" = ok ]; then echo "PASS $name"; else
		echo "FAIL $name: $out"
		fail=1
	fi
}
a=/Applications/EmbalaE2E.app/Contents/Frameworks/Fake.framework
check 1-receipts "n=0; for r in $receipts; do pkgutil --pkg-info \$r >/dev/null && n=\$((n+1)); done; [ \$n = 4 ] && echo ok || echo \"\$n of 4 receipts\""
check 2-order "[ \"\$(tr '\n' ' ' </tmp/embala-pkg-e2e/order)\" = 'app cli agent daemon ' ] && echo ok || cat /tmp/embala-pkg-e2e/order"
check 3-daemon-last "[ \"\$(cat /tmp/embala-pkg-e2e/daemon-saw-all 2>/dev/null)\" = yes ] && echo ok || echo 'daemon postinstall ran before the rest was in place'"
check 4-symlinks "[ \"\$(readlink $a/Versions/Current)\" = A ] && [ \"\$(readlink $a/Fake)\" = Versions/Current/Fake ] && [ -L $a/Resources ] && [ \"\$(cat $a/Resources/res.txt)\" = 'framework resource' ] && echo ok || ls -lR $a"
check 5-modes "[ \"\$(stat -f %Lp '/Library/Application Support/EmbalaE2E/embala-e2e')\" = 755 ] && [ \"\$(stat -f %Lp /Library/LaunchDaemons/$id.daemon.plist)\" = 644 ] && [ \"\$(stat -f %Su:%Sg /Library/LaunchDaemons/$id.daemon.plist)\" = root:wheel ] && echo ok || echo 'wrong mode or owner'"
check 6-app-runs "[ \"\$(/Applications/EmbalaE2E.app/Contents/MacOS/EmbalaE2E)\" = 'embala-e2e app' ] && echo ok"
check 7-receipt-owns-link "pkgutil --files $id.app | grep -qx 'EmbalaE2E.app/Contents/Frameworks/Fake.framework/Versions/Current' && echo ok || echo 'symlink missing from the app receipt'"
check 8-dotfile "[ \"\$(cat $a/Resources/.keep)\" = dotfile ] && pkgutil --files $id.app | grep -q Resources/.keep\$ && echo ok || echo 'dotfile missing'"
exit $fail
