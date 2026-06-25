import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getVersion } from "@tauri-apps/api/app";
import { open } from "@tauri-apps/plugin-dialog";

// ─── Types ────────────────────────────────────────────────────────────

interface ForwardRule {
  local_port: number;
  target_host: string;
  target_port: number;
}

interface TunnelConfig {
  name: string;
  ssh_host: string;
  ssh_port: number;
  ssh_user: string;
  auth_method:
    | { Password: { password: string } }
    | { Key: { private_key_path: string; passphrase: string | null } };
  forwards: ForwardRule[];
}

interface TunnelMetric {
  name: string;
  status: "Disconnected" | "Connecting" | "Connected" | { Error: string };
  latency_ms: number;
  rx_bytes_per_sec: number;
  tx_bytes_per_sec: number;
}

interface AppConfig {
  tunnel: TunnelConfig | null;
}

// ─── Helpers ──────────────────────────────────────────────────────────

function formatBytes(bytesPerSec: number): string {
  const units = ["B/s", "KB/s", "MB/s", "GB/s", "TB/s"];
  let value = bytesPerSec;
  let unitIndex = 0;
  while (value >= 1000 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex++;
  }
  return `${value.toFixed(unitIndex === 0 ? 0 : 1)} ${units[unitIndex]}`;
}

// ─── Canvas Latency Graph ─────────────────────────────────────────────

class LatencyGraph {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private data: number[];
  private maxPoints: number;
  private maxLatency = 50;
  private stepX: number;
  private gradient: CanvasGradient;
  private glowGradient: CanvasGradient;
  private renderPending = false;

  constructor(canvas: HTMLCanvasElement, maxPoints: number) {
    const ctx = canvas.getContext("2d")!;
    this.canvas = canvas;
    this.ctx = ctx;
    this.data = [];
    this.maxPoints = maxPoints;
    this.stepX = canvas.width / (maxPoints - 1);

    this.gradient = ctx.createLinearGradient(0, 0, 0, canvas.height);
    this.gradient.addColorStop(0, "rgba(0, 245, 160, 0.25)");
    this.gradient.addColorStop(1, "rgba(0, 245, 160, 0.02)");

    this.glowGradient = ctx.createLinearGradient(0, 0, 0, canvas.height);
    this.glowGradient.addColorStop(0, "rgba(0, 245, 160, 0.8)");
    this.glowGradient.addColorStop(1, "rgba(0, 245, 160, 0.1)");
  }

  push(value: number) {
    const arr = this.data;
    arr.push(value);
    if (arr.length > this.maxPoints) arr.shift();
    this.scheduleRender();
  }

  private scheduleRender() {
    if (this.renderPending) return;
    this.renderPending = true;
    requestAnimationFrame(() => {
      this.renderPending = false;
      this.renderNow();
    });
  }

  private renderNow() {
    const { canvas, ctx, data, maxLatency, stepX } = this;
    const w = canvas.width;
    const h = canvas.height;
    ctx.clearRect(0, 0, w, h);

    if (data.length < 2) return;

    const yy = (val: number) => h - (val / maxLatency) * (h - 2) - 1;

    ctx.beginPath();
    const points: { x: number; y: number }[] = [];
    for (let i = 0; i < data.length; i++) {
      const x = w - (data.length - 1 - i) * stepX;
      const val = Math.min(data[i], maxLatency);
      const y = yy(val);
      points.push({ x, y });
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }

    ctx.strokeStyle = this.glowGradient;
    ctx.lineWidth = 1.5;
    ctx.stroke();

    if (points.length > 0) {
      const last = points[points.length - 1];
      ctx.beginPath();
      ctx.arc(last.x, last.y, 3, 0, Math.PI * 2);
      ctx.fillStyle = "#00f5a0";
      ctx.fill();
    }
  }
}

// ─── Mini Widget ──────────────────────────────────────────────────────

class MiniWidget {
  private latencyEl: HTMLElement;
  private rxEl: HTMLElement;
  private txEl: HTMLElement;
  private statusEl: HTMLElement;
  private graph: LatencyGraph;

  constructor() {
    this.latencyEl = document.getElementById("metric-latency")!;
    this.rxEl = document.getElementById("metric-rx")!;
    this.txEl = document.getElementById("metric-tx")!;
    this.statusEl = document.getElementById("status-badge")!;

    const canvas = document.getElementById(
      "latency-canvas",
    ) as HTMLCanvasElement;
    this.graph = new LatencyGraph(canvas, 30);

    document.getElementById("btn-config")!.addEventListener("click", () => {
      invoke("show_config_window").catch(console.error);
    });
    document.getElementById("btn-close")!.addEventListener("click", () => {
      getCurrentWindow().hide().catch(console.error);
    });

    listen<TunnelMetric>("tunnel-metric", (event) => {
      this.update(event.payload);
    }).catch(console.error);
  }

  private update(metric: TunnelMetric) {
    const lat = Math.round(metric.latency_ms);
    this.latencyEl.textContent = `${lat} ms`;
    this.graph.push(metric.latency_ms);

    this.rxEl.textContent = formatBytes(metric.rx_bytes_per_sec);
    this.txEl.textContent = formatBytes(metric.tx_bytes_per_sec);

    const statusStr =
      typeof metric.status === "string" ? metric.status : "Error";
    const statusMap: Record<string, string> = {
      Connected: "已连接",
      Connecting: "连接中…",
      Disconnected: "未连接",
      Error: "错误",
    };
    this.statusEl.textContent = `\u25CF ${statusMap[statusStr] || statusStr}`;
    this.statusEl.className = "tunnel-status-badge";
    if (statusStr === "Connected") this.statusEl.classList.add("connected");
    else if (statusStr === "Connecting")
      this.statusEl.classList.add("connecting");
    else if (statusStr === "Error") this.statusEl.classList.add("error");
    else this.statusEl.classList.add("disconnected");
  }
}

// ─── Config Panel ─────────────────────────────────────────────────────

class ConfigPanel {
  private area: HTMLElement;
  private savedConfig: TunnelConfig | null = null;
  private btnState: "stopped" | "starting" | "started" | "stopping" = "stopped";

  constructor() {
    this.area = document.getElementById("tunnel-editor-area")!;
    this.loadAndBuild();
    this.listenStatus();
  }

  private async loadAndBuild() {
    try {
      const appCfg: AppConfig = await invoke("get_config");
      this.savedConfig = appCfg.tunnel;
    } catch (e) {
      console.error("加载配置失败:", e);
    }
    this.renderEditor(this.savedConfig || this.defaultConfig());
  }

  private defaultConfig(): TunnelConfig {
    return {
      name: "SSH 隧道",
      ssh_host: "",
      ssh_port: 22,
      ssh_user: "root",
      auth_method: { Password: { password: "" } },
      forwards: [
        { local_port: 15432, target_host: "127.0.0.1", target_port: 15432 },
      ],
    };
  }

  // ── build UI dynamically ──────────────────────────────────────────

  private renderEditor(cfg: TunnelConfig) {
    const a = this.area;
    a.innerHTML = "";

    // ── SSH section ──────────────────────────────────────────────────
    const sshSection = el("div", { class: "tunnel-editor" });

    sshSection.appendChild(
      el("input", {
        class: "field-name",
        placeholder: "隧道名称",
        value: cfg.name,
      }),
    );
    const grid = el("div", { class: "editor-grid" });

    const sshHost = el("input", {
      class: "field-ssh-host",
      placeholder: "your-server.com",
      value: cfg.ssh_host,
    }) as HTMLInputElement;
    grid.appendChild(lbl("SSH 主机", sshHost));

    const sshPort = el("input", {
      class: "field-ssh-port",
      type: "number",
      value: String(cfg.ssh_port),
    }) as HTMLInputElement;
    grid.appendChild(lbl("端口", sshPort));

    const sshUser = el("input", {
      class: "field-ssh-user",
      placeholder: "root",
      value: cfg.ssh_user,
    }) as HTMLInputElement;
    grid.appendChild(lbl("用户", sshUser));

    const isKey = "Key" in cfg.auth_method;
    const authType = el("select", {
      class: "field-auth-type",
    }) as HTMLSelectElement;
    authType.innerHTML =
      '<option value="password">密码</option><option value="key">私钥</option>';
    authType.value = isKey ? "key" : "password";
    grid.appendChild(lbl("认证方式", authType));

    const pwInput = el("input", {
      class: "field-password",
      type: "password",
      placeholder: "密码",
    }) as HTMLInputElement;
    if (!isKey) pwInput.value = (cfg.auth_method as any).Password.password;
    const pwLabel = lbl("密码", pwInput);
    grid.appendChild(pwLabel);

    const keyRow = el("span", { class: "key-path-row" });
    const keyInput = el("input", {
      class: "field-key-path",
      placeholder: "/path/to/id_rsa",
    }) as HTMLInputElement;
    if (isKey) keyInput.value = (cfg.auth_method as any).Key.private_key_path;
    const browseBtn = el("button", { class: "btn-browse", text: "浏览" });
    keyRow.appendChild(keyInput);
    keyRow.appendChild(browseBtn);
    const keyLabel = lbl("密钥路径", keyRow);
    grid.appendChild(keyLabel);

    // toggle auth fields
    const toggleAuth = () => {
      const km = authType.value === "key";
      pwLabel.style.display = km ? "none" : "";
      keyLabel.style.display = km ? "" : "none";
    };
    toggleAuth();
    authType.addEventListener("change", toggleAuth);

    browseBtn.addEventListener("click", async (e) => {
      e.stopPropagation();
      const selected = await open({
        multiple: false,
        title: "选择 SSH 私钥文件",
        filters: [
          {
            name: "SSH 密钥",
            extensions: ["pem", "key", "id_rsa", "id_ecdsa", "id_ed25519"],
          },
          {
            name: "所有文件",
            extensions: ["*"],
          },
        ],
      });
      if (selected) keyInput.value = selected;
    });

    sshSection.appendChild(grid);

    // ── Forward rules section ────────────────────────────────────────
    const fwdSection = el("div", { class: "forward-rules" });
    const fwdLabel = el("div", { class: "forward-header" });
    fwdLabel.innerHTML = "<span>端口转发规则</span>";
    const addFwdBtn = el("button", {
      class: "btn-add-forward",
      text: "+ 添加",
    });
    fwdLabel.appendChild(addFwdBtn);
    fwdSection.appendChild(fwdLabel);

    const fwdList = el("div", { class: "forward-list" });

    const renderForwards = () => {
      fwdList.innerHTML = "";
      for (let i = 0; i < cfg.forwards.length; i++) {
        const fwd = cfg.forwards[i];
        const row = el("div", { class: "forward-row" });

        const lpInput = el("input", {
          type: "number",
          placeholder: "本地端口",
          value: String(fwd.local_port),
        }) as HTMLInputElement;
        row.appendChild(lbl("本地", lpInput));

        const thInput = el("input", {
          placeholder: "目标主机",
          value: fwd.target_host,
        }) as HTMLInputElement;
        row.appendChild(lbl("目标", thInput));

        const tpInput = el("input", {
          type: "number",
          placeholder: "端口",
          value: String(fwd.target_port),
        }) as HTMLInputElement;
        row.appendChild(lbl("端口", tpInput));

        const delBtn = el("button", { class: "btn-delete-forward", text: "✕" });
        delBtn.addEventListener("click", () => {
          cfg.forwards.splice(i, 1);
          renderForwards();
        });
        row.appendChild(delBtn);

        // sync inputs to cfg
        const sync = () => {
          fwd.local_port = parseInt(lpInput.value) || 0;
          fwd.target_host = thInput.value.trim();
          fwd.target_port = parseInt(tpInput.value) || 0;
        };
        lpInput.addEventListener("input", sync);
        thInput.addEventListener("input", sync);
        tpInput.addEventListener("input", sync);

        fwdList.appendChild(row);
      }
    };
    renderForwards();

    addFwdBtn.addEventListener("click", () => {
      cfg.forwards.push({
        local_port: 15432,
        target_host: "127.0.0.1",
        target_port: 15432,
      });
      renderForwards();
    });

    fwdSection.appendChild(fwdList);

    // ── Action buttons ───────────────────────────────────────────────
    const actions = el("div", { class: "editor-actions" });
    const startBtn = el("button", {
      class: "btn-start-tunnel",
      text: "▶ 启动",
    }) as HTMLButtonElement;

    // build config from inputs
    const buildConfig = (): TunnelConfig => ({
      name:
        (sshSection.querySelector(".field-name") as HTMLInputElement).value ||
        "未命名",
      ssh_host: sshHost.value.trim(),
      ssh_port: parseInt(sshPort.value) || 22,
      ssh_user: sshUser.value.trim() || "root",
      auth_method:
        authType.value === "key"
          ? {
              Key: {
                private_key_path: keyInput.value.trim(),
                passphrase: null,
              },
            }
          : { Password: { password: pwInput.value } },
      forwards: cfg.forwards.filter(
        (f) => f.local_port > 0 && f.local_port <= 65535,
      ),
    });

    const validate = (c: TunnelConfig): string[] => {
      const e: string[] = [];
      if (!c.ssh_host) e.push("SSH 主机不能为空");
      if (!c.ssh_user) e.push("SSH 用户不能为空");
      if (c.ssh_port < 1 || c.ssh_port > 65535) e.push("SSH 端口无效");
      if (
        !("Key" in c.auth_method) &&
        !(c.auth_method as any).Password.password
      )
        e.push("密码不能为空");
      if (
        "Key" in c.auth_method &&
        !(c.auth_method as any).Key.private_key_path
      )
        e.push("密钥路径不能为空");
      if (c.forwards.length === 0) e.push("至少需要一个转发规则");
      return e;
    };

    startBtn.addEventListener("click", async () => {
      if (this.btnState === "starting" || this.btnState === "stopping") return;

      if (this.btnState === "stopped") {
        const c = buildConfig();
        const errs = validate(c);
        if (errs.length > 0) {
          showToast("请修正", errs.join("\n"));
          return;
        }

        this.btnState = "starting";
        startBtn.textContent = "⏳ 连接中…";
        startBtn.disabled = true;
        this.setEditingEnabled(false);

        try {
          await invoke("save_config", { config: c });
          await invoke("start_tunnel");
        } catch (e: any) {
          this.setStopped(startBtn);
          showToast("启动失败", typeof e === "string" ? e : JSON.stringify(e));
        }
      } else if (this.btnState === "started") {
        this.btnState = "stopping";
        startBtn.textContent = "⏳ 停止中…";
        startBtn.disabled = true;
        try {
          await invoke("stop_tunnel");
        } catch (e: any) {
          console.error("停止失败:", e);
        }
        this.setStopped(startBtn);
      }
    });

    actions.appendChild(startBtn);
    sshSection.appendChild(fwdSection);
    sshSection.appendChild(actions);
    a.appendChild(sshSection);

    // ── 版本号 ─────────────────────────────────────────────────────
    const versionEl = el("div", { class: "config-version" });
    getVersion()
      .then((v) => {
        versionEl.textContent = `v${v}`;
      })
      .catch(() => {
        versionEl.textContent = "v0.1.0";
      });
    a.appendChild(versionEl);
  }

  private setStopped(btn: HTMLButtonElement) {
    this.btnState = "stopped";
    btn.textContent = "▶ 启动";
    btn.className = "btn-start-tunnel";
    btn.disabled = false;
    this.setEditingEnabled(true);
  }

  private setStarted(btn: HTMLButtonElement) {
    this.btnState = "started";
    btn.textContent = "⏹ 停止";
    btn.className = "btn-stop-tunnel";
    btn.disabled = false;
    this.setEditingEnabled(false);
  }

  private setEditingEnabled(enabled: boolean) {
    const els = this.area.querySelectorAll("input, select, button");
    els.forEach((el) => {
      // 不操作启动/停止按钮
      if (
        el.classList.contains("btn-start-tunnel") ||
        el.classList.contains("btn-stop-tunnel")
      )
        return;
      (el as HTMLInputElement).disabled = !enabled;
    });
  }

  private listenStatus() {
    listen<TunnelMetric>("tunnel-metric", (event) => {
      const m = event.payload;
      const status = typeof m.status === "string" ? m.status : "Error";

      const btn = this.area.querySelector(
        ".btn-start-tunnel, .btn-stop-tunnel",
      ) as HTMLButtonElement;
      if (!btn) return;

      if (status === "Connected" && this.btnState !== "started") {
        this.setStarted(btn);
      } else if (status === "Error" && this.btnState === "starting") {
        const errMsg =
          typeof m.status === "object" && "Error" in m.status
            ? (m.status as any).Error
            : "";
        this.setStopped(btn);
        showToast("连接失败", errMsg || "请检查配置后重试");
      } else if (status === "Error" && this.btnState === "started") {
        this.setStopped(btn);
      } else if (
        status === "Disconnected" &&
        (this.btnState === "starting" || this.btnState === "started")
      ) {
        this.setStopped(btn);
      }
    }).catch(console.error);
  }
}

// ─── DOM helpers ──────────────────────────────────────────────────────

function el(tag: string, attrs: Record<string, any> = {}): HTMLElement {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "text") e.textContent = v;
    else if (k === "class") e.className = v;
    else (e as any)[k] = v;
  }
  return e;
}

function lbl(text: string, child: HTMLElement): HTMLElement {
  const l = el("label");
  l.textContent = text;
  l.appendChild(child);
  return l;
}

// ─── Toast ────────────────────────────────────────────────────────────

function showToast(title: string, message: string) {
  const existing = document.getElementById("tunnel-toast");
  if (existing) existing.remove();

  const toast = document.createElement("div");
  toast.id = "tunnel-toast";
  toast.innerHTML = `<strong>${title}</strong><p>${message}</p>`;
  document.body.appendChild(toast);

  setTimeout(() => {
    toast.style.opacity = "0";
    setTimeout(() => toast.remove(), 300);
  }, 3000);

  toast.addEventListener("click", () => toast.remove());
}

// ─── 拖拽实现 ──────────────────────────────────────────────────────────

// 手动拖拽：在 widget-container 上监听 mousedown，调用 Tauri 原生 startDragging。
// 同时阻止按钮/输入框的 mousedown 冒泡，防止点击它们时误触发拖拽。
function setupDragAndButtons(root: HTMLElement) {
  // 交互元素阻止冒泡
  root.querySelectorAll("button, input, select, textarea, a").forEach((el) => {
    el.addEventListener("mousedown", (e) => e.stopPropagation());
  });

  // 整个容器作为拖拽区
  root.addEventListener("mousedown", (e) => {
    // 只有左键点击且目标不是交互元素时才拖拽
    if (e.button !== 0) return;
    // 如果事件是从按钮等交互元素冒泡上来的，已经 stopPropagation 了
    // 所以能到这里的一定是空白区域或文本
    getCurrentWindow()
      .startDragging()
      .catch(() => {});
  });
}

// ─── App Entry ────────────────────────────────────────────────────────

// 1. 顶层防御：用最快速度（最高优先级）卡死全局右键和手势缩放
window.addEventListener("contextmenu", (e) => e.preventDefault(), {
  capture: true,
});
window.addEventListener(
  "wheel",
  (e) => {
    if (e.ctrlKey) e.preventDefault();
  },
  { passive: false },
);

window.addEventListener("DOMContentLoaded", () => {
  const win = getCurrentWindow();
  const label = win.label;

  // 显式切换视图：只显示当前窗口对应的 UI，隐藏另一个
  const miniView = document.getElementById("mini-widget-view")!;
  const configView = document.getElementById("config-panel-view")!;

  if (label === "mini_widget") {
    miniView.style.display = "";
    configView.style.display = "none";
    // 手动拖拽：容器 mousedown → Tauri startDragging
    setupDragAndButtons(miniView);
    new MiniWidget();
  } else if (label === "config_panel") {
    miniView.style.display = "none";
    configView.style.display = "";
    new ConfigPanel();
  }

  // 关闭事件处理
  // config_panel: 不注册任何监听器，走默认关闭行为（窗口被销毁释放内存）
  // mini_widget:  拦截关闭 → hide() 隐藏到后台（Rust 端也有 api.prevent_close 双重保障）
  if (label === "mini_widget") {
    win.onCloseRequested((event) => {
      event.preventDefault();
      win.hide().catch(() => {});
    });
  }
});
