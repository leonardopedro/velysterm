{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  name = "velyst-editor-dev-shell";

  # Tools for development
  nativeBuildInputs = [
    pkgs.cargo
    pkgs.rustc
    pkgs.rust-analyzer
    pkgs.pkg-config
    pkgs.lld
    pkgs.clang
  ];

  # System libraries required for the build
  buildInputs =
    pkgs.lib.optionals pkgs.stdenv.isLinux [
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
      pkgs.qcms
    ] ++
    pkgs.lib.optionals pkgs.stdenv.isDarwin [
      # macOS specific dependencies
      pkgs.libiconv
      pkgs.darwin.apple_sdk.frameworks.AppKit
      pkgs.darwin.apple_sdk.frameworks.CoreGraphics
      pkgs.darwin.apple_sdk.frameworks.CoreServices
      pkgs.darwin.apple_sdk.frameworks.CoreVideo
      pkgs.darwin.apple_sdk.frameworks.Foundation
      pkgs.darwin.apple_sdk.frameworks.IOKit
      pkgs.darwin.apple_sdk.frameworks.Security
      pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
    ];

  # Environment variables to help build processes find libraries
  shellHook = ''
    export RUST_SRC_PATH=${pkgs.rustPlatform.rustLibSrc};
    echo "Nix development shell for Velyst Editor is ready."
  '';
  
  idx = {
    extensions = [ "rust-lang.rust-analyzer" ];
    previews = { enable = true; previews = {}; };
      workspace = {
        onCreate = {
          install-add-determinism = "git lfs install && rustup default stable && cargo install add-determinism || echo 'skipped'";
          setup-podman = "./fix-podman-idx.sh";
        };
        onStart = {
          share-mount = "sudo mount --make-rshared /";
        };
      };
  };
}
