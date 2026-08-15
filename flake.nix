{
  description = "Runtime-managed native systemd services from Nix flakes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    adios = {
      url = "github:adisbladis/adios";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      adios,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (system: rec {
        flakelet = nixpkgs.legacyPackages.${system}.callPackage ./default.nix { };
        default = flakelet;
      });

      nixosModules = {
        flakelet =
          { pkgs, lib, ... }:
          {
            imports = [
              ./modules/common.nix
              ./modules/nixos.nix
            ];
            services.flakelets = {
              package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.flakelet;
              # The flake input source is already a store path; pkgs.path may not be.
              nixpkgs = lib.mkDefault nixpkgs.outPath;
              adios = lib.mkDefault adios.outPath;
              flakeletLib = lib.mkDefault "${self}/lib";
            };
          };
        default = self.nixosModules.flakelet;
      };

      templates.default = {
        path = ./templates/service;
        description = "A flakelet service flake";
      };

      devShells = forAllSystems (system: {
        default =
          let
            pkgs = nixpkgs.legacyPackages.${system};
          in
          pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer
              nix-eval-jobs
            ];
          };
      });

      checks = forAllSystems (system: {
        package = self.packages.${system}.flakelet;
        lib = nixpkgs.legacyPackages.${system}.callPackage ./tests/lib.nix { adios = adios.outPath; };
        vm = nixpkgs.legacyPackages.${system}.testers.runNixOSTest (
          import ./tests/vm.nix { flakeletModule = self.nixosModules.flakelet; }
        );
      });
    };
}
