import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import { open, confirm } from "@tauri-apps/plugin-dialog";

// ─── 类型:与 src-tauri/src/config.rs 的 serde 形状一一对应 ───────────
// 修改 Rust 侧结构时务必同步这里(snake_case 字段名、外部标签枚举)。

interface ForwardRule {
  local_port: number;
  target_host: string;
  target_port: number;
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
  forwards: ForwardRule[];
}

type StatusKind = "Connected" | "Connecting" | "Disconnected" | "Error";

type RawStatus = StatusKind | { Error: string };

interface TunnelMetric {
  id: string;
  name: string;
  status: RawStatus;
  rx_bytes_per_sec: number;
  tx_bytes_per_sec: number;
}

interface AppConfig {
  tunnels: TunnelConfig[];
}

// ─── 状态 helper ──────────────────────────────────────────────────────

function statusKindOf(raw: RawStatus): StatusKind {
  return typeof raw === "string" ? raw : "Error";
}

function statusZhOf(raw: RawStatus): string {
  switch (statusKindOf(raw)) {
    case "Connected":
      return "已连接";
    case "Connecting":
      return "连接中";
    case "Error":
      return "错误";
    case "Disconnected":
      return "未连接";
  }
}

function statusErrorOf(raw: RawStatus): string {
  return typeof raw === "object" && "Error" in raw ? raw.Error : "";
}

function cssKindOf(raw: RawStatus): string {
  return statusKindOf(raw).toLowerCase();
}

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

// 单次委托监听:空白区域 mousedown → Tauri 原生 startDragging;
// 交互元素(button/input 等)不拖拽。行/卡片是动态挂载的,因此不逐个绑定。
function setupDragAndButtons(root: HTMLElement) {
  root.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    const target = e.target as HTMLElement;
    if (target.closest("button, input, select, textarea, a")) return;
    getCurrentWindow()
      .startDragging()
      .catch(() => {});
  });
}

// ─── 启停按钮渲染 helper(行与卡片共用)──────────────────────────────

function renderCtlButton(
  btn: HTMLButtonElement,
  kind: StatusKind,
  busy: "start" | "stop" | null,
) {
  btn.disabled = false;
  btn.className = "btn-ctl";
  if (busy === "start") {
    btn.textContent = "⏳ 连接中…";
    btn.disabled = true;
  } else if (busy === "stop") {
    btn.textContent = "⏳ 停止中…";
    btn.disabled = true;
  } else if (kind === "Connected") {
    btn.textContent = "⏹ 停止";
    btn.classList.add("btn-stop-tunnel");
  } else if (kind === "Connecting") {
    btn.textContent = "连接中…";
    btn.disabled = true;
    btn.classList.add("btn-connecting");
  } else {
    btn.textContent = "▶ 启动";
    btn.classList.add("btn-start-tunnel");
  }
}

/// 点击后置 busy;等待 metric 事件驱动按钮回到稳态。invoke 报错时
/// 立即清 busy(已在运行等错误不发事件,由 toast/静默兜底)。
async function clickCtl(
  btn: HTMLButtonElement,
  setBusy: (b: "start" | "stop" | null) => void,
  kind: StatusKind,
  getBusy: () => "start" | "stop" | null,
  id: string,
) {
  if (kind === "Connected") {
    setBusy("stop");
    try {
      await invoke("stop_tunnel", { id });
    } catch (e) {
      showToast("停止失败", String(e));
      setBusy(null);
    }
  } else if (kind !== "Connecting") {
    setBusy("start");
    try {
      await invoke("start_tunnel", { id });
    } catch (e) {
      const msg = typeof e === "string" ? e : JSON.stringify(e);
      if (!msg.includes("已在运行")) showToast("启动失败", msg);
      setBusy(null);
    }
  }
  renderCtlButton(btn, kind, getBusy());
}

// ─── 迷你状态面板:每隧道一行 ─────────────────────────────────────────

interface RowState {
  id: string;
  name: string;
  raw: RawStatus;
  /// 上一次渲染的 kind;busy 状态只在 kind 变化时解除。
  prevKind: StatusKind;
  busy: "start" | "stop" | null;
  root: HTMLElement;
  els: {
    dot: HTMLSpanElement;
    nameEl: HTMLSpanElement;
    statusEl: HTMLSpanElement;
    rates: HTMLSpanElement;
    btn: HTMLButtonElement;
  };
}

const WIDGET_WIDTH = 340;
const WIDGET_MIN_H = 200;
const WIDGET_MAX_H = 420;

class MiniWidget {
  private rowsEl: HTMLElement;
  private headerEl: HTMLElement;
  private rows = new Map<string, RowState>();

  constructor() {
    this.rowsEl = document.getElementById("tunnel-rows")!;
    this.headerEl = document.getElementById("widget-header")!;

    document.getElementById("btn-config")!.addEventListener("click", () => {
      invoke("show_config_window").catch(console.error);
    });
    document.getElementById("btn-close")!.addEventListener("click", () => {
      getCurrentWindow().hide().catch(console.error);
    });

    listen<TunnelMetric>("tunnel-metric", (event) => {
      this.onMetric(event.payload);
    }).catch(console.error);
    // 配置面板保存后重建列表(新增/改名/删除)
    listen("config-updated", () => {
      this.load().catch(console.error);
    }).catch(console.error);

    this.load().catch(console.error);
  }

  private async load() {
    const appCfg: AppConfig = await invoke("get_config");
    // 快照补齐事件缺口:窗口刚打开时可能错过 Connecting 等瞬态
    const snaps: TunnelMetric[] = await invoke("get_tunnel_statuses");
    const snapMap = new Map(snaps.map((m) => [m.id, m]));

    this.rowsEl.innerHTML = "";
    this.rows.clear();

    if (appCfg.tunnels.length === 0) {
      const hint = el("div", { class: "widget-empty", text: "暂无隧道,点击 ⚙ 添加" });
      this.rowsEl.appendChild(hint);
    }
    for (const t of appCfg.tunnels) {
      const st = this.makeRow(t.id, t.name);
      // 无快照的行也要走 applyRaw:按钮/点/状态词初态统一在此渲染
      this.applyRaw(st, snapMap.get(t.id)?.status ?? "Disconnected");
      this.rows.set(t.id, st);
      this.rowsEl.appendChild(st.root);
    }
    this.applyWindowSize();
  }

  private makeRow(id: string, name: string): RowState {
    const dot = el("span", { class: "row-dot disconnected" }) as HTMLSpanElement;
    const nameEl = el("span", { class: "row-name", text: name }) as HTMLSpanElement;
    // 状态文字与速率共用第二行:状态词着色,速率弱化
    const statusEl = el("span", {
      class: "row-status disconnected",
      text: "未连接",
    }) as HTMLSpanElement;
    const rates = el("span", {
      class: "row-rates",
      text: "▼ 0 B/s  ▲ 0 B/s",
    }) as HTMLSpanElement;
    const btn = el("button", { class: "btn-ctl" }) as HTMLButtonElement;

    const st: RowState = {
      id,
      name,
      raw: "Disconnected",
      prevKind: "Disconnected",
      busy: null,
      root: el("div", { class: "tunnel-row" }) as HTMLElement,
      els: { dot, nameEl, statusEl, rates, btn },
    };

    const main = el("div", { class: "row-main" });
    const meta = el("div", { class: "row-meta" });
    meta.appendChild(statusEl);
    meta.appendChild(rates);
    main.appendChild(nameEl);
    main.appendChild(meta);
    st.root.appendChild(dot);
    st.root.appendChild(main);
    st.root.appendChild(btn);

    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      clickCtl(btn, (b) => (st.busy = b), statusKindOf(st.raw), () => st.busy, id);
    });
    return st;
  }

  /// 刷新一行视觉:按钮/状态词/点无条件渲染(1Hz tick 的 DOM 开销可忽略);
  /// busy 只在 kind 真正变化时解除,避免被同 kind 的重复事件提前打断。
  private applyRaw(st: RowState, raw: RawStatus) {
    const kind = statusKindOf(raw);
    const kindChanged = kind !== st.prevKind;
    st.prevKind = kind;
    st.raw = raw;
    if (st.busy && kindChanged) st.busy = null; // 转换完成,按钮回稳态
    renderCtlButton(st.els.btn, kind, st.busy);
    st.els.dot.className = `row-dot ${cssKindOf(raw)}`;
    st.els.statusEl.textContent = statusZhOf(raw);
    st.els.statusEl.className = `row-status ${cssKindOf(raw)}`;
    // 错误详情放悬浮提示;状态面板不弹 toast
    const err = kind === "Error" ? statusErrorOf(raw) : "";
    st.els.statusEl.title = err;
    st.els.dot.title = err;
  }

  private onMetric(m: TunnelMetric) {
    const st = this.rows.get(m.id);
    if (!st) return; // 未知(已删除)id 的事件直接丢弃
    st.els.rates.textContent = `▼ ${formatBytes(m.rx_bytes_per_sec)}   ▲ ${formatBytes(m.tx_bytes_per_sec)}`;
    this.applyRaw(st, m.status);
  }

  /// 窗口高度随行数自适应:头部高 + 行区内容高 + 留白,钳制在 [200, 420]。
  private applyWindowSize() {
    const rowsH = this.rowsEl.scrollHeight || 40;
    const headerH = this.headerEl.offsetHeight || 32;
    const h = Math.min(WIDGET_MAX_H, Math.max(WIDGET_MIN_H, headerH + rowsH + 16));
    getCurrentWindow()
      .setSize(new LogicalSize(WIDGET_WIDTH, h))
      .catch(() => {});
  }
}

// ─── 配置面板:主从式(列表 ↔ 编辑) ─────────────────────────────────

/// 卡片上随 metric 事件更新的元素。
interface CardEls {
  dot: HTMLSpanElement;
  chip: HTMLSpanElement;
  btn: HTMLButtonElement;
  err: HTMLDivElement;
}

class ConfigPanel {
  private area: HTMLElement;
  private titleEl: HTMLElement;
  private mode: "list" | "edit" = "list";
  private tunnels: TunnelConfig[] = [];
  /// 卡片状态表:快照 + tunnel-metric 事件;编辑模式下只更新表,不碰 DOM。
  private statuses = new Map<string, RawStatus>();
  private cardEls = new Map<string, CardEls>();
  private editId: string | null = null; // null = 新建
  private allBusy = false;

  constructor() {
    this.area = document.getElementById("tunnel-editor-area")!;
    this.titleEl = document.getElementById("config-title")!;

    listen<TunnelMetric>("tunnel-metric", (event) => {
      const m = event.payload;
      this.statuses.set(m.id, m.status);
      // 卡片视觉随事件驱动(列表已渲染时);未知(已删除)id 直接丢弃
      const els = this.cardEls.get(m.id);
      if (this.mode === "list" && els) this.applyCardVisuals(els, m.status);
    }).catch(console.error);

    this.load().catch(console.error);
  }

  private async load() {
    const [appCfg, snaps] = await Promise.all([
      invoke<AppConfig>("get_config"),
      invoke<TunnelMetric[]>("get_tunnel_statuses"),
    ]);
    this.tunnels = appCfg.tunnels;
    this.statuses = new Map(snaps.map((m) => [m.id, m.status]));
    this.renderList();
  }

  // ── 列表视图 ──────────────────────────────────────────────────────

  private renderList() {
    this.mode = "list";
    this.titleEl.textContent = "Tunnel - 配置";
    this.area.innerHTML = "";
    this.cardEls.clear();

    // 工具栏:添加 / 全部连接 / 全部停止
    const toolbar = el("div", { class: "config-actions toolbar" });
    const addBtn = el("button", { class: "btn-primary", text: "+ 添加隧道" }) as HTMLButtonElement;
    addBtn.addEventListener("click", () => this.openEditor(null));
    const allStart = el("button", { class: "btn-secondary", text: "▶ 全部连接" }) as HTMLButtonElement;
    allStart.addEventListener("click", () => this.toggleAll("start"));
    const allStop = el("button", { class: "btn-secondary", text: "⏹ 全部停止" }) as HTMLButtonElement;
    allStop.addEventListener("click", () => this.toggleAll("stop"));
    toolbar.appendChild(addBtn);
    toolbar.appendChild(allStart);
    toolbar.appendChild(allStop);
    this.area.appendChild(toolbar);

    if (this.tunnels.length === 0) {
      const empty = el("div", {
        id: "no-tunnels-msg",
        text: "暂无隧道,点击上方 [+ 添加隧道] 创建第一个连接",
      });
      this.area.appendChild(empty);
      return;
    }

    const list = el("div", { class: "tunnel-list" });
    for (const t of this.tunnels) {
      list.appendChild(this.makeCard(t));
    }
    this.area.appendChild(list);
  }

  private makeCard(t: TunnelConfig): HTMLElement {
    const raw = this.statuses.get(t.id) ?? "Disconnected";

    const card = el("div", { class: "tunnel-card" }) as HTMLElement;

    const dot = el("span", { class: "row-dot" }) as HTMLSpanElement;
    const nameEl = el("div", { class: "card-name", text: t.name }) as HTMLDivElement;
    const subEl = el("div", {
      class: "card-sub",
      text: `${t.ssh_user}@${t.ssh_host}:${t.ssh_port} · ${t.forwards.length} 条转发`,
    }) as HTMLDivElement;
    const chip = el("span", { class: "tunnel-status-badge" }) as HTMLSpanElement;
    // 错误原因内联展示(截断),悬停可看全文
    const errEl = el("div", { class: "card-error" }) as HTMLDivElement;

    const head = el("div", { class: "card-head" });
    const titleBox = el("div", { class: "card-title" });
    titleBox.appendChild(nameEl);
    titleBox.appendChild(subEl);
    titleBox.appendChild(errEl);
    head.appendChild(dot);
    head.appendChild(titleBox);
    head.appendChild(chip);
    card.appendChild(head);

    const actions = el("div", { class: "card-actions" });
    const ctl = el("button", { class: "btn-ctl" }) as HTMLButtonElement;
    const els: CardEls = { dot, chip, btn: ctl, err: errEl };
    this.applyCardVisuals(els, raw); // 初始状态一次到位(含错误消息 title)
    this.cardEls.set(t.id, els);

    const editBtn = el("button", { class: "btn-secondary", text: "编辑" }) as HTMLButtonElement;
    editBtn.addEventListener("click", () => this.openEditor(t.id));
    const delBtn = el("button", { class: "btn-delete-tunnel", text: "删除" }) as HTMLButtonElement;
    delBtn.addEventListener("click", () => this.deleteTunnel(t));

    ctl.addEventListener("click", () => {
      const kind = statusKindOf(this.statuses.get(t.id) ?? "Disconnected");
      // 按钮视觉由 metric 事件驱动,这里只负责发起调用
      clickCtl(ctl, () => {}, kind, () => null, t.id);
    });
    actions.appendChild(ctl);
    actions.appendChild(editBtn);
    actions.appendChild(delBtn);
    card.appendChild(actions);
    return card;
  }

  /// 卡片视觉的唯一入口:点色 / 徽章文案与颜色 / 错误内联与悬浮详情 /
  /// 启停按钮。
  private applyCardVisuals(els: CardEls, raw: RawStatus) {
    const kind = statusKindOf(raw);
    els.dot.className = `row-dot ${cssKindOf(raw)}`;
    els.chip.textContent = statusZhOf(raw);
    els.chip.className = `tunnel-status-badge ${cssKindOf(raw)}`;
    const err = kind === "Error" ? statusErrorOf(raw) : "";
    els.chip.title = err;
    els.err.textContent = err;
    els.err.title = err;
    els.err.style.display = err ? "" : "none";
    renderCtlButton(els.btn, kind, null);
  }

  private toggleAll(target: "start" | "stop") {
    if (this.allBusy) return;
    this.allBusy = true;
    const jobs: Promise<unknown>[] = [];
    for (const t of this.tunnels) {
      const kind = statusKindOf(this.statuses.get(t.id) ?? "Disconnected");
      if (target === "start" && (kind === "Connected" || kind === "Connecting")) continue;
      if (target === "stop" && kind !== "Connected" && kind !== "Connecting") continue;
      jobs.push(
        invoke(target === "start" ? "start_tunnel" : "stop_tunnel", { id: t.id }).catch((e) => {
          showToast(target === "start" ? "启动失败" : "停止失败", String(e));
        }),
      );
    }
    Promise.allSettled(jobs).finally(() => {
      this.allBusy = false;
    });
  }

  private async deleteTunnel(t: TunnelConfig) {
    const ok = await confirm(`确定删除隧道 “${t.name}” 吗?`, {
      title: "删除隧道",
      kind: "warning",
    }).catch(() => false);
    if (!ok) return;
    // 后端 prune 会自动停止运行中的隧道;本窗随后收到 Disconnected 事件
    // 时该 id 已不在卡片集合里 → 直接丢弃。
    this.tunnels = this.tunnels.filter((x) => x.id !== t.id);
    try {
      await this.saveAndAdopt();
      this.renderList(); // 删除成功:重画列表移除该卡
    } catch (e) {
      this.tunnels.push(t); // 保存失败回滚
      this.renderList();
      showToast("删除失败", String(e));
    }
  }

  /// 保存当前列表,采纳后端返回(新条目会拿到分配好的 id)。
  private async saveAndAdopt() {
    const saved: AppConfig = await invoke("save_config", { tunnels: this.tunnels });
    this.tunnels = saved.tunnels;
  }

  // ── 编辑视图(新建/编辑共用) ─────────────────────────────────────

  /// 深拷贝(配置是纯 JSON,编辑器内直接改副本,取消不改原数据)。
  private cloneTunnel(t: TunnelConfig): TunnelConfig {
    return JSON.parse(JSON.stringify(t)) as TunnelConfig;
  }

  private async openEditor(id: string | null) {
    this.mode = "edit";
    this.editId = id;
    const cfg: TunnelConfig = id
      ? this.cloneTunnel(this.tunnels.find((t) => t.id === id)!)
      : this.defaultConfig();
    this.titleEl.textContent = id ? "编辑隧道" : "新建隧道";
    this.renderEditor(cfg);
  }

  private defaultConfig(): TunnelConfig {
    return {
      id: "",
      name: "新隧道",
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
    const addFwdBtn = el("button", { class: "btn-add-forward", text: "+ 添加" });
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

    // build config from inputs
    const buildConfig = (): TunnelConfig => ({
      id: cfg.id,
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

    const saveBtn = el("button", { class: "btn-primary", text: "💾 保存" }) as HTMLButtonElement;
    saveBtn.addEventListener("click", async () => {
      const c = buildConfig();
      const errs = validate(c);
      if (errs.length > 0) {
        showToast("请修正", errs.join("\n"));
        return;
      }
      let prevIdx = -1;
      let prevEntry: TunnelConfig | null = null;
      if (this.editId) {
        prevIdx = this.tunnels.findIndex((t) => t.id === this.editId);
        if (prevIdx >= 0) {
          prevEntry = this.cloneTunnel(this.tunnels[prevIdx]);
          this.tunnels[prevIdx] = c;
        }
      } else {
        prevIdx = this.tunnels.length;
        this.tunnels.push(c); // id 为空,由 Rust 分配后经返回采纳
      }
      try {
        await this.saveAndAdopt();
        this.renderList();
      } catch (e) {
        // 保存失败:回滚本地修改,重新进编辑器避免丢数据
        this.tunnels.splice(prevIdx, 1);
        if (prevEntry) this.tunnels.splice(prevIdx, 0, prevEntry);
        showToast("保存失败", typeof e === "string" ? e : JSON.stringify(e));
        this.openEditor(this.editId);
      }
    });

    const backBtn = el("button", { class: "btn-secondary", text: "← 返回列表" }) as HTMLButtonElement;
    backBtn.addEventListener("click", () => this.renderList());

    actions.appendChild(backBtn);
    actions.appendChild(saveBtn);
    sshSection.appendChild(fwdSection);
    sshSection.appendChild(actions);
    a.appendChild(sshSection);
  }
}

// ─── App Entry ────────────────────────────────────────────────────────

window.addEventListener("DOMContentLoaded", () => {
  const win = getCurrentWindow();
  const label = win.label;

  // 显式切换视图:只显示当前窗口对应的 UI,隐藏另一个
  const miniView = document.getElementById("mini-widget-view")!;
  const configView = document.getElementById("config-panel-view")!;

  if (label === "mini_widget") {
    miniView.style.display = "";
    configView.style.display = "none";
    setupDragAndButtons(miniView);
    new MiniWidget();
  } else if (label === "config_panel") {
    miniView.style.display = "none";
    configView.style.display = "";
    new ConfigPanel();
  }

  // 关闭事件处理
  // config_panel: 不注册任何监听器,走默认关闭行为(窗口被销毁释放内存)
  // mini_widget:  拦截关闭 → hide() 隐藏到后台(Rust 端也有 api.prevent_close 双重保障)
  if (label === "mini_widget") {
    win.onCloseRequested((event) => {
      event.preventDefault();
      win.hide().catch(() => {});
    });
  }
});
