# Team mode in 10 minutes

For coordinators behind NAT, Brain ED can use a company-hosted relay with independent relay tokens and end-to-end coordinator TLS. Direct TLS remains available; relay-only join bundles use v2. See the [relay setup and limits (RU)](relay.md). Run `tools/team-demo.sh --via-relay` for a disposable loopback process demonstration. P2P/Iroh and public NAT deployment validation are outside E7.7.

The Brain task coordinator lets several machines work on one task board over TLS:
one machine runs the coordinator (`net serve`), the others join with a bundle the
owner issues (`net enroll` → `net join`). Tasks, claims and leases live in the
coordination database; every principal has its own role and spaces.

Everything below was verified on one machine: the coordinator and the client run
in **separate temporary HOMEs**, and the real command output is quoted verbatim.
The credential token is written only to the credential file (0600) and is never
printed — none of the outputs below contain it.

Notation: `H1` is the coordinator's HOME, `H2` is the client's HOME, `brain` is
the built binary. Ports in the examples are the actual run's values; yours will
differ.

---

## 0. Build the binary

```console
$ CARGO_TARGET_DIR=$HOME/projects/brain-ed-target/GLM-guide cargo build -p brain-cli
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 15s
```

## 1. Initialize the brain and the coordinator's network state

```console
$ HOME=$H1 brain --offline init
{
  "database": "…/coordinator/.brain/brain.sqlite",
  "schema_version": 5,
  …
}

$ HOME=$H1 brain net init --state-dir $H1/.brain/net --name localhost --ip 127.0.0.1
{
  "ca_sha256_fingerprint": "0d6cf6a110392ab34f72cc17c75d1ef94df9a88a74c44b711d107cdb5b55cbc1",
  "created": true,
  "files": {
    "ca_key": "…/coordinator/.brain/net/tls/ca.key",
    …
```

`--name` and `--ip` go into the server certificate's SAN. For a second machine
across the network add its DNS name (`--name laptop.local`) or IP (`--ip
192.0.2.10`); documentation examples only use reserved ranges (192.0.2.x,
100.64.0.x).

## 2. Start the coordinator

`net serve` runs while its standard input stays open. In a terminal just keep the
window open; in a script keep the pipe open:

```console
$ sleep infinity | HOME=$H1 brain net serve --state-dir $H1/.brain/net
{"bind":"127.0.0.1:36401","deadline_ms":5000,"lan":false,"protocol":"brain-coord/1",
 "state_dir":"…/coordinator/.brain/net","status":"serving","stop":"close standard input",
 "transport":"tls (rustls; TLS 1.2/1.3)"}
```

The `bind` line is the actual address (the port is any free one; pin it with
`--port 7443`). Stop the server by closing stdin (Ctrl-C works too).

Is the port already taken? It is now an understandable error with a category, no
paths, no secrets:

```console
$ sleep 1 | HOME=$H1 brain net serve --state-dir $H1/.brain/net --port 36401
{
  "category": "address_in_use",
  "error": { "detail": { "error": "address_in_use", "message": "address_in_use" }, "http_status": 503 },
  "status": "not_serving"
}
```

## 3. Issue bundles: a worker and a reader

`--role worker` reads and writes, `--role reader` is read-only. `--grant-space`
is required: a principal only sees its own spaces. `--dial` lists the addresses
the client will use to reach the coordinator (they go into the bundle); the
default is an empty list, so state it explicitly on a stand.

```console
$ HOME=$H1 brain net enroll --state-dir $H1/.brain/net \
    --machine laptop-two --agent-id claude --role worker --grant-space team \
    --label "second laptop" --dial 127.0.0.1:36401 \
    --credential-out $T/worker.cred.pem --bundle-out $T/worker.bundle
{
  "bundle": "…/worker.bundle",
  "bundle_server_name": "localhost",
  "capabilities": ["work_board", "work_show", "work_claims", "work_add", "work_depend",
                   "work_claim", "work_claim_path", "work_renew", "work_release",
                   "work_set_state", "work_handoff", "work_verify", "work_whoami",
                   "share_pull", "share_query"],
  "credential_file": "…/worker.cred.pem",
  "expires_at": 1800803997,
  "principal": { "agent": "claude", "label": "second laptop",
                 "machine": "laptop-two", "role": "worker", "spaces": ["team"] },
  "secret": "written to credential_file (0600) only",
  "status": "ok",
  "token_id": "t17930279975e61c7dcb2fd93a8e3814461"
}
```

The same for a reader:

```console
$ HOME=$H1 brain net enroll --state-dir $H1/.brain/net \
    --machine htpc --agent-id codex --role reader --grant-space team \
    --dial 127.0.0.1:36401 \
    --credential-out $T/reader.cred.pem --bundle-out $T/reader.bundle
```

`token_id` is not a secret: it is printed and it is what `net revoke` needs. The
token itself lives in `worker.cred.pem`; the bundle `worker.bundle` carries the
same credential plus the CA and the dials.

## 4. Move the bundle and join

Move the bundle file any way you like (scp, a flash drive, a secret manager).
Keep it at 0600 — its contents are equivalent to the machine's password.

```console
$ chmod 600 worker.bundle && scp worker.bundle user@laptop-two:
$ HOME=$H2 brain net join ~/worker.bundle
{
  "bundle_consumed": false,
  "client_dir": "…/client/.brain/net/client",
  "dial": [ "127.0.0.1:36401" ],
  "note": "net status and net call now work without --dial/--ca/--server-name/--credential",
  "previous_client_dir": null,
  "refused_dials": [],
  "server_name": "localhost",
  "status": "ok"
}
```

After `net join` the `--dial/--ca/--server-name/--credential` flags are no longer
needed — the client reads them from `endpoint.json`.

**If the machine goes through Tailscale or another overlay network**, its address
falls under the dial policy and the bundle will refuse it without an explicit
opt-in: IPv6 ULA counts as LAN (`--allow-lan-dial`), CGNAT (RFC 6598) counts
as CGNAT (`--allow-cgnat-dial`):

```console
$ HOME=$H2 brain net join ~/worker.bundle --allow-lan-dial --allow-cgnat-dial
```

Out-of-policy dials are rejected one by one: the allowed ones land in
`endpoint.json`, the rejected ones in `refused_dials` in the output; if all of
them are rejected nothing is written.

## 5. Check the connection and work the board

The space is the global `--space` flag (here `team`, the one from
`--grant-space`):

```console
$ HOME=$H2 brain net status
{
  "capabilities": [ "work_board", … "work_whoami", "share_pull", "share_query" ],
  "endpoint": { "dial": "127.0.0.1:36401", "server_name": "localhost", "tls": true, … },
  …
}

$ HOME=$H2 brain --space team net call add inbox "Triage the inbox" --request-id demo-1
{ … "task": { "id": 1, "state": "queued", "created_by": {"agent": "claude", "machine": "laptop-two"}, … } }

$ HOME=$H2 brain --space team net call claim 1 --request-id demo-2
{ … "claim": { "claim_id": 1, "agent": "claude", "expires_in_secs": 900, "fence": 1, … } }

$ HOME=$H2 brain --space team net call board --state active
{ … "counts": { "active": 1, … }, "tasks": [ { "title": "Triage the inbox", "owner": {…} } ] }

$ HOME=$H2 brain --space team net call show 1
{ … "task": {…}, "recent_events": [ { "kind": "created", … } ], "live_claims": […] }

$ HOME=$H2 brain --space team net call state 1 1 completed --note "done" --request-id demo-3
```

`--request-id` makes `add`/`claim`/`state` idempotent: retrying with the same id
never duplicates (safe to retry after a timeout). `state` also needs the `fence`
number from the `claim` answer — protection against overwriting someone else's
live claim.

## 6. A reader reads but cannot write

Join the reader bundle on the same client (replacing requires `--replace`; the
previous credential is kept aside as `.previous`):

```console
$ HOME=$H2 brain net join ~/reader.bundle
{ "error": { "code": "invalid_config", "message": "…/net/client already holds a joined endpoint;
  pass --replace to swap it (the old one is kept as .previous); nothing was written" }, … }

$ HOME=$H2 brain net join --replace ~/reader.bundle
{ … "previous_client_dir": { … }, "status": "ok" }

$ HOME=$H2 brain --space team net call board      # fine: a reader may read
{ … "principal": { "label": "", "machine": "htpc" }, "result": { "counts": {…} } }

$ HOME=$H2 brain --space team net call add inbox "Read-only, refused"
{ "error": { "detail": { "error": "forbidden", … } }, "status": "refused" }   # exit code 4
```

## 7. Revoking access

One credential, or a whole machine:

```console
$ HOME=$H1 brain net revoke --state-dir $H1/.brain/net --token-id t1793028034d4a908a5ceb366f64ead13ee
{ "effective": "next request; no cached replay is served",
  "revoked": { "token_id": "t1793028034d4a908a5ceb366f64ead13ee" }, "status": "ok" }

$ HOME=$H1 brain net revoke --state-dir $H1/.brain/net --machine htpc
{ "effective": "next request; no cached replay is served",
  "revoked": { "all_agents_and_credentials": true, "machine": "htpc" }, "status": "ok" }

$ HOME=$H2 brain net status
{ "error": { "detail": { "error": "unauthorized", "http_status": 401 }, … } }   # revoked
```

The principal list (no secrets, no hashes): `net principals --state-dir $H1/.brain/net`.

## 8. Rotating certificates

`--rotate` rewrites the CA and the server certificate; restart the server and
re-enroll the machines:

```console
$ HOME=$H1 brain net init --state-dir $H1/.brain/net --name localhost --ip 127.0.0.1 --rotate
{ "created": false, "rotated": true, … }

# restart net serve, then:
$ HOME=$H1 brain net enroll … --bundle-out $T/worker2.bundle   # a fresh bundle
$ HOME=$H2 brain net join --replace ~/worker2.bundle
{ … "dial": [ "127.0.0.1:36401" ], "status": "ok" }

$ HOME=$H2 brain --space team net call whoami
{ … "principal": { "agent": "claude", "machine": "laptop-two", "role": "worker", "spaces": ["team"] } }
```

---

## Common errors

| What you see | Why | What to do |
|---|---|---|
| `"category": "address_in_use"` from `net serve` | the port is taken by another process (or a second coordinator is already up) | pick another `--port` or stop the previous server |
| `bundle dial list is empty; nothing was written` | `net enroll` was called without `--dial` | pass the coordinator's address: `--dial 192.0.2.10:7443` |
| `already holds a joined endpoint; pass --replace` | the client already has a joined connection | `net join --replace …` (the old one is kept as `.previous`) |
| `the bundle's credential has expired; enroll the machine again` | the bundle sat longer than the credential TTL (90 days by default, `--ttl-secs`) | issue a fresh bundle: `net enroll` |
| a `tls_failure` from `net status`/`net call` | the `--server-name` does not match the certificate SAN (verified with `wrong.host`) | use the name from `endpoint.json` or reissue the certificate with the right `--name` |
| `"error": "forbidden"` | the role does not allow the operation (a reader writing) | use a worker for writes |
| `"error": "unauthorized"` (HTTP 401) | the credential was revoked (`net revoke`) or the machine disabled | issue a fresh bundle: `net enroll` |

## Where to go next

- Help for any command: `brain net --help`, `brain net call --help`.
- Roles and capabilities: `worker` reads and writes, `reader` reads only; the
  capability list is visible in `net status`.
- The local board without the network: `brain work …` (the same coordination
  database).
