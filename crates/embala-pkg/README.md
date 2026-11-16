# embala-pkg

macOS flat-package (`.pkg`) builder: a xar archive with BOM and cpio payload,
the shape `productbuild` emits, written on any OS. Supports one or more
components (each with its own receipt id, install location and
pre/postinstall scripts, installed in order) and user-home installs.

Part of [embala](https://github.com/hurricanehrndz/embala), usable on its own.
