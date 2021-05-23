{
  inputs = {
    utils.url = "github:numtide/flake-utils";
    naersk = {
      url = github:nmattia/naersk;
      inputs.nixpkgs.follows = "nixpkgs";
    };
    fenix = {
      url = github:nix-community/fenix;
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, utils, naersk, fenix }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages."${system}";
        toolchain = with fenix.packages.${system};
          combine [
            minimal.rustc
            minimal.cargo
            targets.x86_64-unknown-linux-musl.latest.rust-std
            targets.armv7-unknown-linux-musleabihf.latest.rust-std
          ];

        naersk-lib = naersk.lib.${system}.override {
          cargo = toolchain;
          rustc = toolchain;
        };

        buildJsonnetExporter = target: sdk: args: naersk-lib.buildPackage
          {
            src = ./.;

            nativeBuildInputs = [
              pkgs.pkgconfig
              sdk.stdenv.cc
              sdk.openssl
            ];

            CARGO_BUILD_TARGET = target;
            CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";

            TARGET_CC = with sdk.stdenv;
              "${cc}/bin/${cc.targetPrefix}gcc";

            "CARGO_TARGET_${builtins.replaceStrings [ "-" ] [ "_" ] (pkgs.lib.toUpper target)}_LINKER" = with sdk.stdenv;
              "${cc}/bin/${cc.targetPrefix}gcc";

            OPENSSL_STATIC = "1";
          } // args;

        buildDocker = { build }:
          pkgs.dockerTools.buildLayeredImage {
            name = "simonswine/jsonnet-exporter";
            # TODO: Find a way to determine from git tags
            tag = "0.1.0";
            contents = [
              pkgs.pkgsStatic.busybox
              pkgs.cacert
              build
            ];
            config = {
              Cmd = "jsonnet-exporter";
            };
          };

      in
      rec {
        packages.linux-amd64 =
          buildJsonnetExporter "x86_64-unknown-linux-musl" pkgs.pkgsStatic { };

        packages.linux-armv7 =
          buildJsonnetExporter "armv7-unknown-linux-musleabihf" pkgs.pkgsCross.muslpi { };

        defaultPackage = buildJsonnetExporter { target = "target"; };

        packages.dockerImage = buildDocker { build = packages.linux-amd64; };

        # `nix run`
        apps.my-project = utils.lib.mkApp {
          drv = packages.my-project;
        };
        defaultApp = apps.my-project;

        # `nix develop`
        devShell = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [ rustc cargo pkgconfig openssl lldb ];
        };
      });
}
