# embala-setup

Windows `setup.exe` builder. Appends a payload overlay (files, install and
uninstall scripts, manifest) onto a prebuilt Win32 runtime stub, so building a
setup.exe never compiles anything. The stubs come from `embala-setup-runtime`
and the overlay format from `embala-setup-overlay`.

Part of [embala](https://github.com/hurricanehrndz/embala).
