# The tower's phone API (v1)

The Android app talks to the tower's voice daemon over the local network, and
over Tailscale while it runs on the Mac (from anywhere on your tailnet). It
can show everything the orb's panel shows, act on the tower the way the panel
does, and be the tower's voice: you speak into the phone, the Mac's Whisper
transcribes it, and the tower's replies come back as text and as Kokoro audio
played on the phone.

Since the Quiet Tower ([quiet-tower.md](quiet-tower.md)) it also has the
queue of decisions, the spaces as missions, the threads between their
sessions, launching and closing a space, how much the tower tells and the
model its brains run on: see "The Quiet Tower" below, and
[mobile-api-quiet-walkthrough.md](mobile-api-quiet-walkthrough.md) for every
one of those calls made with `curl` against a running tower, with its real
answers.

Source: `src/daemon/lan/` (server, auth, TLS, hub, audio; `quiet.rs` for the
Quiet Tower's calls, `tower.rs` for what they ask of the daemon),
`src/phone.rs` (`tower phone`). Tests: `cargo test lan` (with an HTTPS
client that pins the certificate the way the app must).

## Turning it on

The API is **off by default**.

```sh
tower phone enable      # [lan] enabled = true in config.toml, and starts it if the voice runs
tower phone pair        # a one-time code and a QR for the app, the network, and what it asks of the phone
tower phone devices     # paired phones (the networks each knows, whether it can reach the tower here), Tailscale; --json for all of it
tower phone network     # the network this Mac is on now; `network name "Office"` names it ("" forgets the name)
tower phone rename <id> "<name>"   # a paired phone's name
tower phone revoke <id> # or `all`: those tokens stop working at once
tower phone disable     # stops the API and ends open event streams (phones stay paired)
```

`config.toml`:

```toml
[lan]
enabled = false
port = 7843
bind = "auto"         # this Mac's address on its default-route network, or an IP of this Mac
allow_public = false  # refuse anything but private/link-local/100.64.0.0/10 (Tailscale) addresses
tailscale = "auto"    # also listen on this Mac's Tailscale IPv4 while Tailscale runs; "off" not to
bonjour = true        # be found by name on the local network (`_herdr-tower._tcp`); false not to
bonjour_name = ""     # the name it is found by; empty: this Mac's name (Sharing's Computer Name)

[lan.networks]        # names for the networks this Mac is on, by range (`tower phone network name`, Settings > Phone)
"10.10.30" = "Office"
"192.168.2" = "Home"
```

**A network** is told by this Mac's address on it, since macOS keeps the
Wi-Fi's name (SSID) from apps without location access: the /24 of the LAN
listener's IPv4 (`10.10.30` for 10.10.30.116), or the address itself for
IPv6. Named under `[lan.networks]`, it reads by its name ("Office");
unnamed, as "the network 10.10.30.x". (`HERDER_TOWER_LAN_IP_FILE` names a
file whose one line stands in for this Mac's address, read on every look:
a sandbox can move "networks" with it.)

**Tailscale.** With `tailscale = "auto"` (the default) the API listens on
two addresses, same port, same certificate: the LAN address as before, and
this Mac's tailnet IPv4 (100.64.0.0/10) while Tailscale is running. The
tower finds Tailscale through its CLI (`tailscale` on the PATH, the
`/usr/local/bin/tailscale` shim, or the standalone app's
`/Applications/Tailscale.app/Contents/MacOS/Tailscale`): `tailscale status
--json` for `BackendState`, the IPv4 and the MagicDNS name (`Self.DNSName`),
`tailscale ip -4` when the status has no address. It looks again every 20 s
while the API is on: when Tailscale starts, the tailnet address is listened
on within 20 s; when it stops, that listener closes (the LAN one never
does). The LAN address (`bind = "auto"`) is looked at on the same beat:
when the Mac is on another network, the LAN listener moves to the new
address on the same port within 20 s, and the pairing offers it; with no
network at all (Wi-Fi off) the old listener stays until there is one. A
phone paired on another network needs that address: pair it again, or use
Tailscale. Stopped, logged out or not installed: the LAN only, and `tower phone
devices` and the panel say so. With no LAN address (Wi-Fi off) but
Tailscale up, the tailnet address alone. A default route over the tailnet
(an exit node) is one address, the tailnet's (`kind: "tailscale"`). It never
listens on `0.0.0.0` or a public address (a `bind` like that is refused,
Tailscale or not).

The daemon's `state.json` (and `GET /v1/state`) has `lan` (the first
address, `https://192.168.1.20:7843`, or `""` when off), `lan_hosts` (every
address, in order: `[{"kind": "lan", "host": "192.168.1.20", "port": 7843},
{"kind": "tailscale", "host": "100.68.110.33", "port": 7843, "dns_name":
"mac.tailnet.ts.net"}]`, `[]` when off), `tailscale` (`{"installed": true,
"running": true, "state": "running", "ip": "100.68.110.33", "dns_name":
"mac.tailnet.ts.net"}`, `null` until the API has started once), `phone`
(the name of the phone that is the voice, or `null`), `network` (the LAN
listener's network: `{"range": "10.10.30", "name": "Office" | null, "lan":
{"host": "10.10.30.116", "port": 7843} | null}`, `null` while the API is
off or without a LAN address) and `phone_hint` (`"Phone: pair again on this
network"` while the Mac's windows say it, else `null`: see "Pairing on more
than one network").

## Security model

What it protects: the tower can type into every agent session on every
machine (and approve what they ask). Whoever can call this API can do that.
So:

1. **Off unless enabled**, and it listens **only on this Mac's private LAN
   address**, plus its **Tailscale address** (100.64.0.0/10) while
   Tailscale runs (not `0.0.0.0`; a public address is refused unless
   `allow_public = true`). Over Tailscale, only devices on your tailnet can
   reach the port at all (and they need a token like anyone else).
2. **TLS always** (TLS 1.3 or 1.2, rustls). The Mac makes a self-signed
   ECDSA P-256 certificate once (`~/.config/herder-tower/lan/`, key mode
   0600). There is no CA: the phone **pins the certificate's SHA-256
   fingerprint** (of the DER), which it gets out of band, in the pairing QR
   shown on the Mac. A changed IP doesn't matter; a different certificate
   (someone in between, another machine) fails the handshake. Hostname
   checks are not used (the certificate's name is `herder-tower`).
3. **Pairing by a one-time code shown on the Mac**: 8 digits, valid 5
   minutes, one use. Pairing again with the same `install_id` (the same
   app install) takes the code too; it gives that device a new token, and
   the old one stops working at once (its open streams close). Wrong tries are counted by address: after 5 from one
   address the code is void for that address (others can still pair), and
   after 30 in all it is void for everyone. Each wrong try also waits a
   second. Opening a new code replaces the old one.
4. **A long-lived random token per device** (`htk_` + 256 random bits,
   base64url), returned once at pairing. The Mac stores only its SHA-256
   (`~/.config/herder-tower/devices.json`, mode 0600) and compares in
   constant time. The phone keeps it in Android Keystore-backed storage
   (EncryptedSharedPreferences or a Keystore-wrapped DataStore).
5. **Every call needs `Authorization: Bearer <token>`**, the event stream and
   audio downloads included. The only unauthenticated call is
   `POST /v1/pair`, and it does nothing without the open code.
6. **Revocation** (`tower phone revoke <id|all>`, or the app's
   `DELETE /v1/device`): the token stops working on the next call, and open
   event streams of that device close within 5 seconds.
7. **Abuse limits**: 20 failed logins (bad token or code) from one address in
   a minute and it's refused for the rest of the minute; 32 connections at
   most, 8 from one address (more are closed unanswered); the TLS handshake
   and the request head must arrive within 10 s; request heads up to
   16 KiB. The token is checked from the head, **before any body is read**,
   and each call has its own body limit (`413 too_large` beyond it):
   `POST /v1/pair` 4 KiB; a plain WAV (`/v1/voice`, `/v1/transcribe`,
   `/v1/questions/{id}/voice`, `/v1/decisions/{id}/voice`) 24 MiB;
   multipart (`/v1/ask`, the voice calls, `/v1/questions/{id}/answer`,
   `/v1/decisions/{id}/answer`) 100 MiB; every other call 1 MiB.
   Recordings are 0.2 to 120 s (checked from the WAV header).
8. **Text from the phone** that is typed into a session (`/v1/ask` text,
   typed answers to questions and decisions, an approve/refuse note, a
   goal) loses its control characters (C0 and C1, escape sequences'
   introducers among them), except newlines and tabs. An address must start
   with a letter or digit.

What's left, honestly: anyone on the same network can see that a TLS server
runs on the port (and, by Bonjour, that it is this Mac's tower, its name and
its certificate's fingerprint, which every TLS handshake shows anyway; a fake
advert gets a phone nowhere, as the phone connects only with the pin), and could try codes during the 5 minutes a code is open
(5 tries from an address, 30 in all: at worst 1 in 3 million), or use up
the 30 from several addresses so the code has to be made again. A phone that is stolen while paired holds a valid
token until you revoke it (`tower phone revoke`). The certificate's private
key is as safe as your Mac account. Over Tailscale the same model holds:
TLS with the pinned certificate inside the tailnet's own WireGuard tunnel,
the same tokens, the same limits (the tailnet's address counts as private).

## Pairing flow

1. On the Mac: `tower phone pair`. It prints a QR, every address, the code
   and the certificate fingerprint. (The orb shows the same, from
   `tower phone pair --json`.) The QR is a link:

   ```
   herdertower://pair?h=192.168.1.20&p=7843&c=<8 digits>&fp=<64 hex chars>&hosts=lan:192.168.1.20:7843,tailscale:100.68.110.33:7843&dns=mac.tailnet.ts.net
   ```

   `h` and `p` are the first address, as before. `hosts` (only when there is
   more than one address, or the one is the tailnet's) is every address in
   the order to try them, comma-separated, each `<kind>:<IPv4>:<port>`;
   `dns` (only while the tailnet address is listened on) is the Mac's
   MagicDNS name. An app that doesn't know `hosts` and `dns` ignores them and
   pairs over `h`/`p` exactly as before.

   `tower phone pair --json` (the same in the daemon's `phone pair` answer;
   it also has `network`, as in the state, for the Mac to say what the
   phone must be on):

   ```json
   {
     "code": "12345678",
     "host": "192.168.1.20",
     "port": 7843,
     "hosts": [
       {"kind": "lan", "host": "192.168.1.20", "port": 7843},
       {"kind": "tailscale", "host": "100.68.110.33", "port": 7843, "dns_name": "mac.tailnet.ts.net"}
     ],
     "magic_dns": "mac.tailnet.ts.net",
     "fingerprint": "<64 hex chars, SHA-256 of the certificate's DER>",
     "link": "herdertower://pair?h=192.168.1.20&p=7843&c=12345678&fp=<hex>&hosts=lan:192.168.1.20:7843,tailscale:100.68.110.33:7843&dns=mac.tailnet.ts.net",
     "expires_in_s": 300,
     "tailscale": {"installed": true, "running": true, "state": "running", "ip": "100.68.110.33", "dns_name": "mac.tailnet.ts.net"}
   }
   ```

   Exactly:
   - `host` (string), `port` (number): the first entry of `hosts`. Kept for
     older apps.
   - `hosts` (array, at least one entry, in order): `kind` is `"lan"` or
     `"tailscale"` (other kinds may come later: skip what you don't know),
     `host` an IPv4 address, `port` a number, and `dns_name` (string) on a
     `tailscale` entry when Tailscale reports a MagicDNS name. The LAN entry
     comes first when there is one; there is at most one of each kind.
   - `magic_dns` (string or `null`): the Mac's MagicDNS name, set only while
     a `tailscale` entry is in `hosts`.
   - `tailscale` (object): `installed`, `running` (booleans), `state`
     (`"running"`, `"stopped"`, `"needs login"`, `"not installed"`,
     `"starting"`, …: for people), `ip`, `dns_name` (string or `null`). It
     describes Tailscale on the Mac even when it isn't listened on (stopped:
     `running: false`, and `hosts` has the LAN only).
   - `code`, `fingerprint`, `link`, `expires_in_s`: as before. One
     fingerprint for every address: it is one certificate.

   Stopped Tailscale (or `[lan] tailscale = "off"`): `hosts` is the LAN
   entry alone, `magic_dns` is `null`, and the link is the old one
   (`h`, `p`, `c`, `fp` only).

2. The app scans it (or the user types address and code; then the app shows
   the fingerprint of the certificate it got, grouped by four, and the user
   compares it with the one the Mac printed before the app goes on).
3. The app calls, pinning `fp`, on the first address that answers (in the
   order of `hosts`):

   ```http
   POST /v1/pair
   Content-Type: application/json

   {"code": "<the 8 digits>", "name": "Pixel 9", "install_id": "k3J9-2fQx_8aZ0pLm"}
   ```

   ```json
   200 {"token": "htk_…", "device": {"id": "d_3fa9c2e1", "name": "Pixel 9"}, "fingerprint": "<hex>", "api": 1,
        "features": ["quiet", "acts", "pairing", "bonjour", "reviews"], "repaired": false,
        "hosts": [{"kind": "lan", "host": "10.10.30.116", "port": 7843}],
        "network": {"range": "10.10.30", "name": "Office", "lan": {"host": "10.10.30.116", "port": 7843}}, "magic_dns": null,
        "bonjour": {"type": "_herdr-tower._tcp", "name": "Hassan’s MacBook Pro", "fp": "<hex>"}}
   400 {"error": {"code": "bad_install_id", "message": "…"}}
   403 {"error": {"code": "pairing_closed", "message": "…"}}
   403 {"error": {"code": "wrong_code", "message": "…", "tries_left": 4}}
   ```

   `install_id` (optional): a random id the app makes once per install and
   keeps, 8 to 64 letters, digits, `-` and `_` (anything else is `400
   bad_install_id`, and the code is not used up). When a paired device has
   the same `install_id`, that device is **paired again in place**: the
   same `device.id`, its replies route kept, its name updated if `name` is
   given, a new token (the old one stops working at once, and its streams
   close), and `"repaired": true`. Otherwise it is a new device
   (`"repaired": false`). Without `install_id` (an older app) it is always
   a new device, as before. The answer also says where the tower is now
   (`hosts`, `network`, `magic_dns`, `bonjour`, as `GET /v1/network`): store them.

4. The app stores every address (`hosts`, else `host`/`port`), the
   MagicDNS name, the fingerprint and the token, and from then on sends the
   token with every call (and merges the addresses the stream sends: see
   "Pairing on more than one network"). To connect it tries the addresses in order (the
   LAN first: on the home network it is the direct path), moving on after a
   short connect timeout (2 to 3 s); the MagicDNS name can stand in for the
   tailscale entry's IP if that changed. The certificate pin is the same
   for all of them; no hostname is checked. A paired app refreshes its
   list from `GET /v1/state` (`lan_hosts`): a Mac that starts Tailscale
   later, or moves to another LAN address, is found there.

## Pairing on more than one network

A phone knows only the addresses it was given. Paired at the office
(10.10.30.116), it can't reach the Mac at home (192.168.2.211), even on
the home Wi-Fi, by the addresses it knows, unless both run Tailscale.
What the tower does about it:

- The LAN listener follows this Mac's network (within 20 s), and every open
  event stream gets the new addresses at once (the `hosts` event): a phone
  connected over Tailscale while the Mac moves learns the new LAN address
  before it needs it.
- Each paired device keeps the networks it was paired on or connected from
  (its `networks` in devices.json and in `tower phone devices --json`:
  `{"range", "name" (then), "host", "port", "first_ms", "last_ms"}`; the
  tailnet's has range `tailnet`), noted when it pairs (every address
  listened on then) and when its event stream connects (the address it
  came in on). From those the Mac says, per phone, whether it **can reach
  the tower here** (it knows the LAN address the Mac has now, or Tailscale
  is listened on and it knows the tailnet's address) or was **paired on
  another network** (pair it again here, or use Tailscale). Settings >
  Phone and `tower phone devices` show it.
- Pairing again from the same install updates that phone's entry
  (`install_id`, above) instead of adding another. Entries an older app
  made are not merged by themselves (nothing says they are one phone):
  Settings > Phone removes the stale ones.
- When the phones used in the last day all were paired elsewhere, and
  Tailscale isn't running, both of the Mac's windows say once, quietly,
  under their status: "Phone: pair again on this network" (`phone_hint` in
  the state), which opens Settings at the Phone card. Never spoken, never a
  decision; gone once a phone pairs or connects here, or Tailscale runs.
- While the LAN listener runs, the Mac is **found by name** on that
  network (Bonjour), so a paired phone that knows none of the addresses
  there can still find it, without pairing again: see "Finding the Mac by
  Bonjour" below.

**What the Android app must do** (with `pairing` in `features`; an older
tower ignores `install_id` and sends no `hosts` event, and all of this
still works there as far as it can):

1. **Send `install_id`** in every `POST /v1/pair`: a random id made once
   per install and kept with the app's settings (16 random bytes as
   base64url without padding, 22 characters, fits the 8 to 64 letters,
   digits, `-` and `_`). A new install (or cleared data) makes a new one.
2. **A QR for a tower it is already paired with re-pairs that entry.** When
   a scanned QR's `fp` is the fingerprint of a tower the app has stored, it
   doesn't add a second tower: it pairs with the same `install_id`, puts
   the new token in that entry (the old one no longer works), takes
   `device.id` from the answer (the same one when `repaired` is true; a new
   one when the Mac had forgotten the phone, or is older), and merges the
   addresses of the QR (`hosts`, else `h`/`p`) and of the answer (`hosts`,
   `magic_dns`). The entry's other settings stay.
3. **Merge `hosts`** from the stream's `hello`, every `hosts` event, and
   the answers of `POST /v1/pair` and `GET /v1/network` (or the state's
   `lan_hosts`) into the stored addresses: add the new ones, keep the old
   ones (home and the office, for when it is back there; at most 8, the
   longest unused going first), and try them in this order: the latest
   list's, in its order, then the others, the one that last answered
   first. Update the MagicDNS name when one comes. Never drop the
   `tailscale` entry because a list came without it (Tailscale was stopped
   for a while). `network.name` (or "the network 10.10.30.x") can name
   where the phone is connected.
4. **When no stored address answers** (each tried with the short connect
   timeout), say: "Can't reach the tower on this network. Pair again here
   (scan the QR in Settings → Phone on the Mac), or turn on Tailscale on
   both." with a button that opens the scanner (and keep the stored
   pairing: back on a known network, it connects again). A `401
   unauthorized` is another thing: this phone's token was revoked or
   replaced, pair again. Before saying it, look for the Mac by Bonjour
   (below): only when that finds nothing either is it said.

### Finding the Mac by Bonjour

With `bonjour` in `features`, the Mac advertises the phone API by DNS-SD
(Bonjour, multicast DNS) while its LAN listener runs:

- **Service type** `_herdr-tower._tcp`, domain `local.`; the **port** is
  the API's (the SRV record's); the **host** is this Mac's
  (`<LocalHostName>.local.`), resolved to its addresses on that network.
- **Instance name**: the Mac's name (Sharing's Computer Name, "Hassan’s
  MacBook Pro"), or `[lan] bonjour_name`; at most 63 bytes. When another
  device on the network has that name, mDNSResponder registers another
  ("Hassan’s MacBook Pro (2)"): never match on the name.
- **TXT record**, in this order: `v=1` (the TXT's version), `fp=<the
  certificate's SHA-256, 64 lowercase hex digits, no colons>` (the
  pairing's `fingerprint`, which the QR's `fp` and `hello` carry), and
  `name=<the instance name asked for>` (for people: what the app may show,
  "Found Hassan’s MacBook Pro"). Each string is at most 255 bytes.
- Only on the interface that holds the LAN listener's address (Wi-Fi,
  Ethernet), never over the tailnet or a loopback address. When the
  listener moves (another network) it is withdrawn there and advertised on
  the new one; with Wi-Fi off it is withdrawn, and advertised again when
  the address is back (within 20 s each time). `[lan] bonjour = false`
  turns it off; so does the API going off.

`GET /v1/network`, the `hosts` event, the stream's `hello`, `GET
/v1/hello` and `POST /v1/pair`'s answer say what is advertised now:

```json
"bonjour": {"type": "_herdr-tower._tcp", "name": "Hassan’s MacBook Pro", "fp": "<64 hex chars>"}
```

or `"bonjour": null` while it isn't (Wi-Fi off, `[lan] bonjour = false`,
the Mac's mDNSResponder refused it).

**What the Android app does** (NsdManager):

1. **When**: no stored address answered (each with the short connect
   timeout, as above), and the phone is on Wi-Fi or Ethernet (not mobile
   data alone). Browse for `_herdr-tower._tcp` (`discoverServices(
   "_herdr-tower._tcp", NsdManager.PROTOCOL_DNS_SD, …)`) for up to 5 s,
   and stop browsing once a match connects (or the 5 s are up). Also
   browse when the user taps "Try again" on the "Can't reach the tower"
   message. Browsing doesn't need `bonjour` in `features` (an older tower
   just isn't found); a tower that lists it advertises while its LAN
   listener runs.
2. **Match by `fp`**: resolve each service found and read its TXT
   (`NsdServiceInfo.getAttributes()`). Take only one whose `v` is `1` and
   whose `fp`, lowercased with any `:` removed, equals the fingerprint
   stored for this tower at pairing. Ignore the others: the name and the
   TXT are not checked by anyone, and anyone can advertise them. Several
   towers on one network are told apart the same way.
3. **IPv4 first**: the API listens on one LAN address, the Mac's IPv4 one
   (unless `[lan] bind` names another), while the host resolves to its
   IPv6 addresses too. Of the resolved addresses (`getHostAddresses()` on Android 14, else
   `getHost()`), connect to an IPv4 one, at the resolved port; an IPv6
   one only when it is the only one there is. The Mac's host name
   (`.local.`) needs no lookup of its own: use the address NsdManager
   gives.
4. **The pin and the token are the same**: connect exactly as to a stored
   address, pinning the certificate's SHA-256 stored at pairing and
   sending the stored token. A certificate that doesn't match is the end
   of it (someone else, or another Mac with a copied TXT): never pair
   there, and never send the token to a certificate that isn't the pin. A `401` is a revoked token, as
   anywhere.
5. **Keep it**: once connected, merge the address (`kind: "lan"`, the
   host and port it connected to) into the stored addresses as any other,
   and take `hosts`, `network` and `bonjour` from the stream's `hello`. On
   the next try that address is among the stored ones (the latest-used
   first), so Bonjour is needed only on a network the phone hasn't been
   on with this Mac.

## Conventions

- Base: `https://<host>:<port>/v1`. JSON in and out (UTF-8), except audio.
- One request per connection (`Connection: close`); bodies need
  `Content-Length` (no chunked uploads). OkHttp does both by default.
  Send the `Authorization` header with the body's call: a call without a
  valid token is answered `401` from its head alone, its body unread.
- Times are Unix milliseconds (`*_ms`).
- Errors: `{"error": {"code": "<word>", "message": "<for a person>"}}` with
  status 400 (bad request), 401 `unauthorized` (missing, unknown or revoked
  token: re-pair), 403 (pairing), 404, 409 (it can't be done as things
  stand: answered already, a session at work, the same thing under way),
  413 (a body over the call's limit, `too_large`; files), 415 (audio,
  files), 429 `too_many_failures`, 503 (the brain, `hc`, or the transcriber
  isn't there; `starting`: the tower is still starting, try again in a
  moment). Some errors carry more beside `code` and `message` (`state`,
  `sessions`, `action`, `prs`, `tries_left`): each call says which.
- Addresses (`t3`, `vps/t3`) are URL-encoded in paths: `vps%2Ft3`. A `+`
  in a path is a plus; only in a query string is it a space.
- A **space** is named by its whole name, in any case, URL-encoded: in a
  path, `/v1/spaces/B2C%20support/goal` (a `/` in a name is `%2F`); in a
  query, `?space=B2C%20support` or `?space=B2C+support`. In a path, or in
  a body, a name no space has is `404 no_space`; a `?space=` filter that
  fits nothing answers an empty list.

## Reading: what the panel shows

| Call | Returns |
|---|---|
| `GET /v1/hello` | `{"api":1,"tower":"0.1.0","device":{"id","name"},"voice":"af_bella","phone_is_voice":bool,"audio_route":"phone"\|"mac","features":["quiet","acts","pairing","bonjour","reviews"],"bonjour":{…}\|null}` (`bonjour` as `GET /v1/network`). Use it to check a stored token, and to see what this tower has (`features`: see "Versioning"). |
| `GET /v1/state` | The daemon's state (below). Also pushed on the event stream. |
| `GET /v1/network` | `{"hosts": [Host], "network": {"range", "name", "lan": {"host", "port"} \| null} \| null, "magic_dns": string \| null, "tailscale": {…}, "bonjour": {"type", "name", "fp"} \| null}`: where the tower can be reached now (every address as the pairing lists them, the network, the MagicDNS name while the tailnet is listened on, Tailscale as in the state, and how Bonjour finds it on this network: see "Finding the Mac by Bonjour"). The same as the `hosts` event. |
| `GET /v1/agents?all=1` | `tower agents --json`: `{"agents":[…],"unreachable":[…]}`; each agent has `address, account, machine, name, spoken, agent, status, doing, title, folder, where`. `all=1` adds plain shells. Slow-ish (asks Herdr, and other machines). |
| `GET /v1/spaces` | `tower spaces --json`: `{"focus": name or null, "spaces":[{"space","focus","follow","mute","agenda","goal","done_when","standing","state","report","quiet_since_ms","ready_since_ms","closed_ms","archived","unread","unread_asked","unread_needs_you","running_jobs","sessions":[{"address","name","status","account"}],"last_activity_ms"}]}`. `goal` is the same as `agenda`; `state` is the space's as a mission (`open`, `quiet`, `ready`, `closed`: docs/quiet-tower.md, step 5 and 8); a space quiet for a day, or closed, is left out. `unread`: held news not yet read; of it, `unread_asked`: reports on work the user asked for (a watch's or a job's), `unread_needs_you`: someone there blocked on or waiting for the user. Show a space with either above 0 as needing a look (the panel marks it amber). |
| `GET /v1/jobs?status=open\|finished\|all&session=t3&limit=50` | `{"jobs":[{"job","session","name","space","account","status","asked","summary","started_ms","updated_ms"}]}`, newest first. `status`: `working`, `blocked`, `done`, `failed`, `error`, `exited`. |
| `GET /v1/history?limit=100&session=t3&kind=summary,answer&space=B2C%20support&before_ms=…` | `{"entries":[…]}`, oldest first, the latest `limit`. Page back with `before_ms` = the first entry's `at`. Summaries are `kind=summary`. `space` keeps one space's entries (its whole name, any case): with it, this is that space's Record. |
| `GET /v1/chat?before=h120&limit=50` | `{"messages":[…], "more": bool}`: **the chat thread**, the same one the Mac shows, newest last; `before` pages back (the first message's `id`), `more` says older ones remain. With `space=<name>`: the conversation with that space's own tower only; with `main=1`: the main one without those; with neither, every message. See "The chat" below. |
| `GET /v1/questions` | `{"questions":[…]}`: what blocked sessions ask, as question sets (see [questions.md](questions.md)); also in the state (`questions`; each agent's `question` id). A set with `kind: "text"`, `decision: "escalate"` and `reason: "it waits for you to do this"` is a session that stopped for the user to act (merge, approve, test, decide): its `title` is the request; no recommendations will come (show "waits for you" and a reply box, not a spinner). A dialog of several questions (`kind: "claude-ask"`) can be answered in the session's own terminal as well, tab by tab: the set follows it within about two seconds (a `question` event, `op: "update"`). `current_tab` is the tab the dialog shows (`tabs.length`: its review), `tabs[i].answered` is true for a question answered in the dialog already, and `tabs[i].answer` (when present) is what it was answered with, as the screen showed it. Open the card on `current_tab`; show an answered question as "Answered in the terminal: <answer>" (or just "Answered in the terminal") and ask it no more; send answers only for the questions still open. Every question answered there and not submitted yet: offer Submit only (`{"answers": [], "submit": true}`). |
| `GET /v1/questions/{id}` | One set. `404 no_question`. |
| `GET /v1/prs` | `{"prs":[…]}`: the PRs the tower's sessions opened, open ones and those closed in the last day, waiting ones first (see "PRs" below). |
| `GET /v1/prs/{owner}/{repo}/{number}` | `{"pr":{…}}`. `404 no_pr`. |
| `GET /v1/questions-policy` | `{"policy": "normal"\|"keep-moving"\|"ask-everything"}`: how questions are handled (also `question_policy` in the state). |
| `GET /v1/behaviors` | The user's behaviors (how the tower works), the fixed core and a summary; add, edit, enable/disable, remove, preview, undo and history: see [behaviors.md](behaviors.md). |
| `GET /v1/accounts` | `{"default": name, "accounts":[{"name","email"}]}`, the default first (from the claude-accounts mod; names are not fixed: colour by name). |
| `GET /v1/screenshots?limit=30&session=t3` | `{"screenshots":[{"id","at","session","name","space","account","job","caption","path","url"}]}`, newest first (still on disk: kept 30 days). |
| `GET /v1/attachments/{folder}/{name}` | A picture someone sent with a message (the image itself, by its bytes: `image/png`, `image/jpeg`, `image/webp`, `image/gif`). `{folder}/{name}` is the end of its path in a `request` entry's `attachments` (`…/attachments/1794165000000-phone/1-login.png` → `/v1/attachments/1794165000000-phone/1-login.png`), URL-encoded (panel attachments keep their original names, spaces and all). Only files directly in a folder of `attachments/`; `400 bad_path`, `404 no_attachment`, `415 not_an_image` (videos and other files aren't served). |
| `GET /v1/screenshots/{id}` | The image itself (`image/png`, `image/jpeg`, `image/webp` or `image/gif`). `400 bad_id`, `404 no_screenshot`. |
| `GET /v1/sessions/{address}/screen?lines=60` | `{"address","screen"}`: what that terminal shows (to see what a blocked agent asks). Up to 400 lines. |

A history entry: `{"at", "kind", "session"?, "name"?, "space"?, "job"?,
"account"?, "status"?, "text", "attachments"?, "held"?, "source"?, "id"?,
"record"?, "from"?, "from_name"?, "why"?, "options"?, "agenda"?}`. Keys
are left out when empty (`record` and `held` when false). `kind`:

| `kind` | What | Carries |
|---|---|---|
| `request` | What the user asked | `source` (`mac`, `voice`, `phone:<device>`), `attachments`; `space` when it was asked of a space's own tower |
| `answer` | A tower's answer | `space` when a space's own tower gave it |
| `watch`, `alert`, `job`, `summary` | News about a session | `session`, `name`, `space`, `status`, `job` |
| `note` | A line of activity; at a quiet level, what a tower wrote on a note turn | `record: true` then, with the `session`, `name` and `space` it was about |
| `peer` | One session's message to another (a space's thread) | `from`, `from_name` (the sender), `session`, `name` (the receiver), `space`, `status` (`delivered` or `undelivered`), `record: true` |
| `raised` | A decision put to the user (`tower raise`) | `id` (the decision's), `session`, `name`, `space`, `why`, `options`; `record: true` when it was raised before |
| `decision` | A decision that is over | `id`, `space`, `status` (`answered`, `dismissed`, `stopped`), `why`, `text` ("<title> -> <answer>"), `record: true` |
| `receipt` | The one line for a finished request | `agenda`, `space`; `record: true` when it was only kept |
| `space` | A space went quiet, is ready to close, or was closed | `space`, `status` (`quiet`, `ready`, `closed`), `record: true` |
| `screenshot` | A picture a session sent | `id`, `caption`, `path` (fetch it from `/v1/screenshots/{id}`) |
| `queue`, `pr`, `behavior`, `introduce`, `answered` | The send queue, a PR found, a behavior changed, sessions introduced, a question the tower answered | |
| `review` | A PR's review moved: its reviewer opened, a round, the owner's fix, agreed, ended | `id` (the PR's key), `session`, `name` (the reviewer, else the owner), `space`, `status`, `record: true` |

**`record: true`** means written down, not told: such an entry was never
spoken and is not a chat message (the chat leaves it out, and so does a
`chat` event). The Record view shows everything; the chat shows what was
told. It is decided when the entry is written, so the past never changes
when the tell level does.

The state (`GET /v1/state`, event `state`):

```json
{
  "version": 812, "updated_ms": 1794158000000, "pid": 28224, "running": true,
  "phase": "idle",
  "mode": "ptt", "mic": "closed", "armed": false, "transcribing": false,
  "speaking": false, "announcing": false,
  "brain": "done", "brain_app": "claude-code",
  "hearing": "", "heard": "what's everyone doing", "heard_ms": 0,
  "said": "Nothing is blocked.", "said_ms": 0,
  "alerts": [{"at_ms": 0, "terminal": "t22", "text": "Claude in eyzflow is done", "level": "done"}],
  "agents": [{"address": "t22", "name": "Claude in eyzflow", "status": "working", "app": "claude-code", "account": "default"}],
  "error": "", "talk_key": "ctrl", "voice": "af_bella", "stt_model": "…",
  "decisions_count": 2, "tell_level": "decisions",
  "brains": [{"name": "tower", "space": null, "address": "t7", "status": "idle", "model": "claude-opus-5-5", "effort": "high"}],
  "brain_model": {"program": "claude", "model": "claude-opus-5-5", "effort": "high", "…": "see \"The brains' model\""},
  "lan": "https://192.168.1.20:7843",
  "lan_hosts": [{"kind": "lan", "host": "192.168.1.20", "port": 7843}, {"kind": "tailscale", "host": "100.68.110.33", "port": 7843, "dns_name": "mac.tailnet.ts.net"}],
  "tailscale": {"installed": true, "running": true, "state": "running", "ip": "100.68.110.33", "dns_name": "mac.tailnet.ts.net"},
  "phone": "Pixel 9",
  "work": {"word": "Reading", "detail": "eyzflow", "since_ms": 1794158000000, "at_ms": 1794158004000, "done_ms": 0},
  "screenshots": [{"id": "1794160000000-login.png", "at": 1794160000000, "session": "t22", "name": "Claude in eyzflow", "space": "B2C support", "account": "default", "job": null, "caption": "The new login page", "path": "/Users/…/attachments/screenshots/1794160000000-login.png", "url": "/v1/screenshots/1794160000000-login.png"}],
  "reviews": [{"key": "eyzhub/eyz-mediaroom-serverless#1250", "repo": "eyzhub/eyz-mediaroom-serverless", "number": 1250,
               "url": "https://github.com/eyzhub/eyz-mediaroom-serverless/pull/1250", "space": "b2c-tickets",
               "owner": "t58", "owner_name": "b2c tickets mediaroom",
               "reviewer": "t61", "reviewer_name": "b2c-tickets-review-eyz-mediaroom-serverless-1250",
               "status": "changes", "rounds": 2, "critical": 2, "line": "the totals still lose cents",
               "decision": null, "created_ms": 1794238300000, "updated_ms": 1794238900000, "ended_ms": null,
               "chip": "Review: 2 critical, round 2", "…": "and its steps"}]
}
```

`reviews` (on a tower with the `reviews` feature): the PRs' reviews, every
one not over and those over in the last day, the latest first (`status`:
`reviewing`, `changes`, `agreed`, `stuck`, `ended`; `line`: the last word
on it, a report's, the owner's, the agreement's or why it ended; `chip`:
what the PR's chip says). The PR itself carries the short form (`review`).

`work` is what the brain is doing right now, as the panel's working line shows
it ("Delegating to orb-rust · 12s"): `{"word": "Delegating", "detail": "to
orb-rust", "since_ms": <turn began>, "at_ms": <this step began>, "done_ms":
<step finished, 0 while it runs>}`, or `null` between turns. A `state` event
is sent when it changes too.

`phase` (one word for the orb): `announcing`, `speaking`, `transcribing`,
`listening`, `thinking`, `blocked`, `armed`, `standby`, `idle`. Unknown keys
may be added later: ignore them.

The state stays small, because it is sent whole whenever it changes. Of the
Quiet Tower it has `decisions_count` (the open decisions outside muted
spaces: the badge), `tell_level`, `brain_model` (the model picker's report)
and `brains` (the towers that run: the main one, and each space's own). The
lists are not in it: the decisions, the missions and the spaces' threads
each have their own call and their own event (`GET /v1/decisions`,
`/v1/missions`, `/v1/thread`; events `decisions`, `missions`, `thread`).

## Acting: what the panel does

| Call | Body | Does |
|---|---|---|
| `POST /v1/ask` | `{"text": "what's everyone doing?"}`, or multipart with images (below). Optional `"space": "B2C support"` (a field named `space` in the form): asked in that space's view | Sends it to the tower like typing in the panel (it arrives as `[voice] …`). With `space`, to that space's own tower when it has one that can take it, else to the main tower with the space named in front (`In B2C support: …`); `404 no_space` for a name no space has (nothing is sent, no file saved). `202 {"sent": text, "attachments": [paths]}`, with `"space": name` when one was given. The canned "Okay." is played (on the phone when it's the voice). |
| `POST /v1/agents/{address}/answer` | `{"decision": "approve"\|"refuse", "note": "only the read"}` | Tells the tower, as the user would: "Approve what t22 is asking for. only the read". The tower reads the session's screen and answers it, as today. `202`. |
| `POST /v1/sessions/{address}/show` | | Brings that session on screen in the Mac's Herdr window (as `tower show <t>`: focuses its tab, or gives it one). `200 {"shown": address}`, `404 no_session`, `400 bad_address`, `503 herdr` (the window isn't open). |
| `POST /v1/focus` | `{"space": "B2C support"}` or `{"space": null}` | Focuses a space (its news is spoken, others held) or clears focus. `200 {"focus": name or null}`; `404 no_space`. |
| `GET /v1/device/audio` | | `200 {"route": "phone"\|"mac"}`: where this phone's replies are heard (below). |
| `PUT /v1/device/audio` | `{"route": "phone"\|"mac"}` | Sets it (kept in the Mac's devices.json, so it survives restarts). `400 bad_route`. Also in `hello` and the state as `audio_route`. |
| `GET /v1/audio?after=<id>` | | `{"clips": [{"id","kind","text","ms","url","at_ms"}]}`: clips meant for this phone (its own and the voice's) from the last 15 minutes, after clip `after`: what it missed while no stream was open. Empty while the tower is muted. |
| `POST /v1/hush` | | Stops speaking now, and drops what's queued (the phone gets `hush`). |
| `GET /v1/queue` | | `{"items": [{"id","session","name","text","at_ms","tries","parked"?,"parked_ms"?,"after"?}]}`: instructions the send queue holds for busy sessions (`tower queue`), oldest first. `parked` says why one won't go on its own (`gone`, `replaced`, `after-cancelled`, `start-failed`), since `parked_ms`; one parked for a week leaves the list (kept aside as expired; `tower queue retarget` still sends it). Each agent in the state also has `folder` and `queued` (how many wait for it). |
| `GET /v1/mute` | | `{"muted": bool}`: the tower-wide mute (`tower mute`). The state has it too (`muted`). |
| `POST /v1/mute` | `{"muted": true}` | Mutes the tower (or unmutes it with `false`): it stops talking now (the phones get `hush`), the clips waiting to be fetched are dropped, and it says nothing on the Mac or any phone until unmuted. Text, chat and questions go on. Kept across restarts. |
| `POST /v1/mode` | `{"mode": "ptt"\|"handsfree"\|"off"}` | The Mac microphone's mode. |
| `POST /v1/questions/{id}/answer` | `{"answers":[{"tab":0,"choice":1},{"tab":1,"choice":{"options":[0,2],"other":"…"}}],"submit":true}`; or multipart: that JSON as a part named `json`, files for tab N's typed answer as parts named `tab<N>` | Types the answers into the session's dialog, checking the screen after each step, then submits after checking the review. `200 {"ok":true,"id","submitted"}`; `409 {"error":{"code":"stopped"\|"changed"\|"answered"\|"cannot_answer","message":"where and why"}}` (`answered`: the dialog was answered in the session meanwhile, and the set is closed; `changed`: the dialog asks something else now, and the set under its new id is in the questions, with `replaces` naming the old one when it is the same dialog read again); `400 bad_tab` (an answer for a tab the set doesn't have; nothing is typed or saved); `404`. Every tab's files are checked before any is saved. What was answered in the session's terminal stands: answers for those tabs are left out, and the keys start where the dialog is. When every answer sent was for such a tab, nothing is typed: `409 cannot_answer` ("Nothing was typed: … answered in its terminal already; still open: …", or "…; only Submit is left"); the set stays open. `{"answers": [], "submit": true}` submits a dialog whose questions are all answered; with one still open it is `409 cannot_answer` ("Not every question is answered yet: … still open"). |
| `DELETE /v1/questions/{id}` | | Dismisses it (a text question that isn't one, or one the user answers in the terminal). |
| `PUT /v1/questions-policy` | `{"policy": "normal"\|"keep-moving"\|"ask-everything"}` | Saved in the Mac's config. |
| `POST /v1/prs/{owner}/{repo}/{number}/confirm` | `{}` | Step one of a merge: GitHub is asked again, and the answer says what merging would do: `{"token","expires_ms","pr":{…},"method":"merge"\|"squash"\|"rebase","can":bool,"blocked":null\|"why","needs_override":null\|"what fails","warnings":["…"]}`. Show it to the user; the token is for this phone, this PR and its head commit as shown, for two minutes, once. `404 no_pr`, `503 github`. |
| `POST /v1/prs/{owner}/{repo}/{number}/merge` | `{"token":"…"}` or `{"token":"…","override":true}` | Step two: merges with the repo's default method, pinned to the commit confirmed, never forced. `override: true` only after the user said a second time that the failing checks are unrelated. `200 {"merged":true,"pr":{…}}`; `403 bad_token` (missing, used, another PR or device, expired: confirm again); `409 {"error":{"code":"blocked"\|"needs_override"\|"changed"\|"merge_failed","message":"…"}}` (`changed`: new commits since the confirmation; `merge_failed`: GitHub refused, e.g. a required review). The token is used up either way. |
| `POST /v1/questions/{id}/interpret` | `{"tab":0,"text":"the second one"}` | `{"tab","choice":{"options":[1],"other":null}}`: a spoken answer as a choice, to show before sending. |
| `POST /v1/questions/{id}/voice?tab=N` | a WAV take | `{"tab","heard","choice"}`: transcribed by the Mac's Whisper, then as `interpret`. |
| `PATCH /v1/device` (or `PUT`) | `{"name": "Work phone"}` | Renames this phone (as Settings > Phone and `tower phone rename` do). `200 {"device": {"id", "name"}}`; `400 bad_name` (empty). |
| `DELETE /v1/device` | | Unpairs this phone (its token stops working). |

### Files with an ask

`POST /v1/ask` also takes `multipart/form-data` (OkHttp `MultipartBody`,
`MultipartBody.FORM`): a `text` field and up to **6 file parts** (any part
with a file name, or named `image…`/`file…`):

- **images**: PNG, JPEG, WebP or GIF (checked by their bytes; a file named
  as an image that isn't one is refused), **15 MB** each;
- **videos** (`.mov .mp4 .m4v .webm .mkv .avi`, or `Content-Type: video/*`)
  and **any other file** (PDF, text, …): **50 MB** each.

```
POST /v1/ask
Content-Type: multipart/form-data; boundary=…

--…  Content-Disposition: form-data; name="text"
what's wrong on this page? [Image #1]
--…  Content-Disposition: form-data; name="image"; filename="login.png"   Content-Type: image/png
<bytes>
--…  Content-Disposition: form-data; name="image"; filename="console.jpg"  Content-Type: image/jpeg
<bytes>
--…--
```

Every file is checked before any is saved (one refused file: nothing is
saved). The Mac saves them as the panel saves its attachments, in a new
folder per message
(`~/.local/share/herder-tower/attachments/<ms>-phone-<8 hex>/<n>-<name>.<ext>`;
attachments are never pruned) and the tower gets exactly what the panel
sends: `[voice] <text> [attached: <path 1> | <path 2>]`, where the text has
the panel's token for the Nth file: `[Image #N]`, `[Video #N]` or
`[File #N: <file name>]`. Put the tokens where the user put the files; any
file the text doesn't mark gets its token in front (`[Image #2] what's wrong
on this page? [Image #1]`; a `[File #N:` token counts by its number). Text
may be empty when there are files.

```json
202 {"sent": "[Image #2] what's wrong on this page? [Image #1]", "attachments": ["/Users/…/attachments/1794165000000-phone-3fa9c2e1/1-login.png", "…/2-console.jpg"]}
413 {"error": {"code": "too_many_files" | "image_too_large" | "file_too_large", …}}
415 {"error": {"code": "not_an_image", …}}
400 {"error": {"code": "bad_multipart" | "empty", …}}
```

A multipart body is 100 MiB at most; a JSON ask 1 MiB.

Asked in a space's view, the form has one more field, `space`:

```
--…  Content-Disposition: form-data; name="space"
B2C support
```

A spoken request in a space's view goes the same way: transcribe the take
(`POST /v1/transcribe`), then `POST /v1/ask` with the text and `space`.
(`POST /v1/voice` hands what it heard to the main tower.)

## The chat

There is one chat thread, the tower's, on every screen: the Mac and every
phone show the same messages. It lives on the Mac in the history
(`history.jsonl`, kept across restarts); what the phone sends goes into it
(marked with its source) and shows on the Mac, and the other way round.
Inside it, each space that has a tower of its own has its own conversation
(`space`, below).

A message:

```json
{
  "id": "h812", "at": 1794158000000, "updated_at": 1794158000000,
  "role": "user",                // user | tower | screenshots | event
  "kind": "request",             // the history's: request, answer, receipt, screenshot, watch, alert, job, summary
  "text": "[Image #1] [File #2: notes.pdf] compare these",
  "source": "phone:d_9fce5c6c",  // mac (typed in the panel, tower ask) | voice (the Mac's microphone) | phone:<device> | tower
  "space": null,                 // a space's name: see "Whose conversation"
  "attachments": [
    {"n": 1, "kind": "image", "name": "1-login.png", "path": "/…/attachments/…/1-login.png", "url": "/v1/attachments/1794-phone/1-login.png"},
    {"n": 2, "kind": "file",  "name": "2-notes.pdf", "path": "/…/2-notes.pdf", "url": null}
  ],
  "screenshots": []
}
```

- **user**: what the user asked, typed or spoken, from any screen. The text
  keeps the panel's tokens; `attachments[n-1]` is the file for token `#n`
  (`kind` `image`, `video` or `file`; pictures have a `url`).
- **tower**: an answer, or the one receipt for a finished request (`kind`
  `receipt`: only one the user was told; a receipt kept for the record is
  no message). Screenshots it attached (`[shot:…]` in what it wrote; never
  in `text`) are in `screenshots`:
  `{"id","caption","session","name","account","at","url"}`.
- **screenshots**: a card of screenshots a session sent (several sent by
  the same session within two minutes make one card; `updated_at` moves
  when one joins). `session`, `name`.
- **pr**: a PR a session opened (see "PRs"): `text` is its title when
  found, `pr` the PR as it is now. It changes in place: a `chat` `update`
  (and a `pr` event) when its checks finish or it's merged or closed,
  never a new message. `session`, `name`, `space`.
- **event**: news the panel shows between messages (a watched session
  finished, a job done or blocked, an alert, a summary), `text` as the
  panel words it; `session`, `name`, `status`.

`id` is `h<line>`: stable, unique, in order. What the tower is writing
right now isn't a message yet: the state's `work` (its working line) and
`said` (what it is saying) show it until the answer lands.

### Whose conversation: `space`

Every message has `space` (a space's name, or `null`).

- On a **request** (`kind: "request"`): the space in whose view it was
  asked (`POST /v1/ask` with `space`, the Mac's space view, `tower ask
  --space`). `null`: asked of the tower itself.
- On an **answer** (`kind: "answer"`): the space of the request it
  answers. A space's own tower's answers carry its space; the main
  tower's answer to a request that named a space is that space's too.
- On anything else, where it comes from: a `receipt`'s request's space, a
  piece of news' or a PR's session's space.

So the app shows two kinds of conversation from one thread:

- **A space's own**: the messages of kind `request`, `answer`, `receipt` or
  `act` whose `space` is that space. `GET /v1/chat?space=<name>` returns
  exactly these, and pages back with `before` (the first message's `id`)
  and `limit` as the main chat does, `more` saying older ones remain. An
  `act` (what the tower opened in answer to a request) is the conversation
  of the request it answers: asked of the space's own tower, or of the main
  tower as `In <space>: …`.
- **The main one**: everything else, that is every message except the
  requests, answers and acts that have a `space`. `GET /v1/chat?main=1`
  returns exactly these. (A receipt that names a space is in both: it is
  the reply to a request that may have been asked in either place.)

`GET /v1/chat` with neither returns every message, as it always did, and a
`chat` event carries each message with its `space`: put it where the rule
says.

A space that has no tower of its own that can take a request (it has no
live session, its tower is starting or waits on its own screen, or
`[brains] per_space` is off) has it handed to the main tower with the space
named in front: such a request's `text` is `In <space>: <what was asked>`,
and its `space` is still that space. The app may show the text without that
prefix in the space's own conversation. Which tower a space has is in
`GET /v1/missions` (`tower`).

## PRs

Every PR the tower's sessions open is a chip in the chat (a message of
`role` `pr`), the same on the Mac and every phone, from when it's found
(a `github.com/…/pull/N` link in what a session says at the end of a turn,
or what its `gh pr create` printed) until it's merged or closed. The Mac
asks GitHub about each open one every 30 s while its checks run or while it
waits for the user's merge (however old: it is often merged on GitHub), every
90 s otherwise, and again at once when the queue is looked at (`GET
/v1/decisions`, the Mac's windows opening; at most every 15 s). Merged on
GitHub, its merge decision closes, and so does an open question of its
space's sessions that names it ("#786" or its link). A PR:

```json
{
  "key": "eyzhub/eyz-mediaroom-serverless#1250",
  "repo": "eyzhub/eyz-mediaroom-serverless", "number": 1250,
  "url": "https://github.com/eyzhub/eyz-mediaroom-serverless/pull/1250",
  "title": "release: support tickets can open a new request beside an open one",
  "base": "main", "head": "dev", "head_sha": "0dcd66e…",
  "state": "open",                   // open | merged | closed
  "draft": false,
  "mergeable": "mergeable",          // mergeable | conflicting | unknown (GitHub still working it out)
  "checks": {"state": "failing",     // pending | passing | failing | none
             "failing": ["CI / lint"], "pending": [], "passed": 4, "total": 5},
  "session": "t58", "name": "b2c tickets mediaroom", "space": "b2c-tickets",
  "waiting": true,                   // its session ended its turn on it, or asked the user to merge
  "found_ms": 1794238300000, "checked_ms": 1794238390000,
  "closed_ms": null,
  "merged_by": null,                 // mac | phone:<device> | github, once merged
  "merge": {                         // whether the Merge button works now
    "can": true,
    "blocked": null,                 // why not at all: merged, closed, a draft, conflicts
    "needs_override": "1 check failing: CI / lint. Merge anyway only if you know the failure is unrelated.",
    "warnings": ["It merges into main: that ships to production.",
                 "The review isn't agreed: 2 critical issues open, round 2."]
  },
  "review": {                        // its reviewer session's review ("PR review" below); absent when it has none
    "status": "changes",             // reviewing | changes | agreed | stuck | ended
    "rounds": 2,                     // reports that found critical issues
    "critical": 2,                   // critical issues in the last report
    "reviewer": "t61", "reviewer_name": "b2c-tickets-review-eyz-mediaroom-serverless-1250",
    "line": "Review: 2 critical, round 2"   // what the chip says
  }
}
```

Show "Waiting for you" when `waiting` and `state` is `open`. The Merge
button: disabled with `merge.blocked` as the reason when it's set. Tapping
it calls `…/confirm` and shows the answer (repo, number, `base`, the
checks, `method`, every warning); "Merge into <base>" then calls `…/merge`
with the token. When `needs_override` is set, that button first shows it
and asks once more ("Merge anyway: the failure is unrelated"), and only
then sends `override: true`. Afterwards the chip changes by itself (`pr`
event, chat `update`); the Mac tells the session ("The user merged PR #N
…", queued) and the brain. The brain never merges.

**PR review.** A PR a session opens in a space whose `review` is on (every
space, unless the user turned it off: "A mission" below) gets a reviewer
session of the tower's: it looks for critical issues only, comments on the
PR, talks to the PR's session directly and agrees with it, or after three
rounds the disagreement is a decision of the user's (kind `choice`, `why`
`review`, about both sessions; its answer goes to both). The PR's `review`
says where that stands; show its `line` on the chip (the Mac colours it
amber until `status` is `agreed`). A review that isn't agreed is among
`merge.warnings` (and the confirm's `warnings`): it never blocks the
merge, which stays the user's. The `pr` event carries a change of it. A
tower that has this says `"reviews"` in `features`; the state has
`reviews` (below).

Example:

```
POST /v1/prs/eyzhub/eyz-mediaroom-serverless/1250/confirm   {}
→ 200 {"token":"9f2c…","expires_ms":1794238510000,"method":"merge","can":true,"blocked":null,
       "needs_override":null,"warnings":["It merges into main: that ships to production."],"pr":{…}}
POST /v1/prs/eyzhub/eyz-mediaroom-serverless/1250/merge     {"token":"9f2c…"}
→ 200 {"merged":true,"pr":{…,"state":"merged","merged_by":"phone:d_9fce5c6c"}}
event pr   {"op":"closed","pr":{…,"state":"merged"}}
event chat {"op":"update","message":{"id":"h1840","role":"pr",…,"pr":{…,"state":"merged"}}}
```

## The Quiet Tower

The tower does the work and keeps the record; it speaks when the user
asked, or when the decision is the user's ([quiet-tower.md](quiet-tower.md)).
For the app that means: it opens on the **queue** of decisions and notifies
for a new decision and for nothing else; a **space** is a mission, with a
goal, the thread between its sessions and (usually) a tower of its own; one
sentence **launches** a space; and the user chooses **how much the tower
tells** them and the **model** its brains run on.

A tower has all of this when `GET /v1/hello` lists the feature `quiet` (see
"Versioning"). Every call here needs the bearer token, like the rest
(`503 starting` is what one would get from a tower that is not whole yet;
the API starts only once it is).
[mobile-api-quiet-walkthrough.md](mobile-api-quiet-walkthrough.md) is each
of them run with `curl` against a tower, with what came back.

| Call | Body | Returns |
|---|---|---|
| `GET /v1/decisions?space=<name>&all=1` | | `{"space": name or null, "count": n, "decisions": [Decision]}` in queue order. Without `space`: the main queue, muted spaces left out. With it: that space's own, muted or not. `all=1` adds `"closed": [Decision]`, the last 50 that closed, newest first. |
| `GET /v1/decisions/{id}` | | `{"decision": Decision}`: an open one, or one that closed lately. `404 no_decision`. |
| `POST /v1/decisions/{id}/answer` | `{"choice": n}`, `{"other": "text"}`, or a question's answers (below) | What the answer did (below). `409 closed` when it is over already. |
| `DELETE /v1/decisions/{id}` | | Dismisses it: out of the queue, nothing sent (a question's sets are dismissed with it). `200 {"dismissed": id, "state": "dismissed"}`; for one that is over already, `200` with the `state` it ended in. `404 no_decision`; `409 cannot_dismiss` (it can't go now: its answer is going in, say). |
| `POST /v1/decisions/{id}/interpret` | `{"tab": 0, "text": "the second one"}` | A spoken answer as a choice, to show before sending (below). |
| `POST /v1/decisions/{id}/voice?tab=N` | a WAV take | The same from a recording, with `heard`. |
| `GET /v1/missions` | | `{"spaces": [Mission], "quiet": [name], "closed_today": [name]}` |
| `POST /v1/spaces/{space}/goal` | `{"goal": "…", "done_when": "…", "standing": false}`, each optional, at least one | Sets what is given (`goal: ""` clears the goal). `200 {"space", "goal", "done_when", "standing", "state", "report", "mission": Mission or null}`. `400 bad_goal`, `404 no_space`. |
| `POST /v1/spaces/{space}/follow` | `{"on": true}` | `200 {"space", "follow": bool, "mission": Mission or null}`. `400 bad_on`, `404 no_space`. |
| `POST /v1/spaces/{space}/mute` | `{"on": true}` | `200 {"space", "mute": bool, "mission": Mission or null}`. Its decisions leave (or rejoin) the main queue and the count at once. `400 bad_on`, `404 no_space`. |
| `POST /v1/spaces/{space}/close` | `{"force": false}` (optional) | Closes the space. `202 {"action": Action}`: see "Actions". `409 busy` while a session there works or waits for the user (unless `force`), `409 closing`, `404 no_space`. |
| `GET /v1/thread?space=<name>&limit=50&before=h120` | | `{"space", "messages": [Message], "more": bool}`: what the sessions of a space wrote to each other, newest last; pages like the chat. |
| `GET /v1/setups` | | `{"setups": [{"name", "account", "about", "repos": [folder], "roles": {folder: role}}]}`: the projects a space can be launched with (`about`: what the project is; `roles`: what each repo is in it, only those that have one). The first is the default. |
| `POST /v1/launch` | `{"goal": "Fix the login redirect", "setup": "eyzflow", "name": "login", "dry_run": false}` (`goal` required) | With `dry_run`: `200`, the plan. Without: `202 {"action": Action}`, see "Actions". `400 empty`, `400 bad_launch`, `404 no_setup`, `409 no_setups`, `409 launching`. |
| `GET /v1/actions` | | `{"actions": [Action]}`, newest first: the launches and closes the phones started since the tower started (the last 50). |
| `GET /v1/actions/{id}` | | `{"action": Action}`. `404 no_action`. |
| `POST /v1/decisions/{id}/rule` | `{"answer": "A sidebar"}` (optional) | Makes the answer just given a standing rule: see "Make it a rule". `202 {"sent": the request, "space": name or null}`. `409 not_answered`, `409 no_rule`, `404 no_decision`. |
| `GET /v1/acts` | | `{"acts": [Act]}`, newest first: what the tower opened lately (the last 50), each with what it assumed and whether it can be undone. See "Acts". |
| `GET /v1/acts/{id}` | | `{"act": Act}`. `404 no_act`. |
| `POST /v1/acts/{id}/undo` | `{"force": false}` (optional) | Ends what that act opened. `200 {"act": Act, "closing", "sessions_ended", "worktrees_removed", "kept_worktrees", "space_closed"}`. `409 held` while work would be lost (unless `force`), `409 undone`, `409 cannot_undo`, `404 no_act`. |
| `GET /v1/tell` | | `{"level": "decisions"\|"results"\|"everything", "voice": "off"\|"decisions"\|"replies"}` |
| `POST /v1/tell` | `{"level": "results", "voice": "decisions"}`, each optional, at least one | `200 {"level", "voice"}`, with `"was"` when the level was set and `"voice_was"` when the voice was. `400 bad_level`, `400 bad_voice`. |
| `GET /v1/model` | | The brains' model report (below), with `"restarted": []`. |
| `POST /v1/model` | `{"model": "claude-fable-5-2", "effort": "high", "restart": false}`, each optional, at least one | The report, with `restarted` (the names of the brains restarted). `400 bad_model`, `400 bad_effort`, `409 codex`. |

### A decision

A decision is the one thing put in front of the user: something only they
can decide. Three sessions asking the same thing are one decision; two PRs
of one repo waiting for a merge in one space are one decision.

```json
{
  "id": "d4f7981cc13c3",             // d + 12 hex digits: this decision, from when it opened
  "key": "raise:fix-login:tabs or a sidebar for the settings",
  "kind": "choice",                  // permission | question | merge | choice | failed | raised | blocked
  "space": "fix-login",              // or null
  "sessions": [
    {"address": "t3", "name": "fix login eyzflow", "waiting": true},
    {"address": "t5", "name": "fix login mediaroom", "waiting": true}
  ],
  "waiting": 2,                      // how many of them are blocked on it now
  "title": "Tabs or a sidebar for the settings?",
  "detail": "",                      // more to read before deciding: the command, the plan; "" when none
  "why": "design",
  "options": [
    {"label": "Tabs", "why": "matches the mock", "pick": true},
    {"label": "A sidebar", "why": "more room for later sections", "pick": false}
  ],
  "pick": "Tabs",                    // the tower's pick (an option's label), or null
  "prs": [],                         // a merge's PRs, in the order to merge them
  "question_id": null,               // a question's or a permission's question set
  "task": null,                      // a failed decision's agenda task
  "first_ms": 1794368747886,
  "last_ms": 1794368749472,
  "count": 2,
  "state": "open",                   // open | answering | answered | stopped | dismissed
  "muted": false
}
```

| `kind` | It is | `options` | Answered with |
|---|---|---|---|
| `question` | What a session asks: a dialog of questions, a question at the end of its turn, or a turn that ended waiting for the user ("waits for you") | The tower's recommendations, the first one its pick (for a set of several questions, option *i* answers each with its recommendation *i*). None for a wait. | `{"choice": n}`, `{"other": "text"}` (one question only), or the answers a question set takes |
| `permission` | A permission, plan or approval menu a session is blocked on | The recommendations; with none, the menu's own options and no pick | the same |
| `choice` | Raised by the tower with choices | As raised, the first one the pick | `{"choice": n}` or `{"other": "text"}` |
| `raised` | Raised by the tower, in a line | None | `{"other": "text"}` |
| `failed` | A task that failed again after one retry | "Try again", "Drop it"; no pick | `{"choice": 0}` (again), `{"choice": 1}` (drop), `{"other": "what to do instead"}` (again, with those words) |
| `merge` | The PRs of one repo that wait for a merge in one space | One per PR, in order; the pick is the first when it can be merged, its checks pass and its base is not production | Not here: see "A merge" |
| `blocked` | A session blocked on something the tower can't read (a dialog it doesn't know): no question set comes of it, but the user still has to act | None | Not here: it is answered in the session itself. Offer "Show on Mac" (`POST /v1/sessions/{address}/show`, the address from `sessions`). Answering returns `400 bad_answer`. It closes by itself (`answered`) once the session is no longer blocked |

- A `kind` the app doesn't know (a newer tower's) is still the user's: show it as a plain card with its `title` and `sessions` and count it; never leave it out.
- `id` is stable for the life of the decision. The same thing needing the
  user again after it closed is another decision, with another id.
- `key` is what makes two sightings the same decision (the space and what
  is asked, lowercased, punctuation dropped). The same key again never
  makes a second decision: it adds 1 to `count`, moves `last_ms`, and adds
  its session to `sessions`. `first_ms` is when it opened and never moves.
- `sessions` are the sessions it is about. `waiting` on a session: blocked
  on it now (false: it works on something else meanwhile); `question_id`
  on a session of a `question` or `permission`: that session's own
  question set. The decision's `question_id` is its first session's: the
  set to show as the card's steps (`GET /v1/questions/{id}`, or
  `questions` in the state) when it has several questions, more than its
  options say, or a dialog to follow.
- `detail`: what the `title` can't say, to show under it (monospace keeps
  a command readable; it can have line breaks), cut to about 600
  characters with an ellipsis. For a `permission`, the command it would run
  (a Codex approval's command, the plan to approve for a plan); for a
  `question`, its question set's detail (the message that asks, its end
  kept, so it starts with "…" when cut); for `choice` and `raised`, what the
  tower gave with it (`tower raise --detail`; raised again with another, it
  changes in place, the same id); `""` for a `merge` (its `prs` say it) and
  whenever there is nothing more. A tower from before this has no `detail`:
  read a missing one as `""`.
- `why`: the reason a question went to the user (the tower's words), the
  category a raise gave (`merge`, `deploy`, `delete`, `data`, `secrets`,
  `money`, `design`, `failed`, `waiting`, …), `merge`, or `failed`.
- `state`: `open` is a card. `answering`: its answer is going in (keys
  into a dialog, or a reply waiting for its session to stop): show
  "Sending your answer", and don't count it. `answered`, `stopped` (over
  without an answer: its sessions are gone, its PRs closed), `dismissed`:
  over. The queue lists only `open` and `answering` ones; a closed one is
  read by its id or with `all=1`. An answer that stops half way (the
  dialog changed while it was typed) leaves it `open` again, and its
  question set says so (`status: "failed"`, `error`).
- `muted`: its space is muted. A muted space's decisions are not in the
  main queue (`GET /v1/decisions` without `space`) nor in `count` /
  `decisions_count`, and the tower says nothing about them; they are in
  the space's own queue (`?space=`), and in the `decisions` event marked
  `muted: true`.
- **Order**: by weight, sessions waiting × time since `first_ms`,
  heaviest first, then oldest first. The tower orders them again every 30
  seconds; show them in the order given.

### Answering

```
POST /v1/decisions/d4f7981cc13c3/answer   {"choice": 0}
→ 200 {"ok": true, "id": "d4f7981cc13c3", "state": "answered", "answer": "Tabs",
       "sent": [{"session": "t3", "name": "fix login eyzflow", "queue_id": 7, "sent": true},
                {"session": "t5", "name": "fix login mediaroom", "queue_id": 8, "sent": false}],
       "errors": []}
```

`choice` is an option's index, from 0. `other` is the user's own words.
What the answer does, and what comes back, by kind:

- **`choice`, `raised`**: the chosen option's words (or the user's) go to
  the sessions that wait on it, through the send queue, as an instruction
  (to all its sessions when none waits), and the decision closes.
  `sent[].sent` is false when that session is busy: it gets it when it
  stops. `200 {"ok", "id", "state": "answered", "answer", "sent": […],
  "errors": […]}`.
- **`question`, `permission`**: the question set of every session of the
  decision is answered (typed into its dialog, or sent as a reply).
  `200 {"ok", "id", "state": "answering"|"answered", "answered":
  [{"session", "question", "status"}], "errors": […]}`. This call returns
  when the typing is done, which can take some seconds per session. Its
  body is `{"choice": n}`, `{"other": "text"}`, or exactly what
  `POST /v1/questions/{id}/answer` takes: `{"answers": [{"tab": 0,
  "choice": 1}, {"tab": 1, "choice": {"options": [0, 2], "other": "…"}}],
  "submit": true}`. As a multipart form too: that JSON as a part named
  `json`, files for tab N's typed answer as parts named `tab<N>` (the same
  kinds and sizes as `/v1/ask`'s files; with `{"other": "text"}` and one
  question, the files go with it as `tab0`).
- **`failed`**: `{"choice": 0}` sends the task to its session once more,
  `{"choice": 1}` drops it, `{"other": "…"}` sends it again with those
  words. `200 {"ok", "id", "state": "answered", "task", "requeued": {…}}`
  (or `"abandoned": {…}`).
- **`merge`**: `400 merge` (below).

Errors, each `{"error": {"code", "message", …}}`:

| Status | `code` | When |
|---|---|---|
| 404 | `no_decision` | No decision has that id |
| 409 | `closed` | It is over already: answered on another screen or in the session, dismissed, or its sessions are gone. `error.state` says how (`answered`, `dismissed`, `stopped`). Drop the card; nothing was sent. |
| 400 | `bad_answer` | The body doesn't fit: no such option, no `choice`, `other` or `answers`, `other` for a set of several questions, empty words |
| 400 | `merge` | It is a merge: `error.prs` lists its PRs' keys |
| 400 | `bad_json`, `bad_multipart`, `no_json` | The body doesn't read (`no_json`: a form without its `json` part) |
| 400 | `bad_answers`, `bad_tab`, `no_files` | A form's answers don't read, name a tab the set doesn't have, or the decision is no question (only a question's typed answer takes files) |
| 413, 415 | `too_many_files`, `image_too_large`, `file_too_large`, `not_an_image` | A file is refused, as for `/v1/ask`; nothing is typed or saved |
| 409 | `stopped`, `changed`, `answered`, `cannot_answer` | A question's answer didn't go in, as `/v1/questions/{id}/answer` says it: the typing `stopped` half way, the dialog `changed` (asks something else now), it was `answered` in the session meanwhile, or it can't be answered now. The decision is still open unless the next `decisions` event says otherwise. For a `choice`, `raised` or `failed`: `cannot_answer` when the queue or the ledger refused it. |

**A merge** is not answered through its decision: merging is the PR's own
two-step flow, which shows the user what merging would do before it is
done. Each of the decision's `prs` has what the app needs:

```json
{"key": "o/app#12", "repo": "o/app", "number": 12, "base": "dev", "title": "…",
 "url": "https://github.com/o/app/pull/12",
 "checks": "pass",                 // pass | pending | fail | none (no checks ran)
 "note": "checks passing",         // the checks in words ("1 check failing: lint")
 "session": "t58",
 "merge": {"can": true, "blocked": null, "needs_override": null, "warnings": []}}
```

`repo` is `owner/name`: call `POST /v1/prs/{owner}/{name}/{number}/confirm`,
show its answer, then `…/merge` with the token (see "PRs"). `merge` is what
a PR chip has. When its PRs are merged or closed the decision closes by
itself (`answered` when one merged); `DELETE` takes it out of the queue
without merging, and those PRs make no decision again while they wait.

**A spoken answer** (`…/interpret` with text from `/v1/transcribe`, or
`…/voice` with a WAV take) is read against the decision before anything is
sent:

```
POST /v1/decisions/d4f7981cc13c3/interpret   {"text": "the first one"}
→ 200 {"tab": 0, "choice": {"options": [0], "other": null}, "answer": {"choice": 0}}
```

- For a `question` or `permission` it is exactly `/v1/questions/{id}/interpret`
  on its question set: `tab` is the question, and `choice` goes into that
  tab's answer (`{"tab": 0, "choice": {"options": [0], "other": null}}`).
  No `answer` key.
- For a `choice`, `raised` or `failed` there is one "tab", its options
  (`tab` 0), and `answer` is the body to send to `…/answer`: `{"choice":
  n}`, `{"other": "the words"}`, or `null` when nothing was said.
- `…/voice` adds `"heard": "the first one"`. `400 bad_tab`, `400 merge`,
  `404 no_decision`, `409 closed`; for a take, `415 bad_audio` and
  `400 bad_length` as `/v1/voice`.

### Notifying: a new decision, and nothing else

The app notifies the user when a decision is new, and for nothing else:
not for a repeat, not for a change, not for news. The `decisions` event is
enough to do it:

1. Keep the set of decision ids the app has seen, on the device (it must
   survive the app being closed). Every id in every `decisions` event, or
   in a `GET /v1/decisions` answer, goes into it: muted or not, whatever
   its `state`.
2. A decision is **new** when its id is not in that set. Notify for it
   when it is new, `muted` is `false` and `state` is `open`. Then add it.
3. Everything else is a change to something already shown, and is silent:
   - the same thing asked or raised again (another session, the same
     line, another PR of the repo) is the same `id` with a higher `count`,
     a later `last_ms`, and maybe one more entry in `sessions` or `prs`.
     Never a new id;
   - `waiting` going up or down, `options` or `pick` changing, `state`
     moving between `open` and `answering`, the order changing;
   - a muted space's decision is seen (and remembered) but never notified,
     not when the space is unmuted either, as on the Mac.
4. An id that was in the last event and is not in this one is over
   (answered, dismissed, stopped): take its card and its notification away.

The stream sends a `decisions` event right after `hello` and `state`, so an
app that was away catches up on connecting with the same rule: what it has
not seen yet is new. `count` in the event (and `decisions_count` in the
state) is the badge: the open decisions outside muted spaces, those whose
answer is going in included.

A decision is also said on the Mac once, when it first appears (an alert:
a chat message of role `event`, and one spoken sentence, as an `audio` clip
when a phone is the voice). The app needs neither to notify.

### A mission

Every space is a mission: what it is for, who is in it, what it asks of the
user, and where it stands.

```json
{
  "spaces": [{
    "name": "fix-login",
    "goal": "Fix the login redirect",          // "" when it has none
    "done_when": "The redirect lands on the dashboard",   // "" when none
    "standing": false,                         // true: the user keeps it; it is never ready, never closed by the tower
    "review": true,                            // the PRs its sessions open get a reviewer session (on unless the user turned it off)
    "state": "open",                           // open | ready | quiet
    "follow": false, "mute": false,
    "needs": 1,                                // its open decisions, muted or not
    "report": "",                              // its closing report, once the tower wrote one
    "sessions": [{"address": "t3", "name": "fix login eyzflow", "status": "working",
                  "account": "orca", "worktree": "/…/.tower-worktrees/fix-login/eyzflow",
                  "branch": "fix-login/eyzflow", "port": 3001,
                  "said": "The redirect now goes through /auth/callback."}],
    "requests": [{"id": "a1", "title": "Fix the login redirect", "state": "open", "done": 0, "total": 2}],
    "thread_count": 2,
    "tower": {"name": "tower-fix-login", "status": "ready", "address": "t4"},
    "quiet_since_ms": null, "ready_since_ms": null, "auto_close_ms": null
  }],
  "quiet": [],
  "closed_today": ["write-docs"]
}
```

- `spaces`: every space that is not closed and not archived, in the order
  to show them: the open and ready ones first (those with `needs` > 0,
  then those with a session `working`, then the rest; then by name), then
  the quiet ones. `quiet`: the names of the quiet ones (the fold).
  `closed_today`: spaces closed since local midnight, oldest first; a
  closed space is in no other list.
- `state`: `open` (it has live sessions, or is new); `ready` (it has a
  goal, every request there is finished, and nothing waits for the user:
  the tower has written its `report` or is about to, and closes it by
  itself after `[spaces] auto_close_hours` left alone: `auto_close_ms`
  says when, null when it won't); `quiet` (no session left, since
  `quiet_since_ms`; archived a day later).
- A session's `status` is Herdr's word (`idle`, `working`, `blocked`,
  `done`, `error`); `account`, `worktree`, `branch`, `port` and `said`
  (its last words, cut to 240 characters) are null when not known.
- `requests`: what the user asked for there (the open ones, oldest first,
  then those finished today): `state` `open`, `done`, `closed` or
  `abandoned`; `done` of `total` tasks are finished. Only the user's: a
  message one session sent another is never a request nor a task of one
  (so a request made only of them isn't listed), and `done`/`total` count
  the request's own tasks. A request is listed in one space only, the one
  it was asked in (else that of its first task): the same request is never
  the current one of two spaces.
- `thread_count`: how many messages its thread holds in the tower's memory
  (at most 50): whether there is a thread to show.
- `tower`: the space's own tower (its own brain), or null when it has none
  and is due none. `status`: `ready` (it takes requests), `working`,
  `starting`, `blocked` (it waits on its own screen), `off` (due one that
  doesn't run right now). While it is not `ready` or `working`, a request
  asked in the space goes to the main tower. Show "its own tower" in the
  space's head when it is `ready` or `working`.
- `follow`: its results reach the user as they land (a receipt is told
  whatever the tell level). `mute`: what needs the user there stays in the
  space's own queue.
- `review`: each PR its sessions open gets a reviewer session ("PR
  review", under "PRs"). On for every space unless the user turned it off
  (on the Mac: the space's **Review PRs** switch, `tower space review
  <space> off`, or New space's **Review its PRs**); a tower from before
  sends no `review`: read that as on. There is no call to set it from a
  phone yet; a `missions` event carries a change.

`POST /v1/spaces/{space}/goal`, `/follow` and `/mute` answer with the
space's mission as it is right after the change (`mission`; null for a
space that is closed). A space gets a goal by its launch, by this call, or
on the Mac (`tower space goal`); `standing: true` keeps a space the tower
would otherwise close when its work is done.

### A space's thread

Sessions of a space write to each other without the tower in between
(`tower queue`, run by a session). What they wrote is the space's thread:

```
GET /v1/thread?space=fix-login&limit=50
→ 200 {"space": "fix-login", "more": false, "messages": [
    {"id": "h31", "at": 1794368771000, "from": "t3", "from_name": "fix login eyzflow",
     "to": "t5", "to_name": "fix login mediaroom", "text": "Which port does the api listen on?",
     "space": "fix-login", "delivered": true},
    {"id": "h34", "at": 1794368776000, "from": "t5", "from_name": "fix login mediaroom",
     "to": "t3", "to_name": "fix login eyzflow", "text": "3001. The redirect goes through /auth/callback.",
     "space": "fix-login", "delivered": true}
  ]}
```

Newest last. `limit` is 50 by default, 200 at most. `before` pages back
(the first message's `id`), `more` says older ones remain, as in the chat.
Without `space`: every space's. A message is listed when it arrives at its
receiver; `delivered: false` is one that could not be delivered (its
receiver was gone: the sender is told, the user is not). `id` is the
message's line in the history, like a chat message's. A space nobody wrote
in has an empty thread. A new message is a `thread` event:
`{"space": "fix-login", "message": {…}}`.

### Actions: launching a space, and closing one

`POST /v1/launch` with `dry_run: true` answers at once with the plan, and
does nothing:

```json
{"space": "fix-login", "goal": "Fix the login redirect", "done_when": "", "made": true, "dry_run": true, "agenda": null,
 "sessions": [{"name": "fix-login-eyzflow", "repo": "~/Work/eyz/eyzflow", "address": null, "account": "orca",
               "worktree": "/…/.tower-worktrees/fix-login/eyzflow", "branch": "fix-login/eyzflow",
               "port": null, "task": null, "error": null, "why": "the web app and its API"}],
 "repos_from": "setup",
 "assumed": {"setup": "eyzflow + mediaroom on orca", "setup_why": "the first of 2",
             "account": "orca", "account_why": "the setup's",
             "name": "fix-login", "name_why": "the goal's first two words, small ones left out",
             "worktrees": true, "worktrees_why": "one per repo, on a branch of its own"}}
```

It never asks: the space's name comes from the goal (or is `name`, taken as
given: a space that has that name already is joined, `made: false`), the
setup is the one named by `setup` (its whole name, or a part), else the
first, and `assumed` says what it chose and why. `goal` is the space's
goal: the ticket's title (the line after one that only asks for sessions,
else the first line, at most 120 characters); the whole text still goes to
the sessions. `done_when` is its "Acceptance criteria" (or "Definition of
done", "Done when") section's lines, joined, or "". Each session's `why`
is its repo's role in the project, or how it came in; `repos_from` is
`first-line`, `setup` or `repos`. Show the plan before launching ("what
the tower will do"). (Drafting a space from a ticket, `tower launch
--draft`, is the Mac's own for now: the phone launches as before.)

Launching for real opens a session per repo of the setup, each in a
worktree of its own: seconds each, sometimes minutes. Closing a space ends
its sessions and removes its worktrees. Neither holds the call open. Both
answer `202` at once with an **action**, and go on by themselves:

```json
{"action": {
  "id": "act_8eb036fb",
  "kind": "launch",                  // launch | close
  "space": "fix-login",
  "state": "running",                // running | done | failed
  "plan": { … },                     // a launch: the plan, as a dry run prints it; null for a close
  "receipt": null,                   // see below
  "error": null,                     // why it failed
  "by": "phone:d_187d916b",
  "started_ms": 1794368740438,
  "finished_ms": null
}}
```

- Follow it on the event stream (`action` events carry the whole action at
  each step), or ask `GET /v1/actions/{id}`. `GET /v1/actions` lists them
  after a reconnect. Actions are kept in the tower's memory: after a
  restart of the tower an id is `404 no_action` (what it did is in
  `GET /v1/missions`).
- **A launch**: `receipt` is null until the first session has opened;
  then it is the receipt so far, the plan's shape with `dry_run: false`:
  a session that has opened has its `address`, `port` and `task`, one that
  couldn't open has its `error`, the others are as planned. One `action`
  event per session. When all are through: `state: "done"`, and `receipt`
  is the whole one (`agenda`: the request the goal became). When no
  session opened at all: `state: "failed"`, `error`, and `receipt` says
  why for each. The new space is in the `missions` event as soon as its
  first session is there; open its view on `done`.
- **A close**: `receipt` is null while it runs. On `done`:
  `{"closed": name, "sessions_ended": [{"address","name"}], "sessions_left":
  [{"address","name","why"}], "herdr_space": "closed"|"kept"|"not shown",
  "worktrees_removed": [path], "kept_worktrees": [{"path","branch","repo","why"}],
  "requests_closed": [id], "tower_ended": [name], "report": "…"}`. A
  worktree with uncommitted work, or commits that are on no remote and no
  other branch, is kept and named with why: say so to the user. `failed`
  with `error` when it couldn't be closed (a session there started working
  meanwhile, say).
- One launch runs at a time: another `POST /v1/launch` meanwhile is
  `409 {"error": {"code": "launching", "message", "action": "<its id>"}}`
  (a second tap finds the first). A space that is being closed is
  `409 closing`, with `action` likewise.

What can be refused before anything starts is said in the call's own
answer, not in an action:

| Status | `code` | When |
|---|---|---|
| 400 | `empty` | No `goal` |
| 409 | `no_setups` | The tower has no setup yet (they are made on the Mac: `tower setup add`, or Settings) |
| 404 | `no_setup` | `setup` names none |
| 400 | `bad_launch` | The plan can't be made: a repo of the setup is no folder, its account isn't known, … (`message` says) |
| 409 | `busy` | A close without `force`, while a session there is `working` or `blocked`: `error.sessions` lists them (`{"address", "name", "status"}`). Ask the user, then send `{"force": true}` (their work in progress is cut off). |
| 404 | `no_space` | A close of a space nobody has |

### Acts: what the tower opened, and taking it back

A command gets a receipt, never a question: asked to open a session or a
space, the tower takes the defaults, acts, and keeps an **act**. A tower
that has them says `"acts"` in `features`.

```
Act { "id": "r3f9a2c1b7d04",                 // r and 12 hex digits
      "kind": "start" | "launch", "at_ms": 1794371414922,
      "by": "tower" | "tower-<space>" | "phone:<device>" | "cli" | "<a session's address>",
      "space": "M5 Pro" or null,
      "line": "Opened notes fix in M5 Pro.",  // what was opened, in one line
      "sessions": [ { "address": "t9", "name": "notes-fix", "label": "notes fix",
                      "worktree": path or null, "branch": name or null } ],
      "assumed": [ { "what": "account", "value": "orca", "why": "what \"second\" names" },
                   { "what": "model", "value": "default model", "why": "none was asked for: a session starts on its account's model" },
                   { "what": "folder", "value": "notes", "why": "the best match for \"notes\"" } ],
      "request": { "at": 1794371413527, "text": "open a session in the notes project on my second account" } or null,
      "state": "done" | "undone",
      "undo": { "possible": true, "why": null },
      "closing": null, "undone_ms": null }
```

- `assumed` is what the tower settled by itself or was told, for chips
  (`what`: `account`, `model`, `folder`, `worktree`, `space`, `name`; for a
  launch `setup`, `account`, `name`, `worktrees`); `why` is one phrase.
- `request` is the request it answered (its chat message's `at`), when the
  tower opened it on a turn the user began; null for one opened from a
  shell, a window, a phone, or on the tower's own account. Sessions opened
  one by one for the same request are one act.
- `undo.possible` is false once it is undone, and while a session of it
  was given more work than its first instruction (`why` says which and
  what). A worktree with changes is only found when the undo is tried.

**In the chat** an act that answered a request is one message, where it was
opened: `{"id": "h5", "role": "tower", "kind": "act", "text": the line (the
closing line once undone), "space": the conversation it was asked in or
null, "act": Act, "at", "updated_at", …}`. It changes in place (`chat`
event, `op: "update"`) when another session joins it, when it can no longer
be undone lightly, and when it is undone. The tower's own words follow as
its `answer` message. An act the tower no longer holds shows as it was,
with `undo.possible: false`. Every change is also an `act` event (`data`:
the Act). The phone's `state` doesn't carry the list.

**Undo**: `POST /v1/acts/{id}/undo`.

```
→ 200 {"act": Act (state "undone", closing set), "closing": "Closed it. Nothing else changed.",
       "sessions_ended": ["t9"], "worktrees_removed": [path], "kept_worktrees": [{"path", "why"}],
       "space_closed": false}
→ 409 {"error": {"code": "held", "message": "not undone: fix-login-eyzflow (t7) has work in its worktree that is nowhere else: 1 uncommitted change (notes.txt). Send {\"force\": true} to undo it anyway.",
                 "sessions": [{"address": "t7", "name": "fix-login-eyzflow", "why": "has work in its worktree that is nowhere else: 1 uncommitted change (notes.txt)"}]}}
→ 409 {"error": {"code": "undone", "message": "r… is undone already", "state": "undone"}}
```

It ends the sessions the act opened and removes the worktrees it made for
them (each only when nothing in it would be lost), or closes the space it
made when nothing else is in it. With `{"force": true}` the sessions are
ended although work would be lost; a worktree with changes is still kept,
and named in `kept_worktrees` and in `closing`. The call answers when it is
done (a few seconds: it waits for the sessions' programs to end). A launch
the phone started names its act in the action's receipt (`receipt.act`).

### Make it a rule

After a decision is answered, the user can turn the answer into a standing
rule with one tap. The answer to `POST /v1/decisions/{id}/answer` says
whether that is offered, and what would be asked:

```
"rule": { "can": true, "text": "Make this a rule: when a session in Shop asks \"Tabs or a sidebar for settings?\", answer \"A sidebar\" without asking me." }
"rule": { "can": false, "text": null }
```

It is offered only for a `question` or a `choice` answered in words
(`{"choice": n}` or `{"other": "…"}`; a question answered step by step with
`answers` says `can: false`, since the tower doesn't know its words), never
for a `merge`, a `permission`, a `failed` task or a plain `raised`, and
never for a deploy, a delete, production data, secrets or money.

`POST /v1/decisions/{id}/rule` (body optional: `{"answer": "…"}`, the
answer in words when the app knows it and the tower doesn't) asks the tower
of that decision's space exactly as `POST /v1/ask` with `space` would, with
that text: `202 {"sent": text, "space": name or null}`. The tower adds a
behavior, which arrives as a `behaviors` event and a `behavior` card in the
chat with what changed, and can be undone there. `409 not_answered` (it is
still open), `409 no_rule` (none is offered for it), `404 no_decision`,
`503 brain`.

### How much the tower tells

`GET /v1/tell`, `POST /v1/tell {"level": …, "voice": …}`; `tell_level` and
`tell_voice` in the state.

| `voice` | It says out loud |
|---|---|
| `off` | Nothing (the mute: `muted` in the state is true) |
| `decisions` | A decision, once, in one sentence. Nothing else: no answer, no receipt |
| `replies` (the default) | Its answers to what the user asks too |

It applies to what a phone plays as the tower's voice as well: at
`decisions` no `reply` or `ack` clip is sent, only the `alert` of a decision.

| `level` | Reaches the user |
|---|---|
| `decisions` (the default) | Decisions, and direct replies to what they asked |
| `results` | Also one receipt when a request of theirs is finished |
| `everything` | Every start, finish and report, as before the Quiet Tower |

Setting it takes effect at once (the tower's brains read their instructions
again). It changes what is told from then on, never what the chat showed
before.

### The brains' model

`GET /v1/model` (also `brain_model` in the state):

```json
{
  "program": "claude",               // or "codex"
  "model": "claude-opus-5-5",        // the setting
  "effort": "high",                  // low | medium | high | xhigh | max
  "choices": {"models": [{"id": "claude-fable-5-2", "label": "Fable 5.2"}, {"id": "claude-opus-5-5", "label": "Opus 5.5"},
                         {"id": "claude-sonnet-5-5", "label": "Sonnet 5.5"}, {"id": "claude-haiku-5-5", "label": "Haiku 5.5"}],
              "efforts": ["low", "medium", "high", "xhigh", "max"]},
  "running": [{"brain": "tower", "space": null, "address": "t7", "model": "sonnet", "effort": "medium", "restart": true}],
  "restart_needed": true,
  "locked_by_args": false,
  "locked": {"model": null, "effort": null},
  "says": "The tower's brain runs on sonnet at medium effort. Opus 5.5 at high effort applies when it restarts.",
  "restarted": []
}
```

One setting for every tower brain, the main one and each space's own. Draw
the picker from `choices`; show `says`. A change applies when a brain
starts, so after `POST /v1/model {"model": …}` the report has
`restart_needed: true` and each of `running` whose `restart` is true still
runs on what it was started with (`model` and `effort` there are null when
its command line names none). `POST /v1/model {"restart": true}` (alone, or
with the change) restarts every brain that runs, now: `restarted` lists
them, each starts a new conversation, and for a few seconds a space's
`tower` is `off` or `starting`. `locked_by_args`: the user's own
`claude_args` in config.toml set the model or the effort (`locked` says
which), and that wins over the choice here: show it, and that the choice is
kept. `model` may be a picker id, a picker label ("Opus 5.5") or any name
the program takes. `409 codex` when the tower's brain is Codex (its model
comes from its own settings; nothing is changed).

## Live updates: `GET /v1/events`

Server-Sent Events (`text/event-stream`), one long response. Use OkHttp's
`okhttp-sse` `EventSource` with the `Authorization` header; reconnect with
backoff (1, 2, 5, 10 s) when it drops. Every event has `id`, `event` and one
line of JSON `data`. A comment line `: ping` comes every 10 s.

| `event` | `data` |
|---|---|
| `hello` | `{"api":1,"device":"d_…","voice":bool,"features":["quiet","acts","pairing","bonjour","reviews"],"hosts":[Host],"network":{…}\|null,"magic_dns":…,"bonjour":{…}\|null}`, first: `hosts`, `network`, `magic_dns` and `bonjour` as `GET /v1/network` |
| `hosts` | `{"hosts": [Host], "network": {…} \| null, "magic_dns": …, "tailscale": {…}, "bonjour": {…} \| null}` (as `GET /v1/network`): the tower's addresses changed (this Mac moved to another network, Tailscale started or stopped, the network was named, Bonjour advertised it under another name or stopped). Merge `hosts` into the stored addresses. |
| `state` | the whole state, first right after `hello`, then whenever something in it changes (at most 4 a second). A change to the decisions, the missions or a thread alone sends their own event, not this one. |
| `decisions` | `{"count": n, "decisions": [Decision]}`: every open decision in queue order, each with `muted`; `count` is how many are outside muted spaces (the badge). Right after the first `state`, then whenever the list changes: one opened, closed or changed, or the order moved (the tower orders them again every 30 s). See "Notifying". |
| `missions` | the answer of `GET /v1/missions`, right after the first `decisions`, then whenever it changes (a session's status or last words, a goal, follow, mute, a request, a space's tower, a space opening, going quiet or closing) |
| `thread` | `{"space": name or null, "message": {…}}`: a session wrote to another (`message` as `GET /v1/thread` lists it); it follows that message's own `history` event |
| `act` | an Act (see "Acts"): what the tower opened was kept, another session joined it, it can no longer be undone lightly, or it was undone |
| `action` | an action (a launch or a close a phone started), at each step: when it begins, as each session of a launch opens, and when it is over (see "Actions") |
| `history` | one new history entry (as in `/v1/history`), as it's written |
| `pr` | `{"op": "open"\|"update"\|"closed", "pr": {…}}`: a PR a session opened appeared, changed (checks, mergeable, waiting) or closed (merged or closed). |
| `question` | `{"op": "open"\|"update"\|"closed", "id", "question": {…}}`: a question set appeared, changed (recommendations arrived, the dialog moved to another tab, answering started, finished or stopped) or closed (the session moved on). |
| `behaviors` | a change: `{at, op, id, title, by, undid, diff}` (the chat also gets a `behavior` card with the diff) |
| `chat` | `{"op": "add"\|"update", "message": {…}}`: a message joined the thread (from the Mac or any phone), or one changed (a screenshot card grew). Messages the phone sent itself come back this way too. |
| `audio` | `{"id":"a17-9c2e1a0b4d3f5e6a","kind":"reply"\|"alert"\|"ack"\|"cue","text":"…","ms":1840,"url":"/v1/audio/a17-9c2e1a0b4d3f5e6a"}`: on voice streams for what the voice says, and on every stream of the phone a turn of its own is for (see "Where replies are heard") |
| `hush` | every stream: stop playing and drop everything queued |
| `pause` / `resume` | every stream: someone is talking at the Mac (hold playback, then go on) |

`?voice=1` makes this phone **the tower's voice** for as long as the stream
is open (see below). Without it, the stream is just the panel's feed.

## Voice

### Choice: whole recordings up, sentence clips down, all over HTTPS

- **Up: one WAV per take.** The phone records while the user holds the talk
  button (or until it hears a pause), then `POST /v1/voice` with the whole
  recording. Whisper on the Mac transcribes complete takes anyway (that's how
  the Mac's own press-to-talk works), so streaming the microphone would add
  a protocol and no speed: a 5-second take uploads over Wi-Fi in tens of
  milliseconds and transcribes in about half a second.
- **Down: one WAV clip per sentence, announced on the event stream.** The
  tower already speaks a sentence at a time (Kokoro makes the next while one
  plays). When a phone is the voice, each finished sentence (and the canned
  acknowledgement, and the alert bell) becomes a clip: an `audio` event
  tells the phone, which fetches `GET /v1/audio/{id}` and plays clips in the
  order the events came. The first words reach the phone as fast as they'd
  play on the Mac.
- Plain HTTPS requests and one SSE stream: no WebSocket, no RTP, no codecs to
  negotiate; it works through anything that carries HTTPS, it's easy to
  retry, and it reuses the same auth and TLS pin. WAV is large-ish
  (48 KB/s at 24 kHz) but trivial on a LAN, and needs no encoder on either
  side. (Opus/OGG can be added later behind `Accept` without changing the
  flow.)

### Speaking into the phone

```http
POST /v1/voice
Authorization: Bearer htk_…
Content-Type: audio/wav
Content-Length: 160044

<RIFF WAVE: 16-bit PCM (or 32-bit float), mono or stereo, 8 to 96 kHz; 16 kHz mono preferred>
```

```json
200 {"heard": "what's everyone doing", "sent": true}
200 {"heard": "", "sent": false}     // silence, noise, or a stop word ("stop": it hushes the tower)
415 {"error": {"code": "bad_audio", …}}
400 {"error": {"code": "bad_length", …}}   // under 0.2 s or over 120 s
```

**In a space's view**: `POST /v1/voice?space=<name>` (the name in any case,
URL-encoded). What was heard goes to that space's own tower, as `POST
/v1/ask` with `space` does (to the main tower, with the space named in
front, while the space has none that can take it), and the answer adds
`"space": name` (its whole name). A name no space has is `404 no_space`,
before anything is transcribed or saved. Its reply is in that space's
conversation (see "Whose conversation"). `space` means nothing to
dictation (`send=0`, `/v1/transcribe`).

**With files**: send `multipart/form-data` instead, the WAV as a part named
`audio`, and up to 6 file parts as for `/v1/ask`. The transcript gets
`[Image #N]` markers in front and the paths attached, as above;
the answer adds `"attachments": [paths]`. (A stop word with images is sent,
not taken as "stop".)

### Dictation: transcribe without sending

`POST /v1/transcribe` (or `POST /v1/voice?send=0`) takes the same WAV and
returns `200 {"heard": "…", "sent": false}`; nothing reaches the tower, and a
stop word is just text. It uses the same Whisper model and vocabulary (the
session names) as the panel's dictation. For **text while the user is still
talking**, do what the panel does: every second or so, post the recording so
far to `/v1/transcribe` and show what comes back, then post the whole take at
the end (to `/v1/voice` to send it, or `/v1/transcribe` to put it in the text
box). Keep one request in flight at a time: the Mac transcribes one
recording at a time, and a 30-second take takes about a second.

The Mac resamples to 16 kHz, transcribes with the same Whisper model and
vocabulary as the Mac's microphone, and hands the text to the tower as a
`[voice]` message, exactly as press-to-talk does. The call returns after the
transcription (about a second); the reply comes on the event stream. On
Android: `AudioRecord` at 16000 Hz, `CHANNEL_IN_MONO`, `ENCODING_PCM_16BIT`;
write a 44-byte WAV header in front.

### Where replies are heard: the phone's `audio_route`

Each paired phone has a setting, **`phone`** (the default) or **`mac`**: where
the replies to what *it* sends (asks, voice takes, approve/refuse) are heard.
Change it with `PUT /v1/device/audio`, in the Mac panel's Phone section, or
`tower phone route <device id> phone|mac`. It is kept on the Mac
(`devices.json`), so it survives restarts.

- **`phone`**: for a turn the phone started, its "Okay." and every sentence
  of the reply go to **that phone only**, as `audio` clips on **every
  stream it has open** (a plain `GET /v1/events` is enough; `voice=1` isn't
  needed), and **the Mac stays silent** for that turn. If the phone has no
  stream open, the clips are not played anywhere: they are **kept 15
  minutes** (the last 200) and listed by `GET /v1/audio?after=<last id>`, so
  the app can play what it missed when it reconnects, or skip it (the text
  is in the history either way).
- **`mac`**: the phone sends, the Mac speaks the reply as if it was asked on
  the Mac; the phone shows the text (history and `state.said`).
- Turns started on the Mac (its microphone, the panel, `tower ask`) and
  alerts: spoken on the Mac, unless a phone with route `phone` has a
  `voice=1` stream open, which takes the voice as before (a route-`mac`
  phone's voice stream doesn't).
- `hush`, `pause` and `resume` go to every open stream.
- **Voice on this Mac** (`voice_on_mac`; `tower mac-voice on`, or the
  panel's Settings > Voice): everything is spoken on the Mac and no phone
  gets an `audio` clip, whatever its route or `voice=1` stream. Off by
  default; `state.voice_on_mac` says which.

### The tower speaking on the phone

- While **any** stream with `voice=1` is open (of a phone whose route is
  `phone`), everything the tower says goes to the phones as `audio` clips
  and **the Mac stays silent**. When the last
  voice stream closes (the app disconnects, the API is turned off, or the
  network drops: noticed within about 30 s, when what was sent stays
  unacknowledged), the Mac speaks again. A clip already
  sent when the phone vanished is not replayed on the Mac.
- Clip kinds: `ack` (the canned "Okay." the moment a request reaches the
  tower), `reply`, `alert` (news nobody asked for), `cue` (the soft bell
  before an alert, `text` empty). Play them in event order, gapless.
- Clips are WAV, 24 kHz, mono, 16-bit, kept 15 minutes (or the last 200).
  A clip's id is opaque (in order, with a random part): take it from the
  `audio` event or `GET /v1/audio`. A clip for another phone's turn is
  `404` to this one, and so is any clip while the tower is muted. Fetch
  each with the bearer token; play from memory (`AudioTrack` in
  `MODE_STREAM`, or `MediaPlayer` with a `MediaDataSource`), since Android's
  players can't add headers to an HTTPS URL by themselves.
- `hush`: stop now and drop the queue (the user pressed hush, or started
  talking at the Mac). The phone should also call `POST /v1/hush` itself
  when the user starts a take on the phone, so the tower stops talking over
  them.
- `pause` / `resume`: someone is holding the Mac's talk key; hold playback.
- Keep the voice stream in a foreground service while the app is the voice,
  so Android doesn't kill it with the screen off.

## Versioning

Everything is under `/v1`. Additions (new keys, events, calls) don't change
the version; the app must ignore what it doesn't know. A breaking change gets
`/v2`, and `GET /v1/hello`'s `api` says what the tower speaks.

What a tower has beyond the first `/v1` is named in **`features`** (a list
of words, in `GET /v1/hello`, in `POST /v1/pair`'s answer and in the
stream's `hello` event). A tower from before there were features sends no
`features`: read that as `[]`.

| Feature | The tower has |
|---|---|
| `pairing` | Pairing on more than one network: `install_id` and `repaired` on `POST /v1/pair` (and `hosts`, `network`, `magic_dns` in its answer), `GET /v1/network`, `hosts`, `network` and `magic_dns` in the stream's `hello`, the `hosts` event, `PATCH /v1/device`, and `network` and `phone_hint` in the state |
| `bonjour` | Found by name on the local network: the `_herdr-tower._tcp` advert with the certificate's `fp` in its TXT, and `bonjour` in `GET /v1/network`, `GET /v1/hello`, `POST /v1/pair`'s answer, the stream's `hello` and the `hosts` event (see "Finding the Mac by Bonjour") |
| `acts` | Acts and what came with them: `GET /v1/acts`, `/v1/acts/{id}`, `POST /v1/acts/{id}/undo`, the chat's `kind: "act"` messages and the `act` event; `rule` in a decision's answer and `POST /v1/decisions/{id}/rule`; `voice` in `/v1/tell` and `tell_voice` in the state |
| `reviews` | PR review: `review` on a PR (`GET /v1/prs`, the `pr` event, a chat chip's `pr`, a merge decision's `prs[]`), `review` on a mission, `reviews` in the state, and a review that isn't agreed among a merge's `warnings` |
| `quiet` | Everything under "The Quiet Tower": the `/v1/decisions`, `/v1/missions`, `/v1/spaces/{space}/…`, `/v1/thread`, `/v1/setups`, `/v1/launch`, `/v1/actions`, `/v1/tell` and `/v1/model` calls; `space` on `POST /v1/ask`, on chat messages, and as a filter of `/v1/history` and `/v1/chat`; the `decisions`, `missions`, `thread` and `action` events; `decisions_count`, `tell_level`, `brain_model` and `brains` in the state |

Both ways round:

- **A newer app, an older tower** (no `quiet`): none of those calls exist
  there (`404 not_found`), and the app works as before: what needs the user
  is the escalated question sets (`GET /v1/questions`, the `question`
  event) and the waiting PRs (`GET /v1/prs`, the `pr` event), and there is
  one conversation. Check `features` once after connecting, not call by
  call.
- **An older app, a newer tower**: every call, key and event it knows is
  as it was. It gets events it doesn't know (`decisions`, `missions`,
  `thread`, `action`) and keys it doesn't know (`space` on a message,
  `features`), and ignores them. Two things it may notice: at the tell
  level `decisions` (the default) the tower writes most news down instead
  of telling it, so fewer `event` messages reach the chat (they are in
  `/v1/history` with `record: true`); and `state` events come only when
  something in the state changed.
