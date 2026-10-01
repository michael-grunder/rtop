# rtop

`rtop` is a terminal UI for monitoring Redis/Valkey instances.

## Implemented MVP

- Polls one or more Redis targets every second (default, configurable)
- Starts immediately and runs Redis/Valkey autodiscovery in the background
- Overview screen with:
  - generic, configurable columns (INFO-backed + calculated)
  - defaults for alias/address/type/memory/ops/latency/status plus a cluster/replication color gutter, with `Type` auto-hidden in `Tree` view and host auto-hidden when all targets share one host
  - available optional columns including `connected_clients` and `master_repl_offset` (`INFO replication` / `master_repl_offset`)
  - available optional cluster slot coverage columns `slots_total` (`#Slots`) and
    `slots` (`Slots`, the full comma separated range list such as
    `0-5460,9000`), both sourced from `CLUSTER SHARDS` and populated only for
    cluster primaries; replicas and non-cluster instances leave them blank
- Detail screen with summary, latency, raw `INFO`, `INFO COMMANDSTATS`, an on-demand `bigkeys`
  view, and a timed `hotkeys` view for CPU/NET sampling, including full
  server-reported error details when polling fails
- Tree, flat, and primary-only overview modes
- Sorting by currently visible column keys and substring filtering
- Kill picker on `k` with Redis `SHUTDOWN` and local signal options
- Credential form on `a` for authenticating the selected server without
  restarting `rtop`
- Bottom status/key bar with mnemonic shortcut labels and live search/filter input echo
- Live discovery status in the footer, including queued/probing/verified counts
- Config loading from TOML + CLI target merge
- Handles per-instance failures without crashing UI
- Surfaces richer instance states such as `PROTECTED`, `AUTH`, `LOADING`, and `DOWN`

## Key Bindings

- `q`: quit from the overview, or close the active overlay window
- `Ctrl+C`: quit immediately
- `H`: open full help page
- `f` or `/`: edit the overview filter, keeping existing text
- `t`: cycle Tree / Flat / Primary (overview)
- `s`: open Sort By to choose from currently visible overview columns
- `c`: open the overview column picker for toggling and reordering columns
- `Space`: toggle selection of the focused overview server
- `a`: enter one username and password and retry all selected servers
- `k`: open the kill picker for selected servers; `Enter` chooses the action,
  with an additional confirmation when stopping more than one server
- `?`: toggle help
- `Up/Down`: move focus in overview, or scroll the active detail pane when it has more rows than fit
- `Enter`: open detail for the focused server
- `Esc`: clear server selections in the overview, or quit if none are selected;
  close the active overlay window, go back from detail/help, or stop filter editing
- `Tab` / `Left` / `Right`: cycle detail tabs
- `S` / `L` / `I` / `C` / `B` / `K`: jump to `Summary` / `Latency` / `Info Raw` / `Commandstats` / `Bigkeys` / `Hotkeys` in detail view
- `Shift+Up/Down`: reorder columns inside the overview column picker
- `h`: toggle host rendering (default auto-hides host when all targets share one host)
- `/`: start filter input in overview, or filter the active detail pane in detail view (`Summary`, `Latency`, `Info Raw`, `Commandstats`, `Bigkeys`, or `Hotkeys`)
- `C` / `N`: start CPU or NET sampling while the `Hotkeys` tab is open
- `X`: stop active `Hotkeys` sampling early, or reset the `Hotkeys` pane back to its idle prompt
- `r` / `R`: refresh now, rerun the on-demand `Bigkeys` scan, or rerun `Hotkeys` sampling for the last selected metric while that tab is open

Overview shortcuts `a`, `f`, `t`, `s`, `c`, and `k` also accept uppercase
letters. While editing a filter or credentials, letters are entered as text.
Detail tab shortcuts keep their existing meanings.

Function keys remain available as aliases: `F1` help, `F5` view mode, `F6`
Sort By, `F7` columns, `F8` auth, and `F9` kill. `v` also opens columns.
`F3` starts overview search input; `F4` starts filtering with an empty filter.

Use `Space` to select or deselect servers. The caret gutter shows `●` for a
selected server, `>` for the focused server, or a bold `▶` when the focused
server is also selected. Other rows leave the gutter blank, and the overview
title shows the selected count. Selections persist across refreshes,
sorting, filtering, and view changes, including servers temporarily hidden by
the filter or Primary view. Auth and kill act on all selected servers, falling
back to the focused server when none are selected. Tree, Sort By, and Columns
always apply globally; detail view still opens the focused server.
Press `Esc` in the overview to clear all selections, including hidden servers;
press it again to exit. This also applies when only one server is selected.

When discovered servers show `AUTH`, select them and press `a` to try the same
credentials on each selected server. The username
defaults to Redis' `default` user and may be cleared for password-only
authentication. The password is masked while it is entered, and the submitted
credentials are kept only for the current process. For compatibility with
Redis versions before 6.0, `rtop` first tries `AUTH default <password>` and
retries with `AUTH <password>` when the server reports that the ACL-style form
is unsupported. A non-default username is sent only with the ACL-style form.

The `k` kill picker offers `SHUTDOWN SAVE`, `SHUTDOWN NOSAVE`, `SIGINT`,
`SIGTERM`, `SIGQUIT`, and `SIGKILL`. The Redis shutdown commands work over the
current connection, while the signal-based options require a local TCP or Unix
socket target plus `process_id` from `INFO server`.
For multiple servers, choosing an action opens `Stop <N> servers with <how>?`;
press `Enter` again to confirm, or `Esc`/`q` to cancel. Each server is attempted
independently, so one failure does not prevent attempts on the others.

The `Bigkeys` detail tab mirrors `redis-cli --bigkeys`: it scans the keyspace with
`SCAN`, fetches each key's type, runs the matching cardinality/length command
(`STRLEN`, `LLEN`, `SCARD`, `ZCARD`, `HLEN`, `XLEN`), and shows the largest keys
found. The `Length` column shows that type-specific cardinality/length value,
while `Memory` shows a humanized `MEMORY USAGE` estimate when supported. Unlike
normal polling, this scan is
performed on demand when the `Bigkeys` tab is opened or refreshed. The header
shows when a scan is in progress, and after completion it shows the result age
in seconds.

The `Hotkeys` detail tab uses Redis `HOTKEYS START ... DURATION 60` so sampling
always stops automatically even if the TUI exits mid-run. The pane starts in an
idle prompt where you can choose `CPU` or `NET` sampling, shows a live
countdown while tracking is active, lets you stop early with `X` by issuing
`HOTKEYS STOP`, and then fetches `HOTKEYS GET` results into a filterable,
scrollable table with per-key share percentages. After completion, `C`/`N` can
start a fresh run, `R` reruns the last metric, and `X` resets the pane back to
its idle prompt.

## CLI

Examples:

```bash
rtop 127.0.0.1:6379 127.0.0.1:6380
rtop 6379 6380
rtop
rtop 192.168.0.148
rtop --autodiscover 192.168.0.148 --autodiscover 192.168.0.149
rtop 6379 --autodiscover
rtop 6379 --autodiscover 192.168.0.148
rtop --unix /tmp/redis.sock --tcp 10.0.0.12:6379
rtop --cluster 7000
rtop --cluster 10.0.0.11:7000 --cluster 10.0.0.12:7000
rtop --once
rtop --output json
rtop --output json --once
rtop --autodiscover 10.0.0.12 --once
rtop --config ~/.config/redis-top.toml
rtop -c config.toml 127.0.0.1:6379
```

For TCP targets, you can pass just a port (for example `6379`), and it is treated as
`127.0.0.1:6379`.

Host-only positional values such as `192.168.0.148` are treated as autodiscovery
hosts, not fixed monitored instances. Exact TCP targets such as `6379` or
`192.168.0.148:6380` disable autodiscovery by default and only connect to the
requested server(s).

When you provide explicit targets, `rtop` does not also add unrelated
`[[targets]]` entries from `redis-top.toml`. If an explicit target matches a
configured TCP or Unix target, `rtop` still reuses that target's context
such as alias, username, password, and tags.

If you do not provide explicit targets, `rtop` autodiscovers on `127.0.0.1`
by default. `--autodiscover[=<HOST>]` opt back into autodiscovery when you also
provide exact targets. With no value it autodiscovers on localhost; with a
value it probes the provided host. `--host <HOST>` remains available as an
alias for compatibility.

Explicit `--cluster <HOST:PORT>` values are treated as seed nodes: the TUI
starts immediately, the seed is monitored right away, and the background
discovery pipeline expands cluster, replication, and sentinel topology as it
verifies peers. Like `--tcp`, a port-only value such as `--cluster 7000` is
treated as `127.0.0.1:7000`.

Important options:

- `-c, --config <PATH>`
- `--once`
- `--output <tui|json>`
- `--refresh <DURATION>`
- `--connect-timeout <DURATION>`
- `--command-timeout <DURATION>`
- `-n, --concurrency <N>`
- `--autodiscover [HOST]`
- `--cluster <HOST:PORT>`
- `--view <tree|flat|primary>`
- `--sort <alias|address|type|cluster|memory|mem|ops|lat|latmax|status>`
- `--no-config`
- `-a, --auth <PASSWORD>`
- `--user <USERNAME>`
- `-v, --verbose`

`--once` skips the interactive TUI. It performs one polling pass for explicit
targets, runs autodiscovery/verification for any configured discovery hosts,
prints the overview table to stdout, and exits.

`--output json` switches the main overview page to newline-delimited JSON
frames instead of drawing the interactive TUI. Each frame includes a top-level
microtime-style timestamp string (`seconds.microseconds`) and is generated from
the same centralized overview data model used by the TUI overview, which makes
the JSON stream suitable for
integration testing. When combined with `--once`, `rtop` emits a single
JSON frame and exits.

Version output includes build metadata:

```bash
rtop --version
# rtop x.y.z [YYYY-MM-DD] (<gitsha>[-dirty])
```

## Building release binaries

CI builds upload release artifacts for:

- `aarch64-apple-darwin`
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`

### Linux static musl binary

Install the musl target once:

```bash
rustup target add x86_64-unknown-linux-musl
```

Then build a release binary with:

```bash
cargo build-musl
```

Output binary:

```bash
target/x86_64-unknown-linux-musl/release/rtop
```

### macOS Apple Silicon binary

On an Apple Silicon macOS host, build the native release binary with:

```bash
cargo build --release --target aarch64-apple-darwin
```

Output binary:

```bash
target/aarch64-apple-darwin/release/rtop
```

## Testing

Run the full test suite with:

```bash
cargo test
```

## Git Hooks

This repository includes a tracked pre-push hook in `.githooks/pre-push` that
blocks pushes when `cargo fmt --all --check` reports required formatting
changes.

Enable the repo-local hooks path once:

```bash
git config core.hooksPath .githooks
```

The integration suite includes live, read-only Redis checks for both a
standalone instance and a Redis Cluster. By default it probes:

- standalone Redis at `localhost:6379`
- cluster Redis at `localhost:7000`

Override those endpoints with:

```bash
RTOP_TEST_REDIS_ADDR=redis.example:6379 \
RTOP_TEST_REDIS_CLUSTER_ADDR=redis-cluster.example:7000 \
cargo test
```

If a live endpoint is unreachable, the corresponding integration test exits
early and the rest of the suite still runs.

## Config

Search order when `--config` is not provided:

1. `$XDG_CONFIG_HOME/redis-top.toml`
2. `~/.config/redis-top.toml`
3. `./redis-top.toml`

Example:

```toml
[global]
refresh_interval_ms = 1000
connect_timeout_ms = 300
command_timeout_ms = 500
concurrency_limit = 16
view_default = "tree"
sort_default = "address"
still_autodiscover = true

[theme]
background_color = "black"
foreground_color = "white"
carat_color = "white"
warning_color = "yellow"
critical_color = "red"

[[targets]]
alias = "local"
addr = ":6379"
protocol = "tcp"
user = "default"
password_env = "REDIS_PASSWORD"
enabled = true

[columns.used_mem]
type = "info"
header = "Mem"
info_key = "used_memory"
value_type = "bytes"
format = "bytes_human"

[columns.maxmem_pct]
type = "calc"
header = "%MaxMem"
calc = "maxmemory_percent"
format = "pct:1"

[columns.lat_max]
type = "calc"
header = "LatMax"
calc = "latency_max_ms"
format = "ms:2"
emphasis = "max"

[view.overview.emphasis_style]
bold = true
italic = false
foreground_color = "yellow"

[columns.lat_max.emphasis_style]
foreground_color = "red"

[view.overview]
visible = ["alias", "addr", "role", "slots_total", "used_mem", "ops", "lat_last", "lat_max", "status"]

[view.overview.sort]
by = "ops"
dir = "desc"
```

`[theme]` colors support: `black`, `red`, `green`, `yellow`, `blue`,
`magenta`, `cyan`, `gray`/`grey`, `white`.

`[global].still_autodiscover` defaults to `true`. Leave it enabled if you want
saved targets to provide credentials or fixed instances without suppressing
background autodiscovery. Set it to `false` if config-defined `[[targets]]`
should behave like an explicit fixed target list.

`[[targets]]` accepts `user` or `username`, plus either `password` or
`password_env`. If you omit the host from a TCP `addr`, `rtop` assumes
`localhost`, so `:6380` and `6380` both resolve to loopback addresses. Configured
TCP target credentials are also reused by autodiscovery when it verifies the
same `host:port`.

Overview columns also support `emphasis = "max"` or `emphasis = "min"` to mark
the highest or lowest visible value each frame.

Emphasis styling is configurable with `[view.overview.emphasis_style]` and may be
overridden per column with `[columns.<key>.emphasis_style]`. Supported style
keys are `bold`, `italic`, `underlined`, `dim`, `reversed`, and
`foreground_color`.
