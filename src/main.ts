import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open } from "@tauri-apps/plugin-dialog";

// ─── Types ────────────────────────────────────────────────────────────

interface TunnelMetric {
  id: string;
  name: string;
  status: "Disconnected" | "Connecting" | "Connected" | { Error: string };
  latency_ms: number;
  rx_bytes_per_sec: number;
  tx_bytes_per_sec: number;
  local_port: number;
  target: string;
}

interface TunnelConfig {
  id: string;
  name: string;
  ssh_host: string;
  ssh_port: number;
  ssh_user: string;
  auth_method:
    | { Password: { password: string } }
    | { Key: { private_key_path: string; passphrase: string | null } };
  local_port: number;
  target_host: string;
  target_port: number;
  enabled: boolean;
}

const statusMap: Record<string, string> = {
  Connected: "已连接",
  Connecting: "连接中…",
  Disconnected: "未连接",
  Error: "错误",
};

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

// ─── Canvas Latency Graph Engine (Optimized) ──────────────────────────

class LatencyGraph {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private data: Float64Array;
  private maxPoints: number;
  private maxLatency: number;
  private stepX: number;
  private gradient: CanvasGradient | null = null;
  private glowGradient: CanvasGradient | null = null;
  private renderPending = false;

  constructor(canvas: HTMLCanvasElement, maxPoints = 30) {
    this.canvas = canvas;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("Canvas 2D context unavailable");
    this.ctx = ctx;
    this.data = new Float64Array(maxPoints);
    this.maxPoints = maxPoints;
    this.maxLatency = 200;
    this.stepX = canvas.width / (maxPoints - 1);
    this.clear();
  }

  clear() {
    this.data.fill(0);
    this.renderNow();
  }

  push(value: number) {
    // Shift array left by 1 (不做完整移位，用环形缓冲区更好，但先保持简单)
    const arr = this.data;
    for (let i = 0; i < this.maxPoints - 1; i++) {
      arr[i] = arr[i + 1];
    }
    arr[this.maxPoints - 1] = Math.min(value, this.maxLatency);
    this.scheduleRender();
  }

  private scheduleRender() {
    // 用 requestAnimationFrame 节流，避免每秒 1000 次重绘
    if (this.renderPending) return;
    this.renderPending = true;
    requestAnimationFrame(() => {
      this.renderPending = false;
      this.renderNow();
    });
  }

  private renderNow() {
    const { canvas, ctx, data, maxPoints, maxLatency, stepX } = this;
    const w = canvas.width;
    const h = canvas.height;

    ctx.clearRect(0, 0, w, h);

    if (maxPoints === 0) return;

    // ── 网格背景（缓存成 ImageData 或复用路径） ─────────────────────
    ctx.strokeStyle = "rgba(0, 212, 255, 0.04)";
    ctx.lineWidth = 0.5;
    for (let y = 0; y < 4; y++) {
      const yy = (y / 4) * h;
      ctx.beginPath();
      ctx.moveTo(0, yy);
      ctx.lineTo(w, yy);
      ctx.stroke();
    }

    // ── 计算所有点的坐标 ─────────────────────────────────────────────
    const points: Array<{ x: number; y: number }> = [];
    for (let i = 0; i < maxPoints; i++) {
      const x = i * stepX;
      const val = data[i] / maxLatency;
      const y = h - val * (h - 2) - 1;
      points.push({ x, y });
    }

    // ── 主线条 ─────────────────────────────────────────────────────
    // 缓存渐变（只创建一次）
    if (!this.gradient) {
      this.gradient = ctx.createLinearGradient(0, 0, w, 0);
      this.gradient.addColorStop(0, "rgba(0, 212, 255, 0.2)");
      this.gradient.addColorStop(0.5, "rgba(0, 245, 160, 0.6)");
      this.gradient.addColorStop(1, "rgba(0, 245, 160, 0.9)");
    }

    ctx.strokeStyle = this.gradient;
    ctx.lineWidth = 1.5;
    ctx.lineJoin = "round";
    ctx.lineCap = "round";
    ctx.beginPath();
    ctx.moveTo(points[0].x, points[0].y);
    for (let i = 1; i < points.length; i++) {
      ctx.lineTo(points[i].x, points[i].y);
    }
    ctx.stroke();

    // ── 辉光层（更宽的半透明线） ─────────────────────────────────
    if (!this.glowGradient) {
      this.glowGradient = ctx.createLinearGradient(0, 0, w, 0);
      this.glowGradient.addColorStop(0, "rgba(0, 212, 255, 0.05)");
      this.glowGradient.addColorStop(1, "rgba(0, 245, 160, 0.15)");
    }
    ctx.strokeStyle = this.glowGradient;
    ctx.lineWidth = 4;
    ctx.beginPath();
    ctx.moveTo(points[0].x, points[0].y);
    for (let i = 1; i < points.length; i++) {
      ctx.lineTo(points[i].x, points[i].y);
    }
    ctx.stroke();

    // ── 末端亮点 ───────────────────────────────────────────────────
    const last = points[maxPoints - 1];
    if (data[maxPoints - 1] > 0) {
      ctx.fillStyle = "#00f5a0";
      ctx.shadowColor = "#00f5a0";
      ctx.shadowBlur = 6;
      ctx.beginPath();
      ctx.arc(last.x, last.y, 2.5, 0, Math.PI * 2);
      ctx.fill();
      ctx.shadowBlur = 0;
    }
  }
}

// ─── Mini Widget Logic ────────────────────────────────────────────────

class MiniWidget {
  private latencyEl: HTMLElement;
  private rxEl: HTMLElement;
  private txEl: HTMLElement;
  private statusEl: HTMLElement;
  private graph: LatencyGraph;
  private configBtn: HTMLElement;
  private closeBtn: HTMLElement;

  constructor() {
    this.latencyEl = document.getElementById("metric-latency")!;
    this.rxEl = document.getElementById("metric-rx")!;
    this.txEl = document.getElementById("metric-tx")!;
    this.statusEl = document.getElementById("status-badge")!;
    this.configBtn = document.getElementById("btn-config")!;
    this.closeBtn = document.getElementById("btn-close")!;

    const canvas = document.getElementById(
      "latency-canvas",
    ) as HTMLCanvasElement;
    this.graph = new LatencyGraph(canvas, 30);

    this.setupListeners();
  }

  private setupListeners() {
    // 配置按钮：阻止冒泡防止拖拽触发
    this.configBtn.addEventListener("mousedown", (e) => e.stopPropagation());
    this.configBtn.addEventListener("click", () => {
      invoke("show_config_window").catch(console.error);
    });

    // 关闭按钮：隐藏到系统托盘
    this.closeBtn.addEventListener("mousedown", (e) => e.stopPropagation());
    this.closeBtn.addEventListener("click", () => {
      getCurrentWindow().hide().catch(console.error);
    });

    // 监听后端推送的隧道指标
    listen<TunnelMetric>("tunnel-metric", (event) => {
      this.update(event.payload);
    }).catch(console.error);
  }

  private update(metric: TunnelMetric) {
    // 延时
    const lat = Math.round(metric.latency_ms);
    this.latencyEl.textContent = `${lat} ms`;

    // 更新折线图
    this.graph.push(metric.latency_ms);

    // 吞吐量 - 自动单位
    this.rxEl.textContent = formatBytes(metric.rx_bytes_per_sec);
    this.txEl.textContent = formatBytes(metric.tx_bytes_per_sec);

    // 状态徽章
    const statusStr =
      typeof metric.status === "string" ? metric.status : "Error";

    this.statusEl.textContent = `\u25CF ${statusMap[statusStr] || statusStr}`;
    this.statusEl.className = "tunnel-status-badge";
    if (statusStr === "Connected") {
      this.statusEl.classList.add("connected");
    } else if (statusStr === "Connecting") {
      this.statusEl.classList.add("connecting");
    } else if (statusStr === "Error") {
      this.statusEl.classList.add("error");
    } else {
      this.statusEl.classList.add("disconnected");
    }
  }
}

// ─── Config Panel Logic ───────────────────────────────────────────────

class ConfigPanel {
  private tunnelList: HTMLElement;
  private noTunnelsMsg: HTMLElement;
  private addBtn: HTMLElement;
  private template: HTMLTemplateElement;

  constructor() {
    this.tunnelList = document.getElementById("tunnel-list")!;
    this.noTunnelsMsg = document.getElementById("no-tunnels-msg")!;
    this.addBtn = document.getElementById("btn-add-tunnel")!;
    this.template = document.getElementById(
      "tunnel-editor-template",
    ) as HTMLTemplateElement;

    this.addBtn.addEventListener("click", () => this.addTunnelEditor());
    this.loadAndRender();
  }

  private async loadAndRender() {
    try {
      const tunnels: TunnelConfig[] = await invoke("get_tunnels");
      this.renderTunnels(tunnels);
    } catch (e) {
      console.error("加载隧道配置失败:", e);
    }
  }

  private renderTunnels(tunnels: TunnelConfig[]) {
    this.tunnelList.innerHTML = "";
    if (tunnels.length === 0) {
      this.tunnelList.appendChild(this.noTunnelsMsg);
      return;
    }
    for (const tunnel of tunnels) {
      this.renderTunnelEditor(tunnel);
    }
  }

  private addTunnelEditor() {
    const defaultConfig: TunnelConfig = {
      id: crypto.randomUUID(),
      name: "新建隧道",
      ssh_host: "127.0.0.1",
      ssh_port: 22,
      ssh_user: "root",
      auth_method: { Password: { password: "" } },
      local_port: 5432,
      target_host: "127.0.0.1",
      target_port: 5432,
      enabled: false,
    };
    this.renderTunnelEditor(defaultConfig);
  }

  private renderTunnelEditor(config: TunnelConfig) {
    const clone = this.template.content.cloneNode(true) as DocumentFragment;
    const editor = clone.firstElementChild as HTMLElement;

    const nameInput = editor.querySelector(".field-name") as HTMLInputElement;
    const sshHostInput = editor.querySelector(
      ".field-ssh-host",
    ) as HTMLInputElement;
    const sshPortInput = editor.querySelector(
      ".field-ssh-port",
    ) as HTMLInputElement;
    const sshUserInput = editor.querySelector(
      ".field-ssh-user",
    ) as HTMLInputElement;
    const authTypeSelect = editor.querySelector(
      ".field-auth-type",
    ) as HTMLSelectElement;
    const passwordInput = editor.querySelector(
      ".field-password",
    ) as HTMLInputElement;
    const keyPathInput = editor.querySelector(
      ".field-key-path",
    ) as HTMLInputElement;
    const browseBtn = editor.querySelector(".btn-browse") as HTMLButtonElement;
    const localPortInput = editor.querySelector(
      ".field-local-port",
    ) as HTMLInputElement;
    const targetHostInput = editor.querySelector(
      ".field-target-host",
    ) as HTMLInputElement;
    const targetPortInput = editor.querySelector(
      ".field-target-port",
    ) as HTMLInputElement;
    const startBtn = editor.querySelector(
      ".btn-start-tunnel",
    ) as HTMLButtonElement;
    const deleteBtn = editor.querySelector(
      ".btn-delete-tunnel",
    ) as HTMLButtonElement;

    // 填充字段
    nameInput.value = config.name;
    sshHostInput.value = config.ssh_host;
    sshPortInput.value = String(config.ssh_port);
    sshUserInput.value = config.ssh_user;

    const isKey = "Key" in config.auth_method;
    authTypeSelect.value = isKey ? "key" : "password";

    if (isKey) {
      const k = config.auth_method as {
        Key: { private_key_path: string; passphrase: string | null };
      };
      keyPathInput.value = k.Key.private_key_path;
    } else {
      const p = config.auth_method as { Password: { password: string } };
      passwordInput.value = p.Password.password;
    }

    localPortInput.value = String(config.local_port);
    targetHostInput.value = config.target_host;
    targetPortInput.value = String(config.target_port);

    // 切换密码/密钥字段可见性
    const toggleAuthFields = () => {
      const isKeyMode = authTypeSelect.value === "key";
      const pwLabel = passwordInput.closest("label");
      const keyLabel = keyPathInput.closest("label");
      if (pwLabel) pwLabel.style.display = isKeyMode ? "none" : "";
      if (keyLabel) keyLabel.style.display = isKeyMode ? "" : "none";
    };
    toggleAuthFields();
    authTypeSelect.addEventListener("change", toggleAuthFields);

    // 文件选择器：选择 SSH 私钥
    browseBtn.addEventListener("click", async (e) => {
      e.stopPropagation();
      const selected = await open({
        multiple: false,
        title: "选择 SSH 私钥文件",
        filters: [
          { name: "所有文件", extensions: ["*"] },
          {
            name: "SSH 密钥",
            extensions: ["pem", "key", "id_rsa", "id_ecdsa", "id_ed25519"],
          },
        ],
      });
      if (selected) {
        keyPathInput.value = selected;
      }
    });

    // ── 启动/停止 状态机 ───────────────────────────────────────────
    // btnState: "stopped" | "starting" | "started" | "stopping"
    let btnState: string = "stopped";
    const tunnelId = config.id;

    const buildConfig = (): TunnelConfig => ({
      id: tunnelId,
      name: nameInput.value || "未命名",
      ssh_host: sshHostInput.value.trim(),
      ssh_port: parseInt(sshPortInput.value) || 22,
      ssh_user: sshUserInput.value.trim() || "root",
      auth_method:
        authTypeSelect.value === "key"
          ? {
              Key: {
                private_key_path: keyPathInput.value.trim(),
                passphrase: null,
              },
            }
          : { Password: { password: passwordInput.value } },
      local_port: parseInt(localPortInput.value) || 5432,
      target_host: targetHostInput.value.trim() || "127.0.0.1",
      target_port: parseInt(targetPortInput.value) || 5432,
      enabled: true,
    });

    const validate = (cfg: TunnelConfig): string[] => {
      const errs: string[] = [];
      if (!cfg.ssh_host) errs.push("SSH 主机不能为空");
      if (!cfg.ssh_user) errs.push("SSH 用户不能为空");
      if (cfg.ssh_port < 1 || cfg.ssh_port > 65535)
        errs.push("SSH 端口无效 (1-65535)");
      const isKeyMode = "Key" in cfg.auth_method;
      if (!isKeyMode && !(cfg.auth_method as any).Password.password)
        errs.push("密码不能为空");
      if (isKeyMode && !(cfg.auth_method as any).Key.private_key_path)
        errs.push("密钥路径不能为空");
      if (cfg.local_port < 1 || cfg.local_port > 65535)
        errs.push("本地端口无效 (1-65535)");
      if (!cfg.target_host) errs.push("目标主机不能为空");
      if (cfg.target_port < 1 || cfg.target_port > 65535)
        errs.push("目标端口无效 (1-65535)");
      return errs;
    };

    // ── 从 tunnel-metric 事件中提取状态字符串 ────────────────────
    const extractStatus = (
      m: TunnelMetric,
    ): { status: string; error?: string } => {
      if (typeof m.status === "string") return { status: m.status };
      if (m.status && typeof m.status === "object" && "Error" in m.status) {
        return { status: "Error", error: (m.status as any).Error };
      }
      return { status: "Disconnected" };
    };

    // ── 按钮 UI 更新 ───────────────────────────────────────────────
    const setBtnStart = () => {
      btnState = "stopped";
      startBtn.textContent = "▶ 启动";
      startBtn.className = "btn-start-tunnel";
      startBtn.disabled = false;
      deleteBtn.disabled = false;
    };

    const setBtnStarting = () => {
      btnState = "starting";
      startBtn.textContent = "⏳ 连接中…";
      startBtn.disabled = true;
      deleteBtn.disabled = true;
    };

    const setBtnStarted = () => {
      btnState = "started";
      startBtn.textContent = "⏹ 停止";
      startBtn.className = "btn-stop-tunnel";
      startBtn.disabled = false;
      deleteBtn.disabled = true;
    };

    const setBtnStopping = () => {
      btnState = "stopping";
      startBtn.textContent = "⏳ 停止中…";
      startBtn.disabled = true;
      deleteBtn.disabled = true;
    };

    // ── 点击处理 ───────────────────────────────────────────────────
    startBtn.addEventListener("click", async () => {
      if (btnState === "starting" || btnState === "stopping") return;

      if (btnState === "stopped") {
        const cfg = buildConfig();
        const errs = validate(cfg);
        if (errs.length > 0) {
          showToast("请修正以下问题", errs.join("\n"));
          return;
        }

        setBtnStarting();

        try {
          await invoke("start_tunnel", { config: cfg });
          // 不立即切换按钮，等后端 Connected 事件再切
          // 但如果 invoke 本身抛异常（权限、序列化等问题）
        } catch (e: any) {
          setBtnStart();
          showToast("启动失败", typeof e === "string" ? e : JSON.stringify(e));
        }
      } else if (btnState === "started") {
        setBtnStopping();
        try {
          await invoke("stop_tunnel", { id: tunnelId });
        } catch (e: any) {
          console.error("停止隧道失败:", e);
        }
        setBtnStart();
      }
    });

    // ── 监听后端状态事件，驱动按钮状态 ──────────────────────────────
    const cfgName = config.name;
    listen<TunnelMetric>("tunnel-metric", (event) => {
      const m = event.payload;
      if (m.id !== tunnelId) return;

      const { status, error } = extractStatus(m);

      if (status === "Connected" && btnState === "starting") {
        // SSH 连接成功，允许停止
        setBtnStarted();
      } else if (
        status === "Error" &&
        (btnState === "starting" || btnState === "started")
      ) {
        // 连接失败（密码错误等）→ 恢复启动按钮
        setBtnStart();
        showToast(`${cfgName} 连接失败`, error || "请检查配置后重试");
      } else if (status === "Disconnected" && btnState === "started") {
        // 意外断开
        setBtnStart();
      }
    });

    // 删除
    deleteBtn.addEventListener("click", () => {
      editor.remove();
    });

    this.noTunnelsMsg.style.display = "none";
    this.tunnelList.appendChild(editor);
  }
}

// ─── Toast 通知 ───────────────────────────────────────────────────────

// 轻量级浮动通知，替代浏览器原生 alert
function showToast(title: string, message: string) {
  const existing = document.getElementById("tunnel-toast");
  if (existing) existing.remove();

  const toast = document.createElement("div");
  toast.id = "tunnel-toast";
  toast.innerHTML = `<strong>${title}</strong><p>${message}</p>`;
  document.body.appendChild(toast);

  // 3 秒后自动消失
  setTimeout(() => {
    toast.style.opacity = "0";
    setTimeout(() => toast.remove(), 300);
  }, 3000);

  // 点击关闭
  toast.addEventListener("click", () => {
    toast.remove();
  });
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
