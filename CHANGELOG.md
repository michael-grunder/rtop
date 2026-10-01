## Unreleased

### Added

- Add `--config` for a grouped, credential-redacted settings overview and
  `--config get/set` for global, theme, and existing target settings selected
  with `--target`. Preserve TOML comments and unrelated sections, validate
  updates, and write atomically with private permissions. Add `--config-file`
  alongside `-c` while preserving legacy `--config PATH` monitoring usage.

- Add opt-in `[global].remember_auth` to persist credentials only after successful
  Redis authentication in a private, generated `rtop-auth.toml` under the user
  config directory. Reuse credentials for matching endpoints while main-config
  authentication overrides the saved pair; use atomic writes and a file lock.

- Add Space selection of overview servers, with row markers and a selected
  count that persist across refreshes, sorting, filtering, and view changes.

- Add available `slots_total` (`#Slots`) and `slots` (`Slots`) overview columns
  that report cluster hash slot coverage parsed from `CLUSTER SHARDS`. Both are
  populated for cluster primaries only, since replicas serve their primary's
  slots and non-cluster instances have none.
- Add an `F8` credential form that lets users enter an optional username and a
  masked password for a selected server in `AUTH` state, then retries polling
  immediately without restarting the TUI.
- Add a tracked `.githooks/pre-push` hook that blocks pushes when
  `cargo fmt --all --check` would reformat the tree, plus README setup
  instructions for enabling the repo-local hooks path.
- Add an `F9` kill picker to the overview TUI with `SHUTDOWN SAVE`,
  `SHUTDOWN NOSAVE`, `SIGINT`, `SIGTERM`, `SIGQUIT`, and `SIGKILL` actions for
  the selected server.
- Add a new `Hotkeys` detail pane that can start timed Redis `HOTKEYS`
  sampling for `CPU` or `NET`, show a live countdown, fetch the results
  automatically, and browse them with the same `/` filtering and scrolling
  behavior as the other detailed TUI panes.
- Add a shared overview-frame data model that now feeds the main TUI overview
  and can also be emitted as newline-delimited JSON with `--output json`,
  including a single-frame `--once --output json` mode for integration tests.
- Add a top-level microtime-style timestamp (`seconds.microseconds`) to each
  `--output json` overview dump.
- Add richer Redis instance statuses such as `PROTECTED` and `AUTH`, and show
  full server-reported error details in the detail summary view.
- Add `[global].still_autodiscover`, defaulting to `true`, so config-defined
  targets can provide credentials without disabling background autodiscovery.
- Add `--autodiscover[=<HOST>]` so explicit instance targets can opt back into
  background autodiscovery on localhost or a specific host.
- Add `--once` to print the overview status table a single time for polled and
  autodiscovered instances, then exit without starting the interactive TUI.
- Add scrolling and `/` filtering to the text-based detail panes (`Summary`,
  `Latency`, and `Info Raw`) so long `INFO` output can be navigated and narrowed
  in place.
- Add an available `master_repl_offset` overview column backed by Redis
  `INFO replication-offset`, so replicas and masters can show replication
  progress.

### Changed

- Rename the project, Cargo crate, executable, and CI artifacts from `reditop`
  to `rtop`, including build and test environment variable prefixes from
  `REDITOP_` to `RTOP_`.

- Make `Esc` in the overview clear all server selections before a second press
  exits, including selections hidden by the current view or filter.

- Share the overview caret gutter with compact selection markers: `●` for
  selected servers and a bold `▶` for the focused selected server, replacing
  the separate checkbox column.

- Apply auth and kill to all selected servers, falling back to the focused
  server when none are selected. Confirm multi-server stops with
  `Stop <N> servers with <how>?`. Tree, Sort By, and Columns remain global.

- Lead overview controls and help with mnemonic shortcuts: `a` for auth,
  `f` or `/` for filtering, `t` for view mode, `s` for Sort By, `c` for columns,
  and `k` for the kill picker. Accept uppercase variants and retain function
  keys and `v` as compatibility shortcuts. `s` now opens the sort picker
  instead of cycling columns; `f` and `/` both preserve existing filter text.

- Authenticate connections explicitly and fall back from `AUTH default
  <password>` to the pre-Redis-6 `AUTH <password>` form when a server rejects
  ACL syntax.
- Auto-hide the redundant `Type` overview column while `Tree` view is active,
  so the default startup layout uses less horizontal space without removing the
  column from flat or primary views.
- Move the detail-pane tab shortcuts into the contextual footer while a detail
  view is open, freeing the old tab-strip space for more pane content.
- Center the idle `Hotkeys` prompt and simplify it to `Start sampling (60
  seconds)` with inline `CPU` / `NET` choices, matching the active sampling
  view's more concise layout.
- Change the `Hotkeys` detail pane shortcut to `K`, extend its default sampling
  duration to 60 seconds, and show rerun/reset affordances after a sample
  completes.
- Rework the `Hotkeys` sampling view to a more concise layout that keeps the
  title as `Hotkeys <type>` and shows only a centered `Sampling <seconds>s`
  line plus the `[X]` stop hint in the pane body.
- Render detail-pane tab shortcuts inline with each title, for example
  `Hot[K]eys` instead of prefixing the label as `[K]Hotkeys`.
- Expand overview view selection to three modes: `Tree` (default), `Flat`, and
  `Primary`, cycle them from `F5`/`t`, and show the active mode in the footer
  from startup through each toggle.
- Treat explicit CLI instances as fixed targets that disable autodiscovery by
  default, while still treating host-only positional inputs as autodiscovery
  hosts.
- Shorten the autodiscovery footer to a compact in-progress spinner label and
  clear it automatically once discovery completes.
- Add background Redis/Valkey autodiscovery with curated host port probing,
  localhost socket/process hints, Redis verification, and live TUI updates.
- Add `--host <HOST>` for remote autodiscovery, including repeated `--host`
  usage for scanning multiple hosts in one session.
- Add an available `connected_clients` overview column backed by Redis `INFO`
  clients output.
- Move the default config file lookup to flat `rtop.toml` files under
  `$XDG_CONFIG_HOME` or `~/.config`, and reuse configured TCP credentials during
  autodiscovery for matching endpoints.

### Fixed

- Finish the `rtop` rename in the TUI title and verbose startup output; verify
  default config discovery uses `rtop.toml`.

- Admit discovery topology expansions inline in the discovery manager loop
  instead of routing them back through its own message channel, so a run seeded
  only by `--cluster <HOST:PORT>` no longer finishes before the peers reported
  by `CLUSTER SHARDS` are queued and now maps the whole cluster.
- Replace constant-size `chunks_exact` iteration with array chunks to satisfy
  current Clippy guidance while retaining malformed key/value-list checks.
- Clean up clippy findings in hotkeys duration construction and TUI key
  handling.
- Read the `master_repl_offset` overview column from Redis' actual
  `INFO replication` field name, so `ReplOff` renders instead of staying `-`.
- Fix stale test fixtures after the `process_id`, `leave_killed_servers`, and
  boxed poller update model changes so `cargo test` and `cargo clippy` pass
  again.
- Keep a locally reset `Hotkeys` pane on its idle prompt after `X`, instead of
  letting later refresh frames resurrect the previously sampled results until a
  new `C`/`N` run is started.
- Allow `Hotkeys` sampling to stop early with `X` by issuing `HOTKEYS STOP`
  before fetching results, instead of forcing the full default duration every
  time.
- Make detail-pane scrolling and `/` filtering behave consistently across all
  detail tabs, and clear any active detail filters when returning to the
  overview.
- Stop loading unrelated config-defined targets when exact CLI targets are
  provided, while still reusing matching configured target context such as
  alias, username, password, and tags.
- Stop adding host-only positional autodiscovery inputs such as
  `192.168.0.174` to the TUI as a synthetic `DOWN` instance.
- Record timed-out poll attempts as observed latency samples so `LatMax` and
  `LatLast` reflect command and connection timeouts instead of keeping the last
  successful latency.
- Make `LatMax` overview emphasis flash only on frames where a new overall
  maximum is first observed, instead of keeping the current record holder
  highlighted indefinitely.
- Update the CI Zig installation step to use `mlugg/setup-zig@v2.2.1`,
  fixing musl `cargo zigbuild` jobs that were still pinned to `v1`.
- Keep autodiscovery active when config-defined TCP targets provide credentials
  by reusing those credentials only for exact matching endpoints instead of
  applying them to every discovered port on the same host.
- Make `q` close the active overlay window instead of exiting the TUI, while
  keeping `Ctrl+C` as an immediate full exit.
- Restore `q` and `Esc` quitting from the main overview when no overlay is
  open, while still making both keys close the active overlay first.
- Preserve configured overview column order on startup by deserializing column
  definitions with insertion order instead of hash order.
- Add config support for `user`/`username`, plaintext `password`, and
  env-backed `password_env`, including loopback defaults for hostless TCP
  addresses such as `:6380`.
