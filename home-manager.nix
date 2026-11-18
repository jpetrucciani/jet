# home-manager module for jet and jetmux: their config files from Nix, and
# jetmux's daemon as a user service. `jetmux` is the package default.nix
# builds.
#
# Sections both programs read ([theme], [colors], [status]) can live in
# either file: jetmux layers jet's config (its [jetmux] table and the shared
# sections) under its own, which wins.
{ jetmux }:
{ config, lib, pkgs, ... }:
let
  toml = pkgs.formats.toml { };
  jet = config.programs.jet;
  mux = config.programs.jetmux;
in
{
  options.programs = {
    jet = {
      enable = lib.mkEnableOption "jet's config file";
      settings = lib.mkOption {
        inherit (toml) type;
        default = { };
        example = lib.literalExpression ''
          {
            font.size = 13;
            theme.name = "Poimandres";
            status.segments = [{ text = "{user}@{host}"; }];
            jetmux.prefix = "C-a";
          }
        '';
        description = "jet's config.toml; see docs/config.toml.";
      };
    };
    jetmux = {
      enable = lib.mkEnableOption "jetmux";
      package = lib.mkOption {
        type = lib.types.package;
        default = jetmux;
        defaultText = lib.literalMD "jetmux from this repository";
        description = "The jetmux package.";
      };
      settings = lib.mkOption {
        inherit (toml) type;
        default = { };
        example = lib.literalExpression ''
          {
            prefix = "C-a";
            persist.interval = 60;
            bar.style = "powerline";
          }
        '';
        description = "jetmux's config.toml; see docs/jetmux.toml.";
      };
      service = lib.mkEnableOption ''
        the daemon as a systemd user service. Without it, the first client
        starts the daemon'';
    };
  };

  config = lib.mkMerge [
    (lib.mkIf (jet.enable && jet.settings != { }) {
      xdg.configFile."jet/config.toml".source = toml.generate "jet-config.toml" jet.settings;
    })
    (lib.mkIf mux.enable {
      home.packages = [ mux.package ];
      xdg.configFile."jetmux/config.toml" = lib.mkIf (mux.settings != { }) {
        source = toml.generate "jetmux-config.toml" mux.settings;
      };
      systemd.user.services.jetmux = lib.mkIf mux.service {
        Unit.Description = "jetmux daemon";
        Service = {
          ExecStart = "${lib.getExe mux.package} daemon";
          # Hot upgrades exec in place, so the main PID stays the same.
          Restart = "on-failure";
        };
        Install.WantedBy = [ "default.target" ];
      };
    })
  ];
}
