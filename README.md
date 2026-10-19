# ldk-telegram

A Telegram bot for [LDK Server](https://github.com/lightningdevkit/ldk-server), modeled on the
`bos telegram` command from [Balance of Satoshis](https://github.com/alexbosworth/balanceofsatoshis).

It posts node activity to a Telegram chat and answers read-only commands about the node.

## Notifications

From ldk-server's event stream:

- 💰 Payments received and ⚡ payments sent, with fees
- ❌ Failed payments, with the reason
- ⏳ Payments waiting to be claimed (`Bolt11ReceiveForHash` and similar)
- 💸 Forwards, collected into a digest every few minutes
- ⏳ Channels opening, 🌹 opened, ⚠️ failed to open, 🥀 closed, and 💥 force-closed, with the
  reason
- 🔀 Splices negotiated or failed
- 🤖 A message when the bot starts, and an alert if ldk-server stays unreachable

The event stream does not replay events. Anything that happens while the bot is disconnected
is not posted.

## Commands

| Command | Description |
|---------|-------------|
| `/balance` | Lightning, on-chain, reserve, and sweeping balances |
| `/info` | Alias, public key, block height, channel and peer counts, sync times, version |
| `/liquidity [peer]` | Inbound and outbound liquidity, in total or with a peer |
| `/pending` | Opening and closing channels, in-flight HTLCs, claimable and sweeping funds |
| `/earnings` | Forwarding fees over the past day and week |
| `/costs` | Fees paid over the past day and week |
| `/graph <pubkey or alias>` | A node's graph info and our channels with it |
| `/connect` | Show the chat ID to put in the config |
| `/version` | Bot and ldk-server versions |
| `/stop` | Stop the bot |

Peers can be given by alias (substring, case-insensitive) or by the start of their public key.

Only the configured chat can use commands. Other chats can only use `/connect`.

`/earnings` gives weekly totals only when ldk-server runs with
`forwarded_payment_tracking_mode = "detailed"`. In the default `stats` mode it shows lifetime
totals.

## Setup

1. Create a bot with [@BotFather](https://t.me/botfather) and save its token to a file.
2. Make a read-only macaroon for the bot and save it to a file. See
   [Creating a macaroon](#creating-a-macaroon).
3. Copy [`config.example.toml`](config.example.toml) and fill it in. Set `chat_id = 0` for now.
4. Start the bot and send it `/connect`. Put the chat ID it replies with in the config, then
   restart the bot.

```sh
ldk-telegram --config /etc/ldk-telegram/config.toml
```

The config path can also come from `LDK_TELEGRAM_CONFIG`. Log verbosity follows `RUST_LOG`
(default `info`).

## Creating a macaroon

The bot only reads from ldk-server, so don't give it the admin macaroon. It needs these
permissions:

| Permission | Used for |
|------------|----------|
| `events:read` | The event stream that drives notifications |
| `node:read` | `/info`, `/balance`, `/pending`, `/version` |
| `channels:read` | `/liquidity`, `/pending`, `/graph`, channel details in notifications |
| `payments:read` | `/earnings`, `/costs` |
| `peers:read` | `/info`, `/graph`, peer lookup by alias |
| `graph:read` | Peer aliases in messages, `/graph` |

### Recommended: a separate macaroon from the server

This makes a macaroon with its own ID, so you can revoke it later without affecting other
clients:

```sh
ldk-server-cli create-macaroon telegram \
  -p events:read node:read channels:read payments:read peers:read graph:read \
  | jq -r .token > telegram.macaroon
```

`--preset readonly` also works. It grants two more permissions than the bot uses
(`messages:verify` and `utilities:read`).

To revoke it later, find its ID with `ldk-server-cli list-macaroons` and run
`ldk-server-cli revoke-macaroon <id>`.

### Offline: restrict a copy of an existing macaroon

You can also add caveats to a macaroon you already have, without contacting the server. This
works even when ldk-server is down:

```sh
ldk-server-cli derive-macaroon "$(cat admin.macaroon)" \
  --caveat 'permissions = events:read,node:read,channels:read,payments:read,peers:read,graph:read' \
  > telegram.macaroon
```

A derived copy shares the original macaroon's ID. You can't revoke it on its own: revoking it
also revokes the original. Use `create-macaroon` unless you need to work offline.

### Check it

```sh
ldk-server-cli -m "$(cat telegram.macaroon)" get-permissions
```

The output should list exactly the six permissions above.

## Running under systemd

[`contrib/ldk-telegram.service`](contrib/ldk-telegram.service) is a hardened unit:

- `Type=notify`: the bot reports ready once it reaches Telegram. `systemctl status` shows
  whether it is connected to ldk-server.
- `WatchdogSec=60`: the bot pings systemd's watchdog.
- `Restart=on-failure`: startup errors are retried. `SIGTERM` and `/stop` exit cleanly and
  are not restarted.
- Shutdown is bounded to 8 seconds, which is less than Docker's 10-second default. The bot stops
  taking commands and lets running ones finish. It handles the event it is processing, posts any
  forwards collected for the digest, and tells Telegram which messages it handled. A second
  signal exits immediately.
- `DynamicUser=yes` with `LoadCredential=`: the bot token (`bot-token`), macaroon (`macaroon`),
  and ldk-server's TLS certificate (`tls-cert`) are passed as credentials. The bot needs no
  access to ldk-server's data directory.
- When logging to the journal, the bot leaves out timestamps and colors.

## NixOS

```nix
{
  inputs.ldk-telegram.url = "github:benthecarman/ldk-telegram";
  # Optional: build with your system's nixpkgs (nixos-26.05 or newer).
  inputs.ldk-telegram.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { nixpkgs, ldk-telegram, ... }: {
    nixosConfigurations.node = nixpkgs.lib.nixosSystem {
      modules = [
        ldk-telegram.nixosModules.default
        {
          services.ldk-telegram = {
            enable = true;
            botTokenFile = "/run/secrets/ldk-telegram-token";
            macaroonFile = "/run/secrets/ldk-telegram-macaroon";
            tlsCertFile = "/var/lib/ldk-server/tls.crt";
            settings = {
              telegram.chat_id = 123456789;
              ldk_server.address = "127.0.0.1:3536";
              notifications.min_forward_amount_sats = 1000;
            };
          };
        }
      ];
    };
  };
}
```

`settings` accepts everything in `config.example.toml`.

`botTokenFile`, `macaroonFile`, and `tlsCertFile` are required. They must be absolute paths
outside the Nix store, such as files managed by agenix or sops-nix. systemd reads them as root
and passes them to the service as credentials, so they can stay root-only and the certificate
can stay in ldk-server's data directory.

The service starts after `ldk-server.service`. If your ldk-server unit has another name, set
`ldkServerUnit`.

## Docker

```sh
docker build -t ldk-telegram .
docker run -d --restart on-failure --network host \
  -v /etc/ldk-telegram:/config:ro \
  -v /var/lib/ldk-server/tls.crt:/config/tls.crt:ro \
  ldk-telegram
```

The image reads `/config/ldk-telegram.toml`. Set `cert_path`, `macaroon_path`, and
`bot_token_file` to paths under `/config`, or pass the token in `LDK_TELEGRAM_BOT_TOKEN`.

## License

Licensed under [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option. See
[LICENSE.md](LICENSE.md).
