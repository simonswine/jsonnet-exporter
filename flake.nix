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
            #targets.x86_64-unknown-linux-musl.latest.rust-std
            targets.armv7-unknown-linux-musleabihf.latest.rust-std
          ];
        naersk-lib = naersk.lib.${system}.override {
          cargo = toolchain;
          rustc = toolchain;
        };
      in
      rec {
        # `nix build`
        packages.my-project = naersk-lib.buildPackage {
          src = ./.;

          #nativeBuildInputs = with pkgs; [ pkgsStatic.stdenv.cc ];
            nativeBuildInputs = with pkgs; [
            pkgsCross.muslpi.stdenv.cc
          ];
          #CARGO_BUILD_TARGET = "x86_64-unknown-linux-musl";
          CARGO_BUILD_TARGET = "armv7-unknown-linux-musleabihf";
          CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";


          CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER = with pkgs.pkgsCross.muslpi.stdenv;
            "${cc}/bin/${cc.targetPrefix}gcc";


          doCheck = false;
        };
        defaultPackage = packages.my-project;

        # `nix run`
        apps.my-project = utils.lib.mkApp {
          drv = packages.my-project;
        };
        defaultApp = apps.my-project;

        # `nix develop`
        devShell = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [ rustc cargo ];
        };
      });
}
