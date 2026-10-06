# rtop

`rtop` is a terminal UI for monitoring Redis/Valkey instances.

## Implemented MVP

- Polls one or more Redis targets every second (default, configurable)
- Starts immediately and runs Redis/Valkey autodiscovery in the background
- Overview screen with:
  - a top activity panel with CPU/operations/network history
    graphs, and aggregate memory and client counts
  - generic, configurable columns (INFO-backed + calculated)
  - defaults for alias/address/type/memory/ops/latency/status plus a cluster/replication color gutter, with `Type` auto-hidden in `Tree` view and host auto-hidden when all targets share one host
  - available optional columns including `connected_clients` and `master_repl_offset` (`INFO replication` / `master_repl_offset`)
  - available optional cluster slot coverage columns `slots_total` (`#Slots`) and
    `slots` (`Slots`, the full comma separated range list such as
    `0-5460,9000`), both sourced from `CLUSTER SHARDS` and populated only for
    cluster primaries; replicas and non-cluster instances leave them blank
- Detail screen with summary (including last, maximum, and average latency in
  milliseconds plus sample count), raw `INFO`, `INFO COMMANDSTATS`, an on-demand `bigkeys`
  view, and a timed `hotkeys` view for CPU/NET sampling, including full
  server-reported error details when polling fails
- Tree, flat, and primary-only overview modes
- Sorting by currently visible column keys and substring filtering
- Kill picker on `K` with Redis `SHUTDOWN` and local signal options
- Credential form on `a` for authenticating the selected server without
  restarting `rtop`
- Overview border summarizing refresh interval, view, sort, host rendering, and filter
- `Status` cells colored with the theme's `warning_color` (`LOADING`, `TIMEOUT`,
  `AUTH`, `PROTECTED`) and `critical_color` (`DOWN`, `ERROR`)
- Bottom status/key bar with mnemonic shortcut labels and live search/filter input echo
- Live discovery status in the footer, including queued/probing/verified counts
- Config loading from TOML + CLI target merge
- Handles per-instance failures without crashing UI
- Surfaces richer instance states such as `PROTECTED`, `AUTH`, `LOADING`, and `DOWN`

## Activity panel

Press `m` / `M` in the main overview to hide or show the top activity metrics
panel, giving the server table more room when hidden. It is visible by default;
the toggle lasts for the current session. Metrics and history continue updating
while hidden.

The top of the main overview shows activity for all monitored servers. Use
`j`/`k` or `Up`/`Down` to move focus in the main server table,
then `Space` to select servers. With any servers selected, the panel sums only
that group. `Esc` clears the selection and returns to all servers (another
`Esc` exits). These are the same selections used by authentication and kill
actions. Filtering and Tree/Flat/Primary mode do not change
the aggregate group; selected servers hidden by the view still contribute.

CPU is the sum of Redis process system and user CPU usage, measured between
successful INFO samples: 100% equals one fully occupied CPU core, so totals
can exceed 100%. It needs two samples after startup, a restart, or a polling
failure. Operations and network rates come from Redis' instantaneous INFO
metrics. Network text shows incoming and outgoing bytes per second; its graph
shows their sum. Memory is summed `used_memory`, and clients are summed
`connected_clients`. These are per-process totals, including replicas, rather
than deduplicated logical data or host-wide utilization.

Graphs retain up to 120 samples at the configured refresh interval, newest on
the right. The rows show process CPU, operations/second, and combined incoming
plus outgoing network bytes/second. Each row's right-hand `max` label is the
value represented by a full-height bar. Scales start at 100% CPU, 1,000 ops/s,
and 1 MiB/s of network traffic **per monitored server**. They grow to retain
higher observed peaks for the current server group, even after those samples
scroll out of history or the terminal is resized. This keeps idle polling
traffic near the bottom instead of stretching it to full height.

These scales are display references, not estimates of server capacity; CPU
100% still means one busy core. Changing the aggregate membership resets both
history and retained peaks. Failed servers and samples older than
two refresh intervals are excluded; the header reports live/total servers
and flags partial data, and unavailable metrics show `-`. Short terminals
use a compact operations graph or hide the panel to leave room for the table.
This panel is interactive only; plain-text and JSON output keep their existing
table format.

## Key Bindings

- `q`: quit from the overview, or close the active overlay window
- `Ctrl+C`: quit immediately
- `H`: open full help page
- `f` or `/`: edit the overview filter, keeping existing text
- `t`: cycle Tree / Flat / Primary (overview)
- `m` / `M`: show or hide the top activity metrics panel (overview)
- `s`: open Sort By to choose from currently visible overview columns
- `c`: open the column picker for the overview or the active `Commandstats` pane
- `p` / `P`: toggle compact layout in `Commandstats`
- `Space`: toggle selection of the focused overview server
- `a`: enter one username and password and retry all selected servers
- `K`: open the kill picker for selected servers; `Enter` chooses the action,
  with an additional confirmation when stopping more than one server
- `?`: toggle help
- `j` / `k` or `Up/Down`: move focus in overview, or scroll the active detail pane when it has more rows than fit
- `PgUp` / `PgDn`: move or scroll a full page in the overview, detail panes, and pickers
- `g` / `Home` and `G` / `End`: jump to the first or last row; `NG` jumps to row `N` in the overview
- `Enter`: open detail for the focused server
- `Esc`: clear server selections in the overview, or quit if none are selected;
  close the active overlay window, go back from detail/help, or stop filter editing
- `h` / `l` or `Left` / `Right`: cycle detail tabs (`Tab` advances, `Shift+Tab` goes back)
- `S` / `I` / `C` / `B` / `K`: jump to `Summary` / `Info Raw` / `Commandstats` / `Bigkeys` / `Hotkeys` in detail view
- `Shift+Up/Down`: reorder columns inside the column picker
- `o` / `O`: toggle host rendering (default auto-hides host when all targets share one host)
- `/`: start filter input in overview, or filter the active detail pane in detail view (`Summary`, `Info Raw`, `Commandstats`, `Bigkeys`, or `Hotkeys`)
- `C` / `N`: start CPU or NET sampling while the `Hotkeys` tab is open
- `X`: stop active `Hotkeys` sampling early, or reset the `Hotkeys` pane back to its idle prompt
- `r` / `R`: refresh now, rerun the on-demand `Bigkeys` scan, or rerun `Hotkeys` sampling for the last selected metric while that tab is open

Shortcuts without a motion-key conflict remain case insensitive. Kill and the
Hotkeys detail tab require `K`. Lowercase
`h`/`j`/`k`/`l` are reserved for movement. While editing filters or credentials,
letters and digits are entered as text. `j`/`k` also navigate the sort, column,
and kill pickers. Help remains on `H`, `F1`, and `?`.

Function keys remain available as aliases: `F1` help, `F5` view mode, `F6`
Sort By, `F7` columns, `F8` auth, and `F9` kill. `v` also opens columns.
`F3` starts overview search input; `F4` starts filtering with an empty filter.

In `Commandstats`, use `c`, `v`, or `F7` to choose columns. `Command`, `Calls`,
`Usec`, and `Usec/Call` are shown by default. Additional metrics reported by
`INFO COMMANDSTATS`, such as `rejected_calls` and `failed_calls`, become
available in the picker automatically and start hidden. Their headers use the
server's field names, and their values are right-aligned as reported by the
server. Metrics discovered from any server stay available for the session;
a command or server that does not report a selected metric shows `-`.
Use `Up/Down` or `j/k` to select a column,
`Enter/Space` to toggle it, `Shift+Up/Down` to reorder it, and `Esc/q` to close
the picker. At least one column stays visible. Choices apply to all servers'
commandstats for the current session, independently of overview columns.
Rows remain sorted by calls even when the `Calls` column is hidden.

Press `p` in `Commandstats` to toggle a compact grid of `Command Calls` pairs,
fitting as many pairs per row as the pane width allows. Commands remain sorted
by calls descending, left to right then top to bottom (ties sort by name).
Filtering and scrolling, including page and first/last-row navigation, still
work; resizing recalculates the grid. The toggle lasts for the session and
preserves your normal column choices. Toggling starts at the top of the pane.

Commandstats uses the overview's Space-selected nodes, including selections
hidden by the overview filter or view. With no selections it uses the focused
node. Multiple selections show combined totals by command and a node count in
the detail pane. `Calls`, `Usec`, and additional unsigned integer counters are
summed; `Usec/Call` is total microseconds divided by total calls (zero for no
calls). Missing commands contribute zero; missing extra metrics contribute
nothing, and non-counter extra values show `-` in combined totals. Totals use
the latest stored samples from each node, which may have different uptimes or
counter reset times; they are server execution counts, not unique client requests.

Prefix a motion with a positive count: `10j` moves down ten rows, `3k` moves
up three, and `2l` advances two detail tabs. Counts also work with arrow keys;
vertical movement stops at the list boundary. `Esc` cancels a pending count,
and another command discards it.

Use `4Space` to select the current server and the next three visible servers.
Press Space within 500 ms after `3k` to select the original server and the two
above it; `3j` followed quickly by Space selects downward. The motion happens
immediately; quick Space converts it to a range selection and puts focus on
the last selected row. After 500 ms, Space just toggles the new focused row.
Ranges include the starting row, stop at the boundary, and add to existing
selections without deselecting already selected servers. They follow the
visible sorted/filtered order captured when the motion was entered.

Use plain `Space` to select or deselect servers. The caret gutter shows `●` for a
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
credentials are kept only for the current process by default. Enable
`[global].remember_auth` to reuse successfully authenticated credentials after
restarting. For compatibility with
Redis versions before 6.0, `rtop` first tries `AUTH default <password>` and
retries with `AUTH <password>` when the server reports that the ACL-style form
is unsupported. A non-default username is sent only with the ACL-style form.

The `K` kill picker offers `SHUTDOWN SAVE`, `SHUTDOWN NOSAVE`, `SIGINT`,
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
performed on demand when the `Bigkeys` tab is opened or refreshed. Scans run in
the background, so regular polling of every server continues while a large
keyspace is scanned. The header shows when a scan is in progress, and after
completion it shows the result age in seconds.

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
rtop -r 500ms 6379
rtop --refresh-rate 2s 6379 6380
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
rtop --config-file ~/.config/rtop.toml
rtop -c config.toml 127.0.0.1:6379
rtop --config
rtop --config get global.refresh_interval_ms
rtop --config set global.refresh_interval_ms 2000
rtop --config get addr --target local
rtop --config set enabled false --target local
```

For TCP targets, you can pass just a port (for example `6379`), and it is treated as
`127.0.0.1:6379`.

Host-only positional values such as `192.168.0.148` are treated as autodiscovery
hosts, not fixed monitored instances. Exact TCP targets such as `6379` or
`192.168.0.148:6380` disable autodiscovery by default and only connect to the
requested server(s).

When you provide explicit targets, `rtop` does not also add unrelated
`[[targets]]` entries from `rtop.toml`. If an explicit target matches a
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

- `-c, --config-file <PATH>` (legacy `--config <PATH>` still works)
- `--config [get KEY | set KEY VALUE]`
- `--target <ALIAS_OR_ADDRESS>` (select a target for config commands)
- `--once`
- `--output <tui|json>`
- `-r, --refresh-rate <DURATION>` (`--refresh` remains an alias)
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

Polling defaults to **1 second**. Use `-r` / `--refresh-rate` with an explicit unit,
such as `500ms`, `1s`, or `2s`, to set the interval between polling passes.
To persist the interval, set it in milliseconds in your TOML config:

```toml
[global]
refresh_interval_ms = 1000
```

The CLI option overrides the configured interval. The interval must be greater
than zero. It applies to polling in both the TUI and JSON stream modes;
`--once` still performs only one polling pass.

`--once` skips the interactive TUI. It performs one polling pass for explicit
targets, runs autodiscovery/verification for any configured discovery hosts,
prints the overview table to stdout, and exits.

`--output json` switches the main overview page to newline-delimited JSON
frames instead of drawing the interactive TUI. Each frame includes a top-level
microtime-style timestamp string (`seconds.microseconds`) and is generated from
the same centralized overview data model used by the TUI overview, which makes
the JSON stream suitable for
integration testing. When combined with `--once`, `rtop` emits a single
JSON frame and exits. Cells that carry a severity (currently `Status`) include
a `tone` of `warning` or `critical`; the field is omitted otherwise.

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

Monitoring config search order when no explicit file is provided:

1. `$XDG_CONFIG_HOME/rtop.toml`
2. `~/.config/rtop.toml`
3. `./rtop.toml`

### Inspecting and editing settings

`rtop --config` prints a grouped overview with the file path, global settings,
saved targets (including disabled ones), and other saved sections. Global
defaults are marked `[default]`; headings are colored when stdout is a terminal.
It exits without starting monitoring or contacting Redis.

Config commands use `$XDG_CONFIG_HOME/rtop.toml`, falling back to
`~/.config/rtop.toml` when `XDG_CONFIG_HOME` is unset, empty, or relative. They
do not fall back to the working directory's `rtop.toml`. Use `-c PATH` or
`--config-file PATH` to inspect or edit another file. A missing user config
shows global defaults and is created on the first successful `set`.

```bash
rtop --config                                      # grouped overview
rtop --config get global.refresh_interval_ms        # single value
rtop --config set global.refresh_interval_ms 2000
rtop --config set remember_auth true                # global. is optional
rtop --config set theme.foreground_color cyan
rtop --config --target local                        # one saved target
rtop --config get addr --target local
rtop --config set enabled false --target local
rtop --config set tags '["dev", "cache"]' --target local
rtop --config set password_env REDIS_PASSWORD --target localhost:6379
rtop --config -c ./rtop.toml
```

`--target` matches an existing target's alias or address. TCP address matching
also accepts port-only forms and equivalent loopback addresses. Missing or
ambiguous targets fail; these commands do not create target entries. Global
`get` returns a saved value or its built-in default; target and theme `get`
require a saved value. Target `user` and `username` are interchangeable.

`set` accepts documented global, theme, and target keys. Booleans use `true` or
`false`, intervals (in milliseconds) and concurrency require positive integers,
and tags use a TOML array of strings. String values are passed literally, without
TOML quoting. Invalid keys and values fail without changing the config. Use
`--` before positional arguments if a value begins with a dash.

Passwords, usernames, secret/token fields, and URL user information are redacted
in both overview and `get` output. `password_env` names remain visible; their
values are never read by config commands. Comments are not displayed, and parse
errors omit source text to avoid exposing credentials. Setting a password does
not echo it, but a literal command-line password may be recorded in shell history
or visible in process arguments; prefer `password_env`.

Updates preserve comments and unrelated sections, use a sibling `.toml.lock`
file to coordinate writers, and replace the file atomically with owner-only
permissions (`0600` on Unix). Setting `password` removes `password_env`, and
vice versa; setting `user` or `username` removes the other spelling. Config
commands never read or modify the generated `rtop-auth.toml` credential cache.
Monitoring flags cannot be combined with config commands. The legacy
`--config PATH` spelling still starts monitoring with that file; the words
`list`, `get`, and `set` are reserved for config commands, so use `-c` for paths
with those names.

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
remember_auth = false

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

`[global].remember_auth` defaults to `false`. Set it to `true` to save credentials
only after Redis accepts `AUTH` (including credentials entered in the auth form,
CLI, or main config). With multiple selected servers, only successful servers
are saved; failed attempts never replace previously saved credentials. Saving
happens immediately after authentication, even if a later monitoring command is
denied by ACLs.

Saved credentials live in the generated `$XDG_CONFIG_HOME/rtop-auth.toml`, or
`~/.config/rtop-auth.toml` when `XDG_CONFIG_HOME` is unset, empty, or relative.
This location is independent of the selected monitoring config file; credential
persistence never rewrites `rtop.toml`.
The file contains **plaintext passwords** and is created with owner-only
permissions (`0600` on Unix). Writes are atomic and use an adjacent
`rtop-auth.lock` to coordinate concurrent processes.

Saved credentials apply only to matching TCP addresses or Unix socket paths,
including servers found by autodiscovery; they do not add targets to the server
list. Auth declared in a matching main-config target (`user`/`username`,
`password`, or `password_env`) replaces the entire saved credential pair. An
unset `password_env` never falls back to a saved password. Explicit connection
credentials and interactive retries also take precedence over saved credentials.

Disabling `remember_auth` stops both reading and writing the generated file;
it does not delete existing credentials. Delete `rtop-auth.toml` to forget all
saved credentials. `--no-config` also disables persistence unless an explicit
`--config` file enables it. A malformed generated file is reported without
printing its contents, and write failures warn without failing authentication.

`[theme]` colors support: `black`, `red`, `green`, `yellow`, `blue`,
`magenta`, `cyan`, `gray`/`grey`, `white`. `warning_color` and
`critical_color` color the overview `Status` cell; a column's explicit
emphasis `foreground_color` takes precedence when both apply.

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
