# Nix devShell for the velysterm workspace.
#
# velysterm builds both bevy-free crates (kanva, kanva_svg, kanva_typst,
# kernel_client, mathed_biblio, mathed_core, mathed_mini, typst_element,
# typst_imaging) and the bevy GUI crate (mathed). The bevy crate pulls in
# wayland/xkbcommon/X11 via winit, which requires system dev libraries plus
# pkg-config to locate them (e.g. on NixOS), and the Skia imaging backend
# (mathed_skia) links freetype + fontconfig — all covered here, so the whole
# workspace including mathed_skia is ONE command: `nix develop -c cargo test
# --workspace`.
#
# Use with:  nix develop
# or:        nix-shell
# The CUDA/GPU toolkit (unfer's flake) is deliberately NOT included here —
# velysterm is CPU-only.

{
  description = "velysterm workspace dev shell (bevy + wayland/xkb/X11 + freetype/fontconfig)";

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
        # The X11 libs are named at the top level (`libx11`, `libxcursor`,
        # `libxi`, `libxrandr`) rather than through `xorg.*`: nixpkgs
        # deprecated the `xorg` package set and warns on every reference
        # ("'xorg.libX11' has been renamed to 'libx11'"). Same derivations,
        # no `xorg` attr in sight, so the evaluation is quiet.
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
          libx11
          libxcursor
          libxi
          libxrandr
          alsa-lib
          udev
          vulkan-loader
          mesa
          vulkan-validation-layers
          vulkan-tools
          # mathed_skia: skia-safe's prebuilt Skia links -lfreetype
          # -lfontconfig and (C++) libstdc++; with these in the shell the
          # RUSTFLAGS hint below makes `cargo test -p mathed_skia` work from
          # the flake alone (was: a second `nix-shell -p freetype fontconfig`
          # just for it, which never covered libstdc++ either — its dir is
          # resolved from the shell's own `cc` below, so no pinned attr name
          # can rot).
          freetype
          fontconfig
          rustup
          # python3 for this repo's own gates (`scripts/check-links`, and the
          # python gates inside `scripts/verify-invariants`): the devshell is
          # the one toolchain source, so the documented commands run inside
          # `nix develop` instead of a second `nix-shell -p python3` wrapper
          # (CI's ubuntu runners ship python3 either way).
          python3
        ];

        # NO RUSTUP_TOOLCHAIN here on purpose. It used to be set to "stable",
        # which *overrides* rust-toolchain.toml (env var beats the toolchain
        # file), so `nix develop` silently ran stable (1.98.x) while CI and
        # bare-shell builds ran the pinned 1.99.0 — two compilers, two clippy
        # lint sets: clippy under the shell failed on a lint CI never sees
        # (chunks_exact_to_as_chunks in mathed_core/transform.rs). The
        # toolchain file is the single source of truth ("Bump deliberately");
        # rustup resolves it from the cwd like CI does.

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

          # mathed_skia links against the system freetype + fontconfig that
          # skia-safe's prebuilt Skia was built with (see
          # crates/mathed_skia/src/lib.rs, "Build requirements"). Provide the
          # two libdirs at link time and as rpath so the test binary also runs
          # — exported shell-wide so ONE command covers the whole workspace:
          #   nix develop -c cargo test --workspace
          # (Appending, so a caller-supplied RUSTFLAGS survives entry.)
          _FT="$(pkg-config --variable=libdir freetype2)"
          _FC="$(pkg-config --variable=libdir fontconfig)"
          # libstdc++.so.6: same directory the linker resolved -lstdc++ from.
          _STDCXX="$(dirname "$(cc -print-file-name=libstdc++.so.6)")"
          export RUSTFLAGS="''${RUSTFLAGS:+$RUSTFLAGS }-L$_FT -L$_FC -C link-arg=-Wl,-rpath,$_FT -C link-arg=-Wl,-rpath,$_FC -C link-arg=-Wl,-rpath,$_STDCXX"

          # stderr, not stdout: `mathed_mini --layout-json` (and anything
          # else piping this shell's stdout as a data contract — emthin's
          # scripts/agent/preview.sh prints ONE compact JSON line) would
          # otherwise be polluted by these banners.
          echo "[velysterm-shell] wayland/xkb/X11 dev libs + pkg-config on PATH" >&2
          echo "[velysterm-shell] Vulkan ICD: RADV (AMD) + lavapipe via VK_DRIVER_FILES" >&2
          echo "[velysterm-shell] generated libudev.pc -> $PKG_CONFIG_PATH" >&2
          echo "[velysterm-shell] freetype/fontconfig/libstdcxx rpath -> $_FT, $_FC, $_STDCXX (mathed_skia included)" >&2
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
          (pkgs.python3.withPackages (
            ps: [ ps.ipykernel ps.jupyter-client ps.matplotlib ]
          ))
        ];
        shellHook = ''
          echo "[velysterm-python-kernel] python3 with ipykernel + jupyter_client + matplotlib"
        '';
      };
    };
}