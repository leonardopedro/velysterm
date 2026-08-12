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
          echo "[velysterm-shell] wayland/xkb/X11 dev libs + pkg-config on PATH"
          echo "[velysterm-shell] Vulkan ICD: RADV (AMD) + lavapipe via VK_DRIVER_FILES"
        '';
      };
    };
}