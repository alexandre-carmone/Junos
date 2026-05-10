{
  description = "junos-web — Ekos Live LAN relay + Leptos/WebGPU browser client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:

    # ── Per-system packages ─────────────────────────────────────────────────
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # Stable Rust toolchain with the wasm32-unknown-unknown target added.
        # Bump the channel by changing "latest" to e.g. "1.86.0".
        rustToolchainWasm = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
        };

        rustPlatformWasm = pkgs.makeRustPlatform {
          cargo = rustToolchainWasm;
          rustc = rustToolchainWasm;
        };

        # wasm-bindgen-cli must match the wasm-bindgen crate version in Cargo.lock.
        # Current required version: 0.2.118
        #
        # To fill in the hashes after changing this version:
        #   1. Set both hashes to pkgs.lib.fakeHash
        #   2. Run:  nix build .#junos-web-dist
        #      The build fails with "got: sha256-<HASH>" — paste that as wasmBindgenSrcHash
        #   3. Run again — it fails with a second hash for wasmBindgenCargoHash
        #   4. Run once more — it should now succeed
        wasmBindgenSrcHash   = "sha256-ve783oYH0TGv8Z8lIPdGjItzeLDQLOT5uv/jbFOlZpI=";
        wasmBindgenCargoHash = "sha256-EYDfuBlH3zmTxACBL+sjicRna84CvoesKSQVcYiG9P0=";

        wasmBindgenCli = pkgs.rustPlatform.buildRustPackage rec {
          pname = "wasm-bindgen-cli";
          version = "0.2.118";
          src = pkgs.fetchCrate {
            inherit pname version;
            hash = wasmBindgenSrcHash;
          };
          cargoHash = wasmBindgenCargoHash;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];
          doCheck = false;
        };

        # Exclude large reference-only subtrees and generated/runtime artifacts
        # from the Nix source hash so unrelated edits don't bust Rust rebuilds.
        filteredSrc = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: _type:
            let rel = pkgs.lib.removePrefix (toString ./.) (toString path);
            in !(pkgs.lib.hasPrefix "/kstars" rel)
            && !(pkgs.lib.hasPrefix "/target" rel)
            && !(pkgs.lib.hasPrefix "/junos-web/dist" rel)
            && !(pkgs.lib.hasPrefix "/junos-web/bin" rel)
            && !(rel == "/junos-web/styles/tailwind.css")
            && !(pkgs.lib.hasPrefix "/.certs" rel)
            && !(pkgs.lib.hasPrefix "/.git" rel)
            && !(rel == "/result");
        };

        # ── Stage 1: compile junos-web to a raw .wasm binary ──────────────
        #
        # buildRustPackage's configurePhase sets up vendored cargo deps;
        # our custom buildPhase runs on top of that — no network needed.
        junosWebRaw = rustPlatformWasm.buildRustPackage {
          pname = "junos-web-raw";
          version = "0.1.0";
          src = filteredSrc;
          cargoLock.lockFile = ./Cargo.lock;

          buildPhase = ''
            cargo build -p junos-web --target wasm32-unknown-unknown --release
          '';
          installPhase = ''
            mkdir -p $out
            cp target/wasm32-unknown-unknown/release/junos-web.wasm $out/
          '';

          doCheck = false;
        };

        # ── Stage 2: tailwind + wasm-bindgen + assets → dist directory ─────
        junosWebDist = pkgs.stdenv.mkDerivation {
          name = "junos-web-dist";
          src = ./junos-web;

          nativeBuildInputs = [
            wasmBindgenCli
            pkgs.python3
            pkgs.tailwindcss
          ];

          buildPhase = ''
            mkdir -p $out

            # Generate Tailwind utilities CSS (matches Trunk's pre_build hook).
            mkdir -p styles
            tailwindcss \
              --config tailwind.config.js \
              --input styles/tailwind.input.css \
              --output styles/tailwind.css \
              --minify

            # Generate JS bindings + processed WASM
            wasm-bindgen \
              --target web \
              --out-dir "$out" \
              ${junosWebRaw}/junos-web.wasm

            # Static catalog assets (checked-in binaries — copy as-is)
            cp -r public/. "$out/"

            # Stylesheets referenced by the rewritten <link rel="stylesheet"> tags
            mkdir -p "$out/styles"
            cp styles/tokens.css      "$out/styles/"
            cp styles/base.css        "$out/styles/"
            cp styles/tailwind.css    "$out/styles/"
            cp styles/responsive.css  "$out/styles/"

            # Strip Trunk directives from index.html and inject the init script
            python3 ${./nix/process-html.py} index.html "$out/index.html"
          '';

          dontInstall = true;
        };

        # ── Server binary ───────────────────────────────────────────────────
        #
        # Workspace default-members = ["junos-server"], so cargo build --release
        # builds only the server.  We override both phases for clarity.
        junosServer = pkgs.rustPlatform.buildRustPackage {
          pname = "junos-server";
          version = "0.1.0";
          src = filteredSrc;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];

          buildPhase = ''
            cargo build -p junos-server --release
          '';
          installPhase = ''
            mkdir -p $out/bin
            cp target/release/junos-server $out/bin/
          '';

          doCheck = false;
        };

        # ── Dev shell ───────────────────────────────────────────────────────
        devShell = pkgs.mkShell {
          packages = [
            rustToolchainWasm
            wasmBindgenCli
            pkgs.trunk
            pkgs.tailwindcss
            pkgs.pkg-config
            pkgs.openssl
            pkgs.python3
            pkgs.uv
            pkgs.just
            pkgs.cacert
          ];

          shellHook = ''
            export OPENSSL_NO_VENDOR=1
            if [ -e junos-web/bin/tailwindcss ]; then
              echo "warning: junos-web/bin/tailwindcss exists — the Nix shell ships its own \`tailwindcss\` on PATH."
              echo "         Trunk's pre_build hook still calls bin/tailwindcss; either delete it or keep both in sync."
            fi
          '';
        };

      in {
        packages = {
          inherit junosWebDist junosServer;

          # Default package: thin wrapper that bakes --dist-dir into the binary
          # so callers never have to pass it explicitly.
          default = pkgs.writeShellScriptBin "junos-server" ''
            exec ${junosServer}/bin/junos-server \
              --dist-dir ${junosWebDist} \
              "$@"
          '';
        };

        devShells.default = devShell;

        # Convenience: `nix run` starts the server. It binds HTTP on :8080
        # (KStars-facing) and HTTPS on :8443 (browser-facing, required by
        # iOS Safari for WebGPU). A self-signed cert is auto-generated into
        # ./.certs/ on first run.
        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/junos-server";
        };
      }
    )

    # ── System-independent outputs ──────────────────────────────────────────
    // {
      # Usage in the user's NixOS flake:
      #
      #   inputs.junos-web.url = "path:/path/to/junos-web";
      #   inputs.junos-web.inputs.nixpkgs.follows = "nixpkgs";
      #
      #   nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      #     modules = [
      #       junos-web.nixosModules.default
      #       { services.junos-web.enable = true; }
      #     ];
      #   };
      nixosModules.default = { config, lib, pkgs, ... }: {
        imports = [ ./nix/module.nix ];
        # Supply the package built for the current system as the default
        services.junos-web.package = lib.mkDefault self.packages.${pkgs.system}.default;
      };
    };
}
