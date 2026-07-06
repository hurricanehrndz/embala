-- Hand-written installer for the `hello` fixture (Phase 3).
--
-- Exercises the native embala.* API: a file copy, a start-menu shortcut, and an
-- Add/Remove-Programs registration built from the package identity. Every
-- mutating call is auto-logged, so the install can be rolled back on
-- cancel/error and reversed on uninstall (spec R12/R14).

local pkg = embala.package

embala.log("Installing " .. pkg.display_name .. " " .. pkg.version .. " (" .. embala.mode .. ")")

-- src is relative to payload_dir (the extracted zip), dest relative to
-- install_dir. The engine records the written file for reversal.
embala.fs.copy("hello.exe", "hello.exe")

-- Start-menu shortcut. `target` is resolved relative to install_dir by the
-- engine; the .lnk lands in the mode's Start Menu\Programs folder.
embala.shortcut.create {
  name = pkg.display_name,
  target = "hello.exe",
  location = "start-menu",
  description = pkg.description,
}

-- Register the ARP entry in the mode's hive. UninstallString points at the
-- uninstall.exe the engine writes into install_dir after this script; the
-- engine also removes this key automatically on uninstall (spec R14).
embala.arp.register {
  display_name = pkg.display_name,
  version = pkg.version,
  publisher = pkg.publisher,
  install_location = embala.install_dir,
  uninstall_string = '"' .. embala.install_dir .. '\\uninstall.exe"',
  display_icon = embala.install_dir .. '\\hello.exe',
}

embala.log("Install complete.")
