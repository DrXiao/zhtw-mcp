{
  description = "Nix package for zhtw-mcp";

  inputs = {
    nixpkgs.url = "https://flakehub.com/f/NixOS/nixpkgs/0.1";
    fenix = {
      url = "https://flakehub.com/f/nix-community/fenix/0.1";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
    }:

    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forEachSupportedSystem =
        f:
        nixpkgs.lib.genAttrs supportedSystems (
          system:
          f {
            inherit system;
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ self.overlays.default ];
            };
          }
        );
    in
    {
      overlays.default = final: prev: {
        rustToolchain =
          with fenix.packages.${final.stdenv.hostPlatform.system};
          combine (
            with stable;
            [
              cargo
              clippy
              rust-src
              rustc
              rustfmt
            ]
          );

        zhtw-mcp =
          let
            cargoToml = fromTOML (builtins.readFile ./Cargo.toml);
            # Same pin gen-s2t-tables.py reads, so a bump touches only Cargo.toml.
            opencc = cargoToml.package.metadata.opencc;
            # shallow = true is load-bearing, not a bandwidth tweak: without
            # it fetchGit resolves rev against the default branch, so a commit
            # that is not an ancestor of master fails to fetch.
            opencc-src = builtins.fetchGit {
              url = opencc.repository;
              rev = opencc.commit;
              shallow = true;
            };
            # Where gen-s2t-tables.py looks before downloading.
            dictDir = "data/opencc/${builtins.substring 0 12 opencc.commit}";
            rustPlatform = final.makeRustPlatform {
              cargo = final.rustToolchain;
              rustc = final.rustToolchain;
            };
          in
          rustPlatform.buildRustPackage {
            pname = "zhtw-mcp";
            inherit (cargoToml.package) version;

            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = [
              final.python3
              final.rustToolchain
            ];

            preBuild = ''
              mkdir -p ${dictDir}
              for dict in STPhrases STCharacters TWVariants; do
                cp ${opencc-src}/data/dictionary/$dict.txt ${dictDir}/$dict.txt
              done
              python3 scripts/gen-s2t-tables.py
              rustfmt src/engine/s2t_data.rs
            '';

            cargoTestFlags = [
              "--lib"
              "--bins"
            ];

            meta = with final.lib; {
              description = "MCP server for Traditional Chinese (zh-TW) text linting and normalization";
              homepage = "https://github.com/sysprog21/zhtw-mcp";
              license = licenses.mit;
              mainProgram = "zhtw-mcp";
            };
          };
      };

      packages = forEachSupportedSystem (
        { pkgs, ... }:
        {
          inherit (pkgs) zhtw-mcp;
          default = pkgs.zhtw-mcp;
        }
      );

      devShells = forEachSupportedSystem (
        { pkgs, system }:
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              rustToolchain
              openssl
              pkg-config
              python3
              self.formatter.${system}
            ];
          };
        }
      );

      formatter = forEachSupportedSystem ({ pkgs, ... }: pkgs.nixfmt);
    };
}
