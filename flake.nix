{
  description = "Profian Benefice";

  inputs.crane.inputs.flake-compat.follows = "flake-compat";
  inputs.crane.inputs.flake-utils.follows = "flake-utils";
  inputs.crane.inputs.nixpkgs.follows = "nixpkgs";
  inputs.crane.url = github:ipetkov/crane;
  inputs.enarx.inputs.fenix.follows = "fenix";
  inputs.enarx.inputs.flake-compat.follows = "flake-compat";
  inputs.enarx.inputs.flake-utils.follows = "flake-utils";
  inputs.enarx.url = github:enarx/enarx;
  inputs.fenix.inputs.nixpkgs.follows = "nixpkgs";
  inputs.fenix.url = github:nix-community/fenix;
  inputs.flake-compat.flake = false;
  inputs.flake-compat.url = github:edolstra/flake-compat;
  inputs.flake-utils.url = github:numtide/flake-utils;
  inputs.nixpkgs.url = github:NixOS/nixpkgs;
  inputs.rust-overlay.inputs.flake-utils.follows = "flake-utils";
  inputs.rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  inputs.rust-overlay.url = github:oxalica/rust-overlay;

  outputs = {
    self,
    crane,
    enarx,
    fenix,
    flake-utils,
    nixpkgs,
    ...
  }:
    with flake-utils.lib.system;
      flake-utils.lib.eachSystem [
        aarch64-darwin
        aarch64-linux
        x86_64-darwin
        x86_64-linux
      ] (
        system: let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [
              # TODO: Add Enarx overlay
              fenix.overlay
            ];
          };

          mkBin = pkgsTarget: args: let
            target = with pkgsTarget.targetPlatform.parsed; "${cpu.name}-${vendor.name}-${kernel.name}-${abi.name}";

            rust = with fenix.packages.${system};
              combine [
                stable.rustc
                stable.cargo
                targets.${target}.stable.rust-std
              ];

            craneLib = (crane.mkLib pkgs).overrideToolchain rust;

            src =
              pkgs.nix-gitignore.gitignoreRecursiveSource [
                "*.lock"
                "!Cargo.lock"

                "*.toml"
                "!Cargo.toml"

                "*.md"
                "*.nix"
                "/.github"
                "LICENSE"
              ]
              self;
          in
            craneLib.buildPackage (
              {
                inherit src;

                CARGO_BUILD_TARGET = target;

                buildInputs = with pkgsTarget; [
                  openssl
                ];
                nativeBuildInputs = with pkgs; [
                  pkg-config
                ];
              }
              // nixpkgs.lib.optionalAttrs (pkgsTarget.targetPlatform.isStatic) {
                CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";

                depsBuildBuild = with pkgsTarget; [
                  stdenv.cc
                ];
              }
              // args
            );

          nativeBin = mkBin pkgs {};
          staticBin = mkBin pkgs.pkgsStatic {};

          nativeDebugBin = mkBin pkgs {CARGO_PROFILE = "";};
          staticDebugBin = mkBin pkgs.pkgsStatic {CARGO_PROFILE = "";};

          cargo.toml = builtins.fromTOML (builtins.readFile "${self}/Cargo.toml");

          buildImage = bin:
            pkgs.dockerTools.buildImage {
              inherit (cargo.toml.package) name;
              tag = cargo.toml.package.version;
              copyToRoot = pkgs.buildEnv {
                name = "${cargo.toml.package.name}-env";
                paths = [bin];
              };
              config.Cmd = [cargo.toml.package.name];
              config.Env = ["PATH=${bin}/bin"];
            };

          devRust = fenix.packages.${system}.fromToolchainFile {
            file = "${self}/rust-toolchain.toml";
            sha256 = "sha256-Et8XFyXhlf5OyVqJkxrmkxv44NRN54uU2CLUTZKUjtM=";
          };
          devShell = pkgs.mkShell {
            buildInputs = [
              pkgs.openssl

              devRust

              enarx.packages.${system}.enarx
            ];

            nativeBuildInputs = with pkgs; [
              pkg-config
            ];
          };
        in {
          formatter = pkgs.alejandra;

          devShells.default = devShell;

          packages."${cargo.toml.package.name}" = nativeBin;
          packages."${cargo.toml.package.name}-static" = staticBin;
          packages."${cargo.toml.package.name}-static-oci" = buildImage staticBin;

          packages."${cargo.toml.package.name}-debug" = nativeDebugBin;
          packages."${cargo.toml.package.name}-debug-static" = staticDebugBin;
          packages."${cargo.toml.package.name}-debug-static-oci" = buildImage staticDebugBin;

          packages.default = nativeBin;
        }
      );
}
