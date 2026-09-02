# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**Tunnel** — a Windows tray-resident SSH port-forwarding (tunnel) desktop app built with Tauri 2. **Multi-host**: the config holds a list of tunnel entries, each = one SSH server + own auth + one or more port-forward rules; the app binds TCP listeners on `127.0.0.1:<local_port>` per rule and forwards through SSH to `<target_host>:<target_port>` (direct-tcpip channels). Each tunnel starts/stops independently, controlled centrally from the mini widget rows, the config panel list, or the tray menu. Designed for database clients to reach services through SSH jump hosts.

- Frontend: vanilla TypeScript + Vite (no framework). Backend: Rust with Tokio.
- UI text and code comments are in Chinese (`zh-CN`) — keep new UI strings/comments consistent.
- App is a background/tray app: the always-on-top `mini_widget` shows one row per tunnel; closing it hides to tray. Config is a separate `config_panel` window (master-detail: list ↔ per-tunnel edit form).
- Windows-only target in practice (WebView2/Windows environment); crate also declares staticlib/cdylib (mobile scaffolding) but only desktop is implemented.
- Both windows load the **same `index.html`**; `src/main.ts` picks the controller by `getCurrentWindow().label` (`mini_widget` → `MiniWidget`, `config_panel` → `ConfigPanel`).

## Commands

```bash
npm run tauri dev      # Full dev: Vite (strict port 1420) + Rust app (cargo build under the hood)
npm run tauri build    # Release: builds frontend, then compiles + bundles the Rust app
npm run dev            # Vite dev server only (frontend work, no Rust)
npm run build          # Frontend typecheck (`tsc`) + Vite production build into dist/
```

- `src-tauri/` is a normal cargo project (the `tauri` CLI drives it); Rust needs the MSVC toolchain.
- Release automation: `.github/workflows/release.yml` builds Windows NSIS+MSI on a `v*` tag push (or workflow_dispatch) and creates a **draft** GitHub Release (publish manually). Version must match across `tauri.conf.json` / `Cargo.toml` / `package.json`.
- **No tests and no linter are configured** in either workspace. Frontend typecheck is enforced by `tsc` during `npm run build` (`noUnusedLocals`/`noUnusedParameters`/`strict` are on — unused private class fields also fail).
- `dist/`, `node_modules/`, `src-tauri/target/` are gitignored build output.
- Gotcha: the repo previously lived at a different path (`Desktop\tunnel`); stale `src-tauri/target` build-script outputs can embed the old absolute path and break `cargo check` with "failed to read plugin permissions" — fix by `cargo clean` in `src-tauri`.

## Architecture

### Rust backend — `src-tauri/src/`

- `main.rs` — thin wrapper calling `tunnel_lib::run()` (crate `tunnel_lib`).
- `config.rs` — serde types + password encryption. **The TS interfaces in `main.ts` mirror these serde shapes by hand** (snake_case; externally-tagged enums) — keep both sides in sync.
  - `TunnelConfig { id, name, ssh_host, ssh_port, ssh_user, auth_method: AuthMethod, forwards: Vec<ForwardRule> }`. `id` is the stable identity for commands/events/statuses; Rust is its single source of truth — empty ids are filled by `AppConfig::ensure_ids()` on save and load (`config::generate_id()` = 32 hex from `ring::rand`, no extra dependency).
  - `AuthMethod` = `Password { password }` | `Key { private_key_path, passphrase }` (externally tagged).
  - `AppConfig { tunnels: Vec<TunnelConfig> }` + a `#[serde(skip_serializing)] tunnel` legacy field that only exists so old single-tunnel `tunnel.json` files deserialize; `migrate_and_ensure_ids()` folds them into `tunnels` on load.
  - `TunnelStatus` = `Disconnected | Connecting | Connected | Error(String)` — serializes as a plain string **or** `{"Error": "..."}`; TS must check `typeof status === "string"` first.
  - `TunnelMetric { id, name, status, rx_bytes_per_sec, tx_bytes_per_sec }` — note there is **no latency field** (removed; it was an unimplemented 0.0 stub).
  - Passwords are AES-256-GCM sealed with a **hardcoded key** (`secret` module) — obfuscation only, not real security. `encrypt_passwords()`/`decrypt_passwords()` run over the whole list on file write/read.
- `tunnel.rs` — the SSH engine:
  - `TunnelManager` (in `AppState`, wrapped in `Arc`) holds `running: Arc<RunningMap>` (`HashMap<tunnel_id, RunningTunnel { stopped: AtomicBool, status, name }>`) plus the shared config mutex. API: `start_tunnel(id)` (Err if already running or still tearing down — waits ≤3 s for a stopped runtime to fully exit before registering, so two runtimes can't fight over the same local ports), `stop_tunnel(id)` (sets the flag only), `start_all`/`stop_all`, `is_running`, `active_statuses()`/`status_metrics()` (map/`TunnelMetric` snapshot of running tunnels), `prune(keep_ids)` (auto-stops running tunnels missing from a newly saved list = delete safety), `refresh_names`.
  - `TunnelRuntime` (one per running tunnel) runs an **outer auto-reconnect loop until the user stops**: connect + auth (password or `load_secret_key`, 15 s timeout, keepalive 15 s × 3) → **bind ALL local ports up front** (any failure aborts that session — no silently half-working tunnel; `bind_local_port` retries 5×500 ms and deliberately avoids `SO_REUSEADDR`, which on Windows lets stale sockets double-bind) → spawn one listener task per bound forward → a 1 Hz metrics task computing per-second byte deltas from `AtomicU64` accumulators (counters reset each session). Connect failure publishes `Error(reason)` (visible in the UI) then retries after 5 s; an SSH drop (`Notify` watcher polling `is_closed()` every 2 s) cleans up the session via a per-session `session_cancel` flag and retries after 5 s. All retry sleeps are interruptible on the stop flag. **Cooperative shutdown**: stop = set flag; tasks poll every ~200 ms; `run()` returns only on user stop.
  - Status publishing has **one funnel**: free fns `publish_metric` / `unregister_runtime` (shared with the manager via the `RunningMap` Arc) update the map → `emit("tunnel-metric")` → `refresh_tray`. 1 Hz metric ticks emit directly (NOT through the funnel, to avoid rebuilding the tray menu every second). A runtime publishes its final `Disconnected` (only reachable via user stop) *before* unregistering; `unregister_runtime` guards against a stale runtime deleting a newer one via `Arc::ptr_eq` on the stopped flag.
  - Server host-key check returns `Ok(true)` unconditionally (no host-key verification — noted as intentional).
  - **Lock discipline** (violating it risks deadlock via `refresh_tray`): acquire config lock before running lock, never hold either across `refresh_tray`/GUI calls, clone data out before releasing.
- `lib.rs` — bootstrap, windows, tray, commands:
  - Tray built once in setup **without** a static menu; `on_menu_event` registers into a global listener list, so menus swapped in later still route events. `refresh_tray(app)` (async) rebuilds everything on status transitions/config saves: aggregate icon color (any `Error`→red; any `Connecting`→yellow; all connected→green; else gray — drawn via `make_tray_icon_pixels`/`tray_icon`) + full menu: one item per tunnel `tunnel_<id>` labeled `{●|◐|○} {name}  {已连接|连接中|未连接|错误}` (click toggles that tunnel), then 全部连接 (`all_start`) / 全部断开 (`all_stop`) / 配置 (`open_config`) / 退出 (`quit`, which stops all tunnels first, then destroys windows so WebView2 releases cleanly).
  - Windows: `mini_widget` declared in `tauri.conf.json` (340×200, frameless/transparent/always-on-top, non-resizable). Closing it is intercepted both here (`prevent_close` → hide) and in TS — the process lives until tray quit. `config_panel` created on demand by the `show_config_window` command (480×640, resizable); idempotent (show/focus if open).
  - Config persisted to `<app_config_dir>/tunnel.json` (`AppConfig`, list form). `load_config` runs migrate + decrypt; `save_config` (command) ensures ids, swaps the shared config, `prune`s deleted running tunnels, writes the file, emits `config-updated`, refreshes the tray, and returns the updated `AppConfig` (frontend adopts it — that's how new tunnels learn their assigned ids).
  - `tauri-plugin-single-instance`: second launch re-shows the mini widget.

### IPC contract (both windows)

- Commands (`invoke`): `get_config` → `AppConfig`, `get_tunnel_statuses` → `Vec<TunnelMetric>` (running-tunnel snapshot — windows call it at startup because metric events are transient and a fresh window would miss them), `save_config(tunnels)` → updated `AppConfig`, `start_tunnel(id)`, `stop_tunnel(id)`, `show_config_window`. All in `lib.rs` `invoke_handler`; window permissions in `src-tauri/capabilities/default.json` (includes `core:window:allow-set-size` — needed by the widget's auto-height).
- Events: `tunnel-metric` (status transitions + 1 Hz rates; **route by `payload.id`**, drop unknown/deleted ids) and `config-updated` (fired by `save_config`; mini widget reloads its row list on it — the config panel doesn't listen since it's the only saver and adopts the save return).
- Status/buttons flow **one-way Rust → UI**: start/stop commands return immediately and report nothing; UI state machines flip on subsequent `tunnel-metric` events (busy labels "⏳ 连接中…/⏳ 停止中…" cover the ≤ ~400 ms cooperative-stop lag; `Err("已在运行")` is swallowed).

### Frontend — `index.html` + `src/main.ts` (+ `styles.css`)

- Types + shared helpers (`el`, `lbl`, `showToast`, `formatBytes`, `statusKindOf`, `statusZhOf`, `renderCtlButton`) at module top; `setupDragAndButtons` is a **single delegated** `mousedown` on the view root (`e.target.closest("button,input,…")` → skip; else `getCurrentWindow().startDragging()`) because rows are dynamically mounted.
- `MiniWidget`: one `<div class="tunnel-row">` per tunnel — status dot (`.row-dot`, error text in `title`) + name + `▼rx ▲tx` rates + per-row start/stop button. Source of truth `Map<id, RowState>`; populated by `get_config()`+`get_tunnel_statuses()`, then event deltas. Window height auto-fits rows via `getCurrentWindow().setSize(new LogicalSize(340, h))` clamped to [200, 420] (`WIDGET_*` consts; header height + rows `scrollHeight` + 16). Widget listens to `config-updated` and reloads (wholesale rebuild — row count can only change on save).
- `ConfigPanel`: master-detail in one mount point. List view: toolbar (添加隧道 / 全部连接 / 全部停止 — all-start/stop is a frontend loop of per-id invokes with `allSettled`) + cards (`.tunnel-card`: name, `user@host:port · N 条转发`, status chip, per-card 启动/停止/编辑/删除; delete uses the dialog plugin's native `confirm`). Edit view: the whole form is built dynamically (inputs located by class: `.field-ssh-host`, `.field-key-path`, …; forward-rule rows, private-key file browse). Editing is form-only (启停 lives on cards/rows); edits take effect on that tunnel's next start. 保存 validates → merges into local list → `save_config` → **adopts the returned list** (fresh ids) → back to list; failure rolls back the local entry and reopens the editor.
- UI must tolerate `tunnel-metric` events for deleted tunnels (drop them) and statuses arriving for tunnels never seen (snapshot handles init).

## Common gotchas

- Two tunnels running concurrently with the same `local_port`: the later one fails its up-front bind and keeps retrying with a visible error (it will not half-start); free the port or change it. `Error` status now means "running but retrying" — the UI shows a 停止 button on error rows to cancel the retry loop.
- `resizable: false` on `mini_widget` does not block programmatic `set_size` (user resize only); if QA ever shows a fixed-size widget, flip `resizable: true`.
- Rust rebuilds are slow after a full `cargo clean` (the whole Tauri/russh dep tree compiles); prefer incremental `cargo check` while iterating.
