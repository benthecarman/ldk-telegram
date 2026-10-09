self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.ldk-telegram;
  format = pkgs.formats.toml { };
  configFile = format.generate "ldk-telegram.toml" cfg.settings;

  # Secrets must stay out of the world-readable Nix store, so only accept absolute paths
  # outside of it. systemd reads them as root and hands them to the service as credentials.
  secretPath = lib.types.pathWith { inStore = false; absolute = true; };

  credentials = {
    bot-token = cfg.botTokenFile;
    macaroon = cfg.macaroonFile;
    tls-cert = cfg.tlsCertFile;
  };
in
{
  options.services.ldk-telegram = {
    enable = lib.mkEnableOption "the LDK Server Telegram bot";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "ldk-telegram.packages.\${system}.default";
      description = "The ldk-telegram package to run.";
    };

    botTokenFile = lib.mkOption {
      type = secretPath;
      example = "/run/secrets/ldk-telegram-token";
      description = ''
        File containing the Telegram bot token from @BotFather. It is passed to the service
        as a systemd credential and never copied to the Nix store.
      '';
    };

    macaroonFile = lib.mkOption {
      type = secretPath;
      example = "/run/secrets/ldk-telegram-macaroon";
      description = ''
        File containing the hex macaroon the bot uses with ldk-server. A read-only macaroon
        (node:read channels:read payments:read peers:read graph:read events:read) is enough.
      '';
    };

    tlsCertFile = lib.mkOption {
      type = secretPath;
      example = "/var/lib/ldk-server/tls.crt";
      description = ''
        ldk-server's TLS certificate (PEM). It can stay in ldk-server's data directory; the bot
        doesn't need permission to read it.
      '';
    };

    ldkServerUnit = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = "ldk-server.service";
      description = ''
        The systemd unit running ldk-server, so the bot starts after it. The bot keeps retrying
        while ldk-server is down, so this only affects the order things start in.
      '';
    };

    settings = lib.mkOption {
      type = lib.types.submodule {
        freeformType = format.type;
        options.telegram.chat_id = lib.mkOption {
          type = lib.types.int;
          description = "The chat to post to and accept commands from. Send /connect to the bot to find it.";
        };
        options.ldk_server.address = lib.mkOption {
          type = lib.types.str;
          default = "127.0.0.1:3536";
          description = "gRPC address of ldk-server.";
        };
      };
      default = { };
      example = lib.literalExpression ''
        {
          telegram.chat_id = 123456789;
          notifications = {
            forward_digest_interval_secs = 600;
            min_forward_amount_sats = 1000;
          };
        }
      '';
      description = ''
        Contents of the bot's config file. See config.example.toml for every option. Don't put
        secrets here; use the *File options instead.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.ldk-telegram = {
      description = "Telegram bot for LDK Server";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ] ++ lib.optional (cfg.ldkServerUnit != null) cfg.ldkServerUnit;

      serviceConfig = {
        Type = "notify";
        NotifyAccess = "main";
        WatchdogSec = 60;
        ExecStart = "${lib.getExe cfg.package} --config ${configFile}";
        Restart = "on-failure";
        RestartSec = 10;
        # The bot gives itself 8 seconds to stop cleanly, so this is only a backstop.
        TimeoutStopSec = 15;
        LoadCredential = lib.mapAttrsToList (name: path: "${name}:${path}") credentials;

        DynamicUser = true;
        UMask = "0077";
        CapabilityBoundingSet = "";
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged @resources" ];
      };
    };
  };
}
