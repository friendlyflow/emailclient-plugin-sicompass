{
  # emailclient_plugin_sicompass: an IMAP and SMTP email client, a sicompass WASM plugin. The
  # plugin is built for wasm32-wasip2, which nixpkgs' rustc
  # has no std for, so the toolchain comes from rust-overlay (as in
  # sicompass-plugin-sdk's flake). flake.lock pins it.
  description = "emailclient_plugin_sicompass: an IMAP and SMTP email client, a sicompass WASM plugin";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    nixpkgs-x86-darwin.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, nixpkgs-x86-darwin, rust-overlay }:
    let
      supportedSystems = [ "aarch64-linux" "aarch64-darwin" "x86_64-linux" "x86_64-darwin" ];
      nixpkgsInputFor = system:
        if system == "x86_64-darwin" then nixpkgs-x86-darwin else nixpkgs;
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      nixpkgsFor = forAllSystems (system:
        import (nixpkgsInputFor system) {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        });
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = nixpkgsFor.${system};
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
            targets = [ "wasm32-wasip2" ];
          };
        in
        {
          default = pkgs.mkShell {
            buildInputs = with pkgs; [
              rustToolchain
              # Validating the component.
              wasm-tools
              # scripts/release-plugin.sh reads plugin.json with it.
              jq
            ];
            # ring (rustls's crypto) has C and needs a compiler that targets
            # wasm: the host's gcc does not. The unwrapped clang, since the
            # wrapper would add the host's libc. ring needs no libc headers.
            CC_wasm32_wasip2 = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
            AR_wasm32_wasip2 = "${pkgs.llvmPackages.bintools-unwrapped}/bin/llvm-ar";
            shellHook = ''
              export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library";
            '';
          };
        });
    };
}
