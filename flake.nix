{
  description = "Ratty: a GPU-rendered terminal emulator with inline 3D graphics";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    { self, nixpkgs, crane }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forEachSystem = nixpkgs.lib.genAttrs supportedSystems;

      # Shared module options — used by both HM and NixOS modules
      rattyOptions =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.ratty;
          tomlFormat = pkgs.formats.toml { };
        in
        {
          options.programs.ratty = {
            enable = lib.mkEnableOption "Ratty, a GPU-rendered terminal emulator";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.ratty;
              defaultText = lib.literalExpression "self.packages.\${pkgs.stdenv.hostPlatform.system}.ratty";
              description = "The ratty package to install.";
            };

            settings = lib.mkOption {
              type = tomlFormat.type;
              default = { };
              description = "Ratty configuration (ratty.toml).";
              example = lib.literalExpression ''
                {
                  window = {
                    opacity = 0.8;
                  };
                  shell = {
                    program = "bash";
                  };
                }
              '';
            };
          };
        };
    in
    {
      packages = forEachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          craneLib = crane.mkLib pkgs;
        in
        {
          ratty = pkgs.callPackage ./nix/default.nix {
            inherit craneLib;
            # Pass Darwin frameworks when on Darwin
            darwinFrameworks = pkgs.lib.optionals pkgs.stdenv.isDarwin (
              with pkgs.darwin.apple_sdk.frameworks;
              [
                Cocoa
                CoreFoundation
                CoreGraphics
                CoreText
                CoreVideo
                Metal
                QuartzCore
              ]
            );
          };
          default = self.packages.${system}.ratty;
        }
      );

      devShells = forEachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
            packages = with pkgs; [
              rust-analyzer
              cargo
              clippy
              rustfmt
            ];
          };
        }
      );

      formatter = forEachSystem (system: nixpkgs.legacyPackages.${system}.nixfmt-rfc-style);

      checks = forEachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          # Build + run tests (cargo test via cargoCheckHook)
          ratty = self.packages.${system}.ratty;
        }
      );

      # Home Manager module — declarative user-level config
      #
      # Usage in home.nix:
      #   programs.ratty = {
      #     enable = true;
      #     settings = {
      #       window = { opacity = 0.9; };
      #       shell = { program = "zsh"; };
      #     };
      #   };
      #
      # Config is written to $XDG_CONFIG_HOME/ratty/ratty.toml
      # ratty discovers this path automatically.
      homeManagerModules.default =
        args@{
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.ratty;
          tomlFormat = pkgs.formats.toml { };
          opts = rattyOptions args;
        in
        {
          inherit (opts) options;
          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];
            xdg.configFile."ratty/ratty.toml" = lib.mkIf (cfg.settings != { }) {
              source = tomlFormat.generate "ratty.toml" cfg.settings;
            };
          };
        };

      # NixOS module — declarative system-level config
      #
      # Usage in configuration.nix:
      #   programs.ratty = {
      #     enable = true;
      #     settings = {
      #       window = { opacity = 0.9; };
      #       shell = { program = "zsh"; };
      #     };
      #   };
      #
      # Config is written to /etc/ratty/ratty.toml
      # Binary is wrapped with --config-file flag to use system config.
      nixosModules.default =
        args@{
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.ratty;
          tomlFormat = pkgs.formats.toml { };
          opts = rattyOptions args;
        in
        {
          inherit (opts) options;
          config = lib.mkIf cfg.enable {
            environment.systemPackages = [
              (
                if cfg.settings == { } then
                  cfg.package
                else
                  pkgs.symlinkJoin {
                    name = "ratty-system-wrapped";
                    paths = [ cfg.package ];
                    nativeBuildInputs = [ pkgs.makeWrapper ];
                    postBuild = ''
                      rm -f $out/bin/ratty
                      makeWrapper ${cfg.package}/bin/ratty $out/bin/ratty \
                        --add-flags "--config-file /etc/ratty/ratty.toml"
                    '';
                  }
              )
            ];

            environment.etc."ratty/ratty.toml" = lib.mkIf (cfg.settings != { }) {
              source = tomlFormat.generate "ratty.toml" cfg.settings;
            };
          };
        };
    };
}
