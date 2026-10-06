{
  # emailclient-plugin-sicompass: an IMAP and SMTP email client, as a sicompass
  # plugin. A plugin is a program, released for every platform sicompass runs
  # plugins on. On Linux that is a static musl build, which nixpkgs' rustc has
  # no std for, so the toolchain comes from rust-overlay. flake.lock pins it.
  description = "emailclient-plugin-sicompass: an IMAP and SMTP email client, a sicompass plugin";

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
          # This computer's plugin target, which `release-plugin.sh --dry-run`
          # builds. The other platforms are built on their own CI runners.
          pluginTarget = {
            "x86_64-linux" = "x86_64-unknown-linux-musl";
            "aarch64-linux" = "aarch64-unknown-linux-musl";
            "aarch64-darwin" = "aarch64-apple-darwin";
            "x86_64-darwin" = "x86_64-apple-darwin";
          }.${system};
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
            targets = [ pluginTarget ];
          };
          # A C compiler for musl, for the C the static Linux build compiles
          # (SQLite, bundled into the envelope cache). The shell's own cc
          # builds against glibc's headers, which leaves references to
          # `open64` and `__memcpy_chk` that musl does not have.
          muslCc = {
            "x86_64-linux" = pkgs.pkgsCross.musl64.stdenv.cc;
            "aarch64-linux" = pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc;
          }.${system} or null;
          ccVar = "CC_" + builtins.replaceStrings [ "-" ] [ "_" ] pluginTarget;
        in
        {
          default = pkgs.mkShell ({
            buildInputs = with pkgs; [
              rustToolchain
              # scripts/release-plugin.sh reads plugin.json with it.
              jq
            ] ++ pkgs.lib.optional (muslCc != null) muslCc;
            shellHook = ''
              export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library";
            '';
          } // pkgs.lib.optionalAttrs (muslCc != null) {
            ${ccVar} = "${muslCc}/bin/${muslCc.targetPrefix}cc";
          });
        });
    };
}
