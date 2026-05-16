{
  inputs = {
    nixpkgs.url = "https://nixos.org/channels/nixos-unstable/nixexprs.tar.xz";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      # Generate an output for each flake-exposed system. Flakes suck.
      forAllSystems = nixpkgs.lib.genAttrs nixpkgs.lib.systems.flakeExposed;

      # Make an attribute-set that instances Nixpkgs with our overlay for each
      # system
      common = forAllSystems (system: {
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            (import self)
          ];
        };
      });
    in
    {
      formatter = forAllSystems (system: common.${system}.pkgs.nixfmt-tree);

      # Route all uses through here so we are
      # testing it the way most users will use the derivation
      # Which is `import sld`
      overlays.default = import self;

      # Output sld as a stand-alone package.
      packages = forAllSystems (system: {
        default = common.${system}.pkgs.sld-unwrapped;
      });

      # Tests to ensure sld continues working on Nixos
      # We run unit tests, and some smoke tests that are in Nixpkgs.
      checks = forAllSystems (
        system:
        let
          inherit (common.${system}) pkgs;
        in
        {
          # Tests in Nixpkgs to run
          inherit (pkgs.callPackage "${nixpkgs}/pkgs/by-name/wi/wild-unwrapped/adapterTest.nix" { })
            adapterGcc
            adapter-llvm
            ;

          # Use the crane-cached build artifacts to speed up building the unit tests.
          sld = pkgs.sld-unwrapped.overrideAttrs (old: {
            stdenv = p: p.stdenvNoCC;

            doCheck = true;
            doInstallCheck = false;
            # Skip the build phase and don't install anything
            # because it ends up building libsld twice. Once for the buildPhase,
            # once for the checkPhase.
            dontBuild = true;
            installPhase = "touch $out";
          });
        }
      );

      # devShell for developing sld
      devShells = forAllSystems (system: {
        default = common.${system}.pkgs.callPackage ./nix/shell.nix { };
      });
    };
}
