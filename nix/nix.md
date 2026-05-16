# Nix

sld includes a Nix flake, an overlay, and a derivation for building sld from
this repository.

There are two ways of using an unstable sld, one is with Nix Flakes. Note that
until NixOS 25.11 is branched, unstable Nixpkgs is required.

```nix
{
  inputs = {
    # Have Nixpkgs
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # Include sld
    sld = {
      url = "path:/path/to/sld";
      # If using the sld Flake (not required)
      # inputs.nixpkgs.follows = "nixpkgs";
      #
      # If not using the sld flake, and just using the overlay
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      sld,
    }:
    let
      # Create an instance of Nixpkgs targeting x64 Linux with the
      # sld overlay applied
      pkgs = import nixpkgs {
        system = "x86_64-linux";
        overlays = [
          (import sld)
        ];
      };

      # Create a stdenv that uses the sld linker
      sldStdenv = pkgs.useWildLinker pkgs.stdenv;
    in
    {
      # Add an output of some very cool package that is linked with the sld linker
      #
      # Note that if a Rust package is being linked with `buildRustPackage`, you will
      # need to create a `rustPlatform` using `makeRustPlatform` with this stdenv. See
      # below how to do that.
      packages.x86_64-linux.default = pkgs.callPackage ./package.nix { stdenv = sldStdenv; };

      # A devShell for the very cool package that uses sld.
      #
      # It also has rust-analyzer in its environment
      devShell.x86_64-linux.default = pkgs.mkShell.override { stdenv = sldStdenv; } {
        inputsFrom = [ self.packages.x86_64-linux.default ];
        packages = [
          pkgs.rust-analyzer
        ];
      };
    };
}
```
Without flakes (npins shown, but any solution can be used):

Add the dependencies to your lockfile with npins or another pinning tool before using the example
below.

```nix
let
  sources = import ./npins;
  pkgs = import sources.nixpkgs {
    overlays = [
      (import sources.sld)
    ];
  };
  sldStdenv = pkgs.useWildLinker pkgs.stdenv;
in
{
  # C Package
  package = pkgs.callPackage ./package.nix { stdenv = sldStdenv; };
}
```
If building a Rust package with `rustPlatform.buildRustPackage`, a little more
setup is required. This applies to Flake-based packages, or other solutions.

```nix
let
  # First steps are the same as above. Create a Nixpkgs instance
  # with sld.
  pkgs = import nixpkgs {
    system = "x86_64-linux";
    overlays = [
      (import sld)
    ];
  };

  # Create a stdenv that uses sld as its linker
  sldStdenv = pkgs.useWildLinker pkgs.stdenv;

  # Next a custom rustPlatform is required.
  #
  # This uses Nixpkgs rustc and cargo, but uses
  # the stdenv that has sld.
  sldRustPlatform = pkgs.makeRustPlatform {
    inherit (pkgs) rustc cargo;
    stdenv = sldStdenv;
  };
in
# Then create whatever cool package you are building
callPackage ./package.nix { rustPlatform = sldRustPlatform; }
```
