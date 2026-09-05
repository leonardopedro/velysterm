# Nix devShell for the velysterm workspace.
#
# velysterm builds both bevy-free crates (kanva, kanva_svg, kanva_typst,
# kernel_client, mathed_biblio, mathed_core, mathed_mini, typst_element,
# typst_imaging) and the bevy GUI crate (mathed). The bevy crate pulls in
# wayland/xkbcommon/X11 via winit, which requires system dev libraries plus
# pkg-config to locate them (wayland-sys fails without them, e.g. on NixOS).
#
# Use with:  nix develop
# or:        nix-shell
# The CUDA/GPU toolkit (unfer's flake) is deliberately NOT included here —
# velysterm is CPU-only.

{
  description = "velysterm workspace dev shell (bevy + wayland/xkb/X11 dev libs)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      pkgs = import nixpkgs { system = "x86_64-linux"; };
    in
    {
      devShells.x86_64-linux.default = pkgs.mkShell {
        name = "velysterm";

        # bevy/winit build deps: pkg-config to find the libs, then the
        # wayland/X11/xkb client libraries + GL/EGL + ALSA (bevy audio) +
        # the Vulkan loader (winit's GL backend selection).
        #
        # `mesa` provides the actual Vulkan ICD drivers (RADV for AMD, lavapipe
        # software fallback); the loader alone finds no physical device, which
        # makes winit panic at runtime ("no Vulkan ICD"). `vulkan-validation-layers`
        # and `vulkan-tools` (vulkaninfo) are for diagnostics.
        packages = with pkgs; [
          pkg-config
          wayland
          wayland-protocols
          libxkbcommon
          libGL
          xorg.libX11
          xorg.libXcursor
          xorg.libXi
          xorg.libXrandr
          alsa-lib
          udev
          vulkan-loader
          mesa
          vulkan-validation-layers
          vulkan-tools
          rustup
        ];

        RUSTUP_TOOLCHAIN = "stable";

        # Keep the pkg-config-generated LD_LIBRARY_PATH hints for the
        # vendored static libs winit/bevy link at runtime.
        shellHook = ''
          # Expose the mesa ICDs so the Vulkan loader finds a physical device.
          # VK_DRIVER_FILES is the modern name; VK_ICD_FILENAMES is the legacy
          # alias some loaders still read. `lvp` (lavapipe) is the CPU fallback
          # that keeps bevy/winit running even when no GPU ICD matches.
          export VK_DRIVER_FILES="${pkgs.mesa}/share/vulkan/icd.d/radeon_icd.x86_64.json:${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.x86_64.json"
          export VK_ICD_FILENAMES="$VK_DRIVER_FILES"

          # Modern nixpkgs (systemd >= 25x) no longer ships libudev.pc, but the
          # `libudev-sys` build.rs insists on `pkg-config find_library("libudev")`.
          # Generate a minimal libudev.pc pointing at the udev store lib so the
          # bevy/bevy_audio->cpal->libudev-sys chain can link.
          export _UNFER_UDEV_PCDIR="$HOME/.cache/unfer-libudev-pc"
          mkdir -p "$_UNFER_UDEV_PCDIR"
          _UNFER_UDEV_LIB="${pkgs.udev}/lib"
          cat > "$_UNFER_UDEV_PCDIR/libudev.pc" <<EOF
prefix=${pkgs.udev}
exec_prefix=${pkgs.udev}
libdir=$_UNFER_UDEV_LIB
includedir=${pkgs.udev.dev}/include

Name: libudev
Description: udev library
Version: 1
Libs: -L$_UNFER_UDEV_LIB -ludev
Cflags: -I${pkgs.udev.dev}/include
EOF
          export PKG_CONFIG_PATH="$_UNFER_UDEV_PCDIR:$PKG_CONFIG_PATH"

          echo "[velysterm-shell] wayland/xkb/X11 dev libs + pkg-config on PATH"
          echo "[velysterm-shell] Vulkan ICD: RADV (AMD) + lavapipe via VK_DRIVER_FILES"
          echo "[velysterm-shell] generated libudev.pc -> $PKG_CONFIG_PATH"
        '';
      };

      # The ipykernel e2e env: python with ipykernel + jupyter_client
      # for `crates/kernel_client/scripts/ipykernel_stdio_bridge.py`
      # and `run_ipykernel_e2e.sh` — a REAL Python kernel driven over
      # the framed stdio transport behind mathed's `\kernel` segments.
      # Much smaller than `default` (no bevy/winit deps); host cargo
      # stays visible, so `cargo run -p mathed_mini` works inside it.
      devShells.x86_64-linux.python-kernel = pkgs.mkShell {
        name = "velysterm-python-kernel";
        packages = [
          (pkgs.python3.withPackages (ps: [ ps.ipykernel ps.jupyter-client ]))
        ];
        shellHook = ''
          echo "[velysterm-python-kernel] python3 with ipykernel + jupyter_client"
        '';
      };
    };
}