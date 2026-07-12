{ pkgs, ... }:

{
  # https://devenv.sh/packages/
  packages = with pkgs; [
    just
    # MSI differential-testing oracle: wixl builds reference MSIs on Linux,
    # msiinfo/msidump inspect table contents.
    msitools
    # Cross toolchain for the fixture Windows test exe (x86_64-w64-mingw32-cc);
    # embala itself never compiles user artifacts.
    pkgsCross.mingwW64.buildPackages.gcc
    # setup.exe stub cross-compiler: zig supplies the mingw CRT + a resource
    # compiler (`zig rc`) for BOTH windows arches, cargo-zigbuild drives it.
    # These serve `just stubs` ONLY — `embala build` never compiles anything,
    # it only include_bytes!-embeds the committed stubs.
    zig
    cargo-zigbuild
    # Signing tools the `[sign]` config shells out to in local/e2e tests:
    # osslsigncode (Authenticode on Linux), rcodesign (Apple code signing),
    # jsign (Authenticode for .msi — osslsigncode can't parse the msi crate's
    # CFB v4 output).
    osslsigncode
    rcodesign
    jsign
  ];

  languages.rust = {
    enable = true;
    # The `nixpkgs` channel cannot add cross-compile targets; the fenix-backed
    # `stable` channel can, which `just stubs` needs for the windows-gnu std.
    channel = "stable";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
    ];
    # Windows std for the setup-exe stub cross-build (`just stubs` only).
    # aarch64 Windows has no mingw-gcc target in Rust; its GNU flavour is the
    # LLVM-based `gnullvm` triple, which zig cross-compiles natively.
    targets = [
      "x86_64-pc-windows-gnu"
      "aarch64-pc-windows-gnullvm"
    ];
  };

  # The mingw cross package (above) exports `CC=x86_64-w64-mingw32-gcc` into the
  # shell, which the `cc` crate would otherwise use for HOST C builds too — so
  # mlua's vendored Lua fails to compile natively. Pin the host-target compiler
  # to native `cc`; cargo-zigbuild sets its own per-target CC for the stubs.
  env.CC_x86_64_unknown_linux_gnu = "cc";

  treefmt = {
    enable = true;
    config.programs = {
      nixfmt.enable = true;
      rustfmt.enable = true;
      yamlfmt.enable = true;
      mdformat.enable = true;
    };
  };

  # https://devenv.sh/git-hooks/
  # Run treefmt on commit. Enabling the `treefmt` module above already wires
  # its config-baked wrapper into this hook (git-hooks.hooks.treefmt.package),
  # so we only need to switch the hook on.
  git-hooks.hooks.treefmt.enable = true;

  git-hooks.hooks.clippy.enable = true;
}
