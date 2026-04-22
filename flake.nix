{
  description = "arkiv-op-reth";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    devshell = {
      url = "github:numtide/devshell";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {flake-parts, ...}:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin"];
      imports = [inputs.devshell.flakeModule];
      perSystem = {pkgs, ...}: {
        devshells.default = {
          env = [
            {
              name = "LIBCLANG_PATH";
              value = "${pkgs.llvmPackages.libclang.lib}/lib";
            }
            {
              name = "BINDGEN_EXTRA_CLANG_ARGS";
              value = "-isystem ${pkgs.glibc.dev}/include";
            }
          ];
          packages = with pkgs; [
            foundry
            solc
            python3
            vscode-solidity-server
            just

            # Rust + native build deps
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
            gcc
            gnumake
            pkg-config
            openssl
            llvmPackages.libclang
            mold
            glibc.dev
          ];
        };
      };
    };
}
