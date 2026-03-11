# rude.nix — Home Manager module
# Drop into your ~/.nixos-btw/modules/rude.nix and import it.
#
# Usage:
#   services.rude = {
#     enable = true;
#     settings = {
#       max_concurrent_downloads = 5;
#       download_dir = "/home/${config.home.username}/Downloads";
#       rpc.port = 16800;
#     };
#   };

{ config, lib, pkgs, ... }:

let
  cfg = config.services.rude;
  settingsFormat = pkgs.formats.toml { };
  configFile = settingsFormat.generate "rude-config.toml" cfg.settings;
in
{
  options.services.rude = {
    enable = lib.mkEnableOption "rude download daemon";

    package = lib.mkOption {
      type = lib.types.package;
      # Replace with your flake input once published:
      # default = pkgs.rude;
      description = "The rude package to use.";
    };

    settings = lib.mkOption {
      type = lib.types.submodule {
        freeformType = settingsFormat.type;
        options = {
          max_concurrent_downloads = lib.mkOption {
            type = lib.types.int;
            default = 5;
          };
          max_connections_per_server = lib.mkOption {
            type = lib.types.int;
            default = 8;
          };
          split = lib.mkOption {
            type = lib.types.int;
            default = 8;
          };
          adaptive_workers = lib.mkOption {
            type = lib.types.bool;
            default = true;
          };
          work_stealing = lib.mkOption {
            type = lib.types.bool;
            default = true;
          };
          download_dir = lib.mkOption {
            type = lib.types.str;
            default = "${config.home.homeDirectory}/Downloads";
          };
          save_session_interval = lib.mkOption {
            type = lib.types.int;
            default = 60;
          };
          rpc = {
            enabled = lib.mkOption { type = lib.types.bool; default = true; };
            host = lib.mkOption { type = lib.types.str; default = "127.0.0.1"; };
            port = lib.mkOption { type = lib.types.int; default = 16800; };
            secret = lib.mkOption {
              type = lib.types.nullOr lib.types.str;
              default = null;
            };
          };
        };
      };
      default = { };
    };
  };

  config = lib.mkIf cfg.enable {
    xdg.configFile."rude/config.toml".source = configFile;

    systemd.user.services.rude = {
      Unit = {
        Description = "Rude download daemon";
        After = [ "network-online.target" ];
        Wants = [ "network-online.target" ];
      };
      Service = {
        ExecStart = "${cfg.package}/bin/rude daemon";
        Restart = "on-failure";
        RestartSec = "5s";
        # Harden the service
        PrivateTmp = true;
        NoNewPrivileges = true;
      };
      Install = {
        WantedBy = [ "default.target" ];
      };
    };
  };
}
