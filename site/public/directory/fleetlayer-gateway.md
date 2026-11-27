# fleetlayer-gateway

One address per service, a status page that alerts, and the same routes as Traefik and Gatus config.

| | |
|---|---|
| Kind | Service |
| Status | beta |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/crates/fleetlayer-gateway |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-gateway` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.3.8 and later |

## What it does

On a laptop it gives each service an address (`http://pages.localhost:7680`), checks each one's health every 30
seconds, takes Gatus-style heartbeats, and tells you and an agent when something stops. On a server it writes
Traefik and Gatus config from the same list.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

```yaml
# Started by `fleetlayer up` on a laptop; its settings are in ~/.config/fleetlayer/gateway.yaml.
# On a server: fleetlayer-gateway emit traefik … and fleetlayer-gateway emit gatus …
```

## What it can read and do

Listens on 127.0.0.1 and [::1] only by default, in plain HTTP for *.localhost addresses. Exposing it to a network (listen 0.0.0.0) exposes every service it routes, without TLS.

More: https://fleetlayer.ai/guides/laptop.md.
