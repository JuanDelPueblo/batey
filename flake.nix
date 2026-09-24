{
  description = "Batey: persistent ACP project and chat supervisor";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.crane.url = "github:ipetkov/crane";
  outputs = { self, nixpkgs, crane }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
    in {
      packages = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };
          batey = import ./nix/package.nix { inherit pkgs crane; };
          batey-oci = import ./nix/oci.nix { inherit pkgs batey; };
          oci-foreign-elf-fixture = import ./nix/foreign-elf-fixture.nix { inherit pkgs; };
        in {
          inherit batey batey-oci oci-foreign-elf-fixture;
          default = batey;
        });

      devShells = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.mkShell {
            name = "batey-dev";

            packages = with pkgs; [
              # Rust toolchain
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer

              # Build speed
              mold
              sccache
              cargo-nextest
              cargo-watch

              # Frontend and fake backend
              nodejs_22

              # Integration tests and tooling
              python3
              git
              sqlite
              direnv
              binutils
            ];

            # The bundled SQLite of rusqlite compiles C sources.
            nativeBuildInputs = [ pkgs.pkg-config ];

            shellHook = ''
              # Link with mold. mold is much faster than the default linker.
              # The variable is target-scoped, so it does not replace the
              # rustflags of Cargo.toml or of a later cargo invocation.
              host_triple="$(rustc -vV | sed -n 's/^host: //p')"
              flag_var="CARGO_TARGET_$(echo "$host_triple" | tr 'a-z.-' 'A-Z__')_RUSTFLAGS"
              export "$flag_var=-C link-arg=-fuse-ld=mold"

              # Keep build output out of the Nix store and out of the source tree.
              export CARGO_TARGET_DIR="''${CARGO_TARGET_DIR:-$PWD/target}"

              # sccache caches the compilation of dependencies between checkouts.
              # It turns off incremental compilation, so it stays opt-in.
              # Run `export RUSTC_WRAPPER=sccache` to enable it.

              export NG_CLI_ANALYTICS=false
              export RUST_BACKTRACE=1

              echo "batey dev shell: $(rustc --version), node $(node --version)"
              echo "  cargo check       fast type check"
              echo "  cargo nextest run fast test run"
              echo "  npm run dev       frontend against the fake backend (in frontend/)"
            '';
          };
        });

      apps = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };
          verify = pkgs.writeShellApplication {
            name = "batey-verify";
            # Every tool the verification suite needs, so `nix run .#verify`
            # works on a clean checkout without entering `nix develop`.
            runtimeInputs = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              pkg-config
              stdenv.cc
              cargo-nextest
              nodejs_22
              python3
              git
              sqlite
              direnv
            ];
            text = builtins.readFile ./nix/verify.sh;
          };
        in {
          verify = {
            type = "app";
            program = pkgs.lib.getExe verify;
            meta.description = "Run Batey's canonical source verification suite";
          };
        });

      nixosModules.default = { pkgs, ... }:
        {
          imports = [ ./nix/nixos-module.nix ];
          # Keep the service default tied to the exact package exposed by this
          # flake. Consumers do not need a package overlay just to import the
          # module, and the option remains overridable in the normal way.
          _module.args.bateyPackage = self.packages.${pkgs.system}.batey;
        };

      checks = eachSystem (system:
        let
          pkgs = import nixpkgs { inherit system; };
          batey = self.packages.${system}.batey;
          batey-oci = self.packages.${system}.batey-oci;
          module = self.nixosModules.default;
        in {
          eval-checks = import ./nix/eval-checks.nix { inherit pkgs batey module; };
          oci-config = import ./nix/oci-check.nix { inherit pkgs batey batey-oci; };
        });
    };
}
