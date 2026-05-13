{
  description = "Loro dev environment (Ordovicia mergeable-containers work)";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1";
    flake-parts.url = "https://flakehub.com/f/hercules-ci/flake-parts/*";

    fenix = {
      url = "https://flakehub.com/f/nix-community/fenix/0.1";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {
    flake-parts,
    nixpkgs,
    fenix,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin"];

      perSystem = {
        pkgs,
        lib,
        system,
        ...
      }: let
        fenixPkgs = fenix.packages.${system};

        # Loro's rust-toolchain says "stable". Pull stable from fenix with the
        # components and targets we need for the full dev loop:
        #   - rust-src: required for some IDE tooling and certain proc-macro paths
        #   - clippy / rustfmt: matches `pnpm check` / `pnpm fix`
        #   - rust-analyzer: editor LSP
        #   - wasm32-unknown-unknown: required for the loro-wasm crate
        rustToolchain = fenixPkgs.combine [
          fenixPkgs.stable.rustc
          fenixPkgs.stable.cargo
          fenixPkgs.stable.clippy
          fenixPkgs.stable.rustfmt
          fenixPkgs.stable.rust-src
          fenixPkgs.stable.rust-analyzer
          fenixPkgs.targets.wasm32-unknown-unknown.stable.rust-std
        ];

        # wasm-snip is not in nixpkgs. CONTRIBUTING.md lists it as a required
        # tool, and past experience says we'll need it for the WASM release
        # pipeline. Build it from crates.io. The package hasn't been updated
        # since 2019 — pinning 0.4.0 is fine.
        #
        # The crate ships with a Cargo.lock v1 format (pre-checksums), which
        # modern Nix vendoring can't handle. Regenerate the lockfile in
        # postPatch via `cargo generate-lockfile` so the vendor tool gets a
        # v3 lockfile with checksums. cargoHash is `lib.fakeHash` on first
        # build — replace with the value Nix reports.
        wasm-snip = pkgs.rustPlatform.buildRustPackage rec {
          pname = "wasm-snip";
          version = "0.4.0";

          src = pkgs.fetchCrate {
            inherit pname version;
            hash = "sha256-+oThqcy3H4/s2T+Uw0V/nnVzx7SW5xPghbaJxoub7yc=";
          };

          # The published crate ships a Cargo.lock v1 (pre-checksums) that
          # modern Nix vendoring can't parse. Replace it with a v4 lockfile
          # generated locally via `cargo generate-lockfile`. Lives in this
          # repo as `wasm-snip-Cargo.lock`.
          cargoLock = {
            lockFile = ./wasm-snip-Cargo.lock;
          };

          postPatch = ''
            ln -sf ${./wasm-snip-Cargo.lock} Cargo.lock
          '';

          doCheck = false;

          meta = {
            description = "A tool to selectively remove functions from a `.wasm` binary";
            homepage = "https://github.com/rustwasm/wasm-snip";
            license = with lib.licenses; [asl20 mit];
            mainProgram = "wasm-snip";
          };
        };
      in {
        devShells.default = pkgs.mkShell {
          packages = [
            # --- Rust toolchain (via fenix) ---
            rustToolchain

            # --- Rust dev tooling pulled from nixpkgs ---
            pkgs.cargo-nextest # `pnpm test` invokes this
            pkgs.cargo-hack # `pnpm check-all`
            pkgs.cargo-llvm-cov # `pnpm coverage`
            pkgs.cargo-fuzz # `pnpm run-fuzz-corpus`
            pkgs.cargo-vet # `pnpm vet`

            # --- WASM build pipeline ---
            # Loro pins wasm-bindgen = "=0.2.100" in Cargo.toml; the CLI must
            # match the library exactly or generated bindings will fail at
            # build time. nixpkgs ships per-version packages — use the 0.2.100
            # one directly rather than overriding the unversioned attribute.
            pkgs.wasm-bindgen-cli_0_2_100
            pkgs.binaryen # provides `wasm-opt`
            wasm-snip # custom derivation; see above

            # --- JS toolchain for WASM tests & build scripts ---
            pkgs.deno # `pnpm release-wasm`, build scripts in crates/loro-wasm
            pkgs.nodejs_22 # vitest runner, npm
            pkgs.pnpm # workspace package manager
            pkgs.bun # `cd bun_tests && bun test`

            # --- Misc ---
            pkgs.git
            pkgs.pkg-config
          ];

          shellHook = ''
            echo "[loro-dev] Rust $(rustc --version | cut -d' ' -f2) (target: wasm32-unknown-unknown available)"
            echo "[loro-dev] cargo-nextest $(cargo-nextest --version 2>/dev/null | head -1 || echo '?')"
            echo "[loro-dev] wasm-bindgen $(wasm-bindgen --version 2>/dev/null | cut -d' ' -f2 || echo '?') (Loro pins =0.2.100; matched)"
            echo "[loro-dev] wasm-snip $(wasm-snip --version 2>/dev/null | cut -d' ' -f2 || echo '?')"
            echo "[loro-dev] deno $(deno --version 2>/dev/null | head -1 | cut -d' ' -f2), node $(node --version 2>/dev/null), pnpm $(pnpm --version 2>/dev/null), bun $(bun --version 2>/dev/null)"
            echo ""
            echo "[loro-dev] Common commands:"
            echo "  cargo test --workspace                 # Phase A baseline (no nextest)"
            echo "  pnpm test                              # Full test suite (nextest + doctests)"
            echo "  pnpm check                             # cargo clippy --all-features -- -Dwarnings"
            echo "  pnpm test-wasm                         # WASM build + test (later phases)"
          '';
        };
      };
    };
}
