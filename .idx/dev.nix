# To learn more about how to use Nix to configure your environment
# see: https://firebase.google.com/docs/studio/customize-workspace
{ pkgs, ... }: {
  # Which nixpkgs channel to use.
  channel = "stable-25.05"; # or "unstable"

  # Use https://search.nixos.org/packages to find packages
  packages = [
    pkgs.rustup
    pkgs.gcc
    pkgs.git-lfs
    pkgs.rust-analyzer
    pkgs.pkg-config
    pkgs.lld
    pkgs.clang
    # Dependencies for Bevy (windowing, audio, graphics)
    pkgs.alsa-lib
    pkgs.vulkan-loader
    pkgs.libxkbcommon
    pkgs.wayland
    pkgs.xorg.libX11
    pkgs.xorg.libXcursor
    pkgs.xorg.libXrandr
    pkgs.xorg.libXi
    pkgs.xorg.libXfixes
    pkgs.udev
    # Dependencies for Typst and its ecosystem
    pkgs.fontconfig
    pkgs.harfbuzz
    pkgs.icu
    pkgs.openssl
  ];

  # Sets environment variables in the workspace
  env = {
    RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
  };

  idx = {
    extensions = [ "rust-lang.rust-analyzer" ];
    previews = {
      enable = true;
      previews = {};
    };
    workspace = {
      onCreate = {
        install-cargo-determinism = "git lfs install && rustup default stable && cargo install cargo-determinism || echo 'skipped'";
        setup-podman = "./fix-podman-idx.sh";
      };
      onStart = {
        share-mount = "sudo mount --make-rshared /";
      };
    };
  };
}
