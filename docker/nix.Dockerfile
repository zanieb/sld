# Run inside a Nix shell.
#
# docker build --progress=plain -t sld-dev-nix . -f docker/nix.Dockerfile
#
# docker run -it sld-dev-nix

FROM nixos/nix AS chef

COPY docker/shell.nix shell.nix
RUN nix-shell --run "rustup toolchain install nightly"

WORKDIR /sld

FROM chef AS planner
COPY . .
COPY docker/shell.nix shell.nix
RUN nix-shell --run "cargo chef prepare --recipe-path recipe.json"

FROM chef AS builder
COPY --from=planner /sld/recipe.json recipe.json
COPY docker/shell.nix shell.nix
RUN nix-shell --run "cargo chef cook --all-targets --recipe-path recipe.json"
COPY . .

ENTRYPOINT ["nix-shell"]
