-- Hand-written uninstaller for the `hello` fixture (Phase 3).
--
-- This runs as an additive teardown hook: the engine ALSO replays install.log
-- LIFO afterwards, which is what actually removes the Start-menu shortcut and
-- the ARP entry (and re-deletes any file listed there — idempotently). So this
-- script only needs to demonstrate a custom action: a log line plus an explicit
-- removal of the payload file. `install_dir` is the uninstaller's own directory;
-- there is no payload_dir at uninstall time.

embala.log("Uninstalling " .. embala.package.display_name)

-- Idempotent with the engine's log replay (remove-if-exists).
embala.fs.remove("hello.exe")

-- Uninstall option (spec R2/R4): only wipe the data dir when the user ticked
-- "purge-data" in the confirm dialog (or passed /options=purge-data).
if embala.ui.selected("purge-data") then
  embala.log("Deleting all Hello data")
  embala.fs.remove_tree("data")
end
