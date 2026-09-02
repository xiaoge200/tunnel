<div align="center">

# Tunnel

**Multi-host SSH Port-Forwarding Tool · Windows**

[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/Platform-Windows%2010%2F11-0078d4)

A tray-resident SSH tunnel manager with a floating status widget. Manage multiple tunnels (one SSH host + multiple port-forward rules each) centrally, and let local database clients reach services behind a jump host.

</div>

[简体中文](README.md) · **English**

---

## Table of Contents

- [What it does](#what-it-does)
- [Quick start (3 steps)](#quick-start-3-steps)
- [UI guide](#ui-guide)
- [Status colors](#status-colors)
- [Port-forward rules explained](#port-forward-rules-explained)
- [Installation](#installation)
- [Configuration & security](#configuration--security)
- [FAQ](#faq)
- [Development](#development)
- [License](#license)

---

## What it does

Suppose your database runs at `192.168.1.50:3306` on a network reachable only through an SSH jump host. Tunnel lets you:

```
local DB client  ──►  127.0.0.1:15432   (Tunnel listens here)
                         │
                  SSH encrypted tunnel (via jump host)
                         │
                     192.168.1.50:3306   (target service)
```

- **Run multiple hosts in parallel**, each started/stopped independently; one failing host never affects the others
- **Auto-reconnect**: on an unexpected SSH drop or failed connection, retries in the background every 5 s (stop any time with the 停止 button)
- Floating widget shows each tunnel's **live status and up/down throughput** with per-row controls
- Config window for central management (add/edit/delete + connect/stop all)
- Passwords encrypted at rest; closing windows hides to tray and forwarding keeps running

> Platform: **Windows 10/11 x64 only** (macOS / Linux not supported)

---

## Quick start (3 steps)

### Step 1 — Add a tunnel

1. Click **⚙** in the floating widget (or tray → 配置) to open the config window
2. Click **[+ 添加隧道] (Add tunnel)** and fill in:

| Field | Meaning | Example |
|---|---|---|
| 隧道名称 (Name) | A label you recognize | `prod-jump` |
| SSH 主机 (Host) | Jump host address | `106.54.242.26` |
| 端口 (Port) | SSH port | `22` |
| 用户 (User) | SSH login user | `root` |
| 认证方式 (Auth) | Password or private key | password: `******` |
| 端口转发规则 (Rules) | See [below](#port-forward-rules-explained) | see example |

3. Click **💾 保存 (Save)** — the tunnel appears as a card in the list

### Step 2 — Start the tunnel

Any of these works:

- **[▶ 启动]** on the card in the config window
- The row's **[▶ 启动]** button in the floating widget
- Right-click tray → click the tunnel entry

The status changes 连接中 (yellow, pulsing) → 已连接 (green). On failure the card shows an **inline red reason** — hover it for the full message.

### Step 3 — Use it

Point your local database client / tool at `127.0.0.1` + the **local port** from your forward rule — it behaves exactly like a direct connection:

```
MySQL:  127.0.0.1:15432   user/password as usual
psql:   psql -h 127.0.0.1 -p 15432 ...
```

> First time: verify the port is listening with `Test-NetConnection 127.0.0.1 -Port 15432` (PowerShell) or `telnet 127.0.0.1 15432`.

---

## UI guide

### Floating status widget

```
┌────────────────────────────────┐
│ TUNNEL                    ⚙ ✕ │   ← drag blank space to move; ⚙ opens config; ✕ hides to tray
├────────────────────────────────┤
│ ● 已连接  生产库跳板     [⏹] │
│   已连接 ▼ 12KB/s ▲ 3KB/s      │
│ ○ 未连接  测试环境       [▶] │
│   未连接 ▼ 0B/s   ▲ 0B/s       │
└────────────────────────────────┘
```

- Each row: status dot + tunnel name + status text + live ↓rx / ↑tx rates + start/stop button
- The window auto-fits its height to the row count (scrolls above the cap)
- Closing the window only hides it to the tray — **the app keeps running** and forwarding continues

### Config window (central management)

- **List page**: each card shows name, `user@host:port`, forward count and a status chip; buttons: 启动 (start) · 停止 (stop) · 编辑 (edit) · 删除 (delete, with confirmation; deleting a running tunnel stops it first)
- Toolbar: [+ 添加隧道] [▶ 全部连接] [⏹ 全部停止]
- **Edit page**: edits take effect on that tunnel's **next start** (a running tunnel is untouched); [← 返回列表] discards changes
- A version footer (`v0.2.0`) sits at the bottom of the list page

### Tray

- **Icon = the app logo, tinted by the overall status** (see table); left-click shows the floating widget
- **Right-click menu**: one entry per tunnel (live status, click to start/stop it) + 全部连接 (connect all) / 全部断开 (disconnect all) / 配置 (config) / 退出 (quit)

---

## Status colors

| State | Widget / menu | Tray logo | Meaning |
|---|---|---|---|
| Disconnected | gray ● 未连接 | gray-blue | not started |
| Connecting | yellow ◐ 连接中 (pulsing) | yellow | SSH session being established |
| Connected | green ● 已连接 | green | forwarding active |
| Error | red ● 错误 | red | connection failed — auto-retrying in the background (click 停止 to cancel); reason shown in the card |

The tray icon shows an **aggregate**: any error → red; else any connecting → yellow; all connected → green; otherwise gray-blue.

---

## Port-forward rules explained

Each tunnel may have **several rules** — each maps one local port to a remote address:

```
local port (this PC)  →  target host:target port (as seen from the SSH server)
   15432                  192.168.1.50:3306
   15433                  127.0.0.1:5432     ← the jump host itself works too
```

- **Target host** is resolved *from the jump host's point of view*: internal IPs, `127.0.0.1` (the jump host itself), or internal DNS names
- **Local ports** must be unique across all tunnels/rules
- Use [+ 添加] and the ✕ at the end of a row to manage rules

---

## Installation

### Option A — Download a release (recommended)

Grab the latest installer (e.g. `Tunnel_x.x.x_x64-setup.exe`) from [Releases](../../releases) and run it. No extra environment needed.

### Option B — Build from source

Prerequisites (Windows):

- [Rust](https://rustup.rs) (MSVC toolchain) + [VS Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) ("Desktop development with C++")
- [Node.js](https://nodejs.org) ≥ 18
- WebView2 runtime (built into Windows 11)

```bash
git clone <repo-url>
cd tunnel
npm install
npm run tauri dev     # development with hot reload
npm run tauri build   # build the installer
```

Output: `src-tauri/target/release/bundle/nsis/*.exe`

---

## Configuration & security

- Config file: `%APPDATA%\com.xiaoge.tunnel\tunnel.json` (all tunnels for this user)
- SSH passwords are stored **AES-256-GCM encrypted**, never in plaintext — but the key is compiled into the binary, so this is **obfuscation-grade, not hardware-grade security**. Don't keep important credentials on untrusted machines
- Private key paths are stored in plaintext — keep the key files themselves access-controlled
- **No SSH host-key fingerprint verification** (first-connect trust-all) — use it only on networks you trust

---

## FAQ

**Q: Stuck at 连接中 (connecting), then turns red?**
A: A single attempt times out after 15 s. On failure the card shows the reason in red and **retries every 5 seconds** until you press 停止. Check the reason: host/port reachability (cloud security group / firewall allowing 22), DNS, and whether the account may log in at all.

**Q: What happens when the SSH connection drops?**
A: The old session is torn down and it reconnects every 5 s (status flickers 连接中 meanwhile). No manual action needed — press that tunnel's 停止 if you don't want retries.

**Q: Password authentication failed?**
A: Wrong password, or the server **disables password login** — switch to private-key auth and install your public key at `~/.ssh/authorized_keys`.

**Q: Private key import fails?**
A: Use an OpenSSH / PEM key (`id_rsa`, `id_ed25519`, …) and pick the file with the [浏览] (browse) button to avoid path-escaping issues. Note: **passphrase-protected keys are not supported** — use an unencrypted copy.

**Q: Started fine but can't reach 127.0.0.1:<port>?**
A: First make sure the tunnel shows 已连接 (green). If the local port is taken by another program or a running tunnel, Tunnel waits and retries as a whole with a visible bind error — free the port or pick another. Also confirm the target service is really listening on `target host:target port` (run `ss -ltn` on the jump host).

**Q: Edits don't seem to take effect?**
A: Running tunnels are not affected by config edits — [⏹ 停止] then [▶ 启动] that tunnel.

**Q: After restarting the app everything shows 未连接 (disconnected)?**
A: Normal — state is not persisted across restarts. Use the tray's 全部连接 to bring everything back.

**Q: Clicked ✕ / closed the widget — is the app still running?**
A: Yes, it just hid to the tray (the tray icon stays). To fully quit use tray → 退出.

**Q: Bugs or feature requests?**
A: Open an [Issue](../../issues) with steps and logs.

---

## Development

```
├── index.html            one shared page; the view is chosen by the window label
├── src/
│   ├── main.ts           frontend logic: widget + config panel (list ↔ edit)
│   └── styles.css        dark theme styles
└── src-tauri/
    ├── src/
    │   ├── config.rs     data model (ids / tunnel list / migration / password crypto)
    │   ├── tunnel.rs     SSH engine: manager registry + per-tunnel runtime with auto-reconnect
    │   └── lib.rs        tray (status-tinted logo + dynamic menu), windows, commands
    └── icons/
        └── logo.svg      logo design source (gateway ring); PNGs/ICO generated from it
```

```bash
npm run tauri dev    # develop
npm run build        # frontend typecheck + build
cargo test           # Rust tests (in src-tauri/, incl. tray icon regression)
npm run tauri build  # package
```

Stack: Tauri 2 · Vanilla TypeScript + Vite · Rust + Tokio · [russh](https://github.com/Eugeny/russh)

---

## License

[Apache License 2.0](LICENSE) · Copyright © 2026 xiaoge
