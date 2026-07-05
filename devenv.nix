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
  ];

  languages.rust = {
    enable = true;
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
    ];
  };

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
