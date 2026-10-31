// The dashboard: polls the server's JSON and fills the windows in, in the
// manner of the EVE client: overview-style lists sorted by any column, the
// game's security colours, [CORP] and <ALLIANCE> tickers, EVE time.

interface Summary {
  players: number;
  bots: { total: number; online: number; inSpace: number; running: boolean };
  corporations: number;
  alliances: number;
  wars: number;
  lostInFights: number;
  systemsHeld: number;
  uptimeSeconds: number;
}

interface War {
  declaredBy: string;
  declaredByTicker: string;
  against: string;
  againstTicker: string;
  declared: number | null;
  ends: number | null;
  mutual: boolean;
  bots: boolean;
  losses: number;
}

interface Player {
  name: string;
  ticker: string | null;
  alliance: string | null;
  ship: string | null;
  system?: string;
  systemSecurity?: number;
  docked?: boolean;
}

interface SystemRow {
  name: string;
  security: number;
  region: string;
  inSpace: number;
  docked: number;
}

interface AllianceRow {
  name: string;
  ticker: string;
  corporations: number;
  pilots: number;
  kind: string;
  region: string | null;
  systems: number;
}

interface NewsItem {
  time: number;
  text: string;
}

interface CorporationRow {
  name: string;
  ticker: string;
  members: number;
  alliance: string | null;
  bots: boolean;
}

interface Kill {
  time: number | null;
  victim: string | null;
  victimCorporation: string | null;
  ship: string | null;
  system: string | null;
  finalBlow: string | null;
  pod: boolean;
}

/// One column of a list: its heading, what it sorts by, and its cell.
interface Column<T> {
  title: string;
  num?: boolean;
  key: (item: T) => string | number;
  cell: (item: T) => (Node | string)[];
}

// A module, so nothing here is mistaken for the window's own globals.
export {};

const REFRESH_MS = 15_000;
const numbers = new Intl.NumberFormat("en-US");

async function get<T>(path: string): Promise<T> {
  const response = await fetch(path, { cache: "no-store" });
  if (!response.ok) {
    throw new Error(`${path}: ${response.status}`);
  }
  return (await response.json()) as T;
}

function span(text: string, className: string): HTMLElement {
  const e = document.createElement("span");
  e.textContent = text;
  e.className = className;
  return e;
}

const corpTicker = (t: string | null | undefined) => span(t ? ` [${t}]` : "", "tick");
const allianceTicker = (t: string | null | undefined) => span(t ? ` <${t}>` : "", "tick");

/// The security status colours of the client's map and overview, 1.0 to 0.0.
const SECURITY_COLOURS = ["#f00000", "#d73000", "#f04800", "#f06000", "#d77700", "#efef00", "#8fef2f", "#00f000", "#00ef47", "#48f0c0", "#2fefef"];

function security(s: number): HTMLElement {
  const rounded = Math.max(-1, Math.min(1, Math.round(s * 10) / 10));
  const e = span(rounded.toFixed(1), "sec");
  e.style.color = SECURITY_COLOURS[Math.max(0, Math.round(rounded * 10))];
  return e;
}

const pad = (n: number) => String(n).padStart(2, "0");

/// EVE time is UTC, written YYYY.MM.DD HH:MM.
function eveTime(ms: number | null): string {
  if (!ms) {
    return "";
  }
  const d = new Date(ms);
  return `${d.getUTCFullYear()}.${pad(d.getUTCMonth() + 1)}.${pad(d.getUTCDate())} ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`;
}

// ---- lists ------------------------------------------------------------------------

interface List {
  columns: Column<any>[];
  items: any[];
  sortBy: number;
  descending: boolean;
  empty: string;
}

const lists = new Map<string, List>();

/// Shows a list, sorted as its reader last chose (or by its first column).
function list<T>(id: string, columns: Column<T>[], items: T[], empty: string, sortBy = 0, descending = false): void {
  const known = lists.get(id);
  lists.set(id, {
    columns,
    items,
    empty,
    sortBy: known?.sortBy ?? sortBy,
    descending: known?.descending ?? descending,
  });
  draw(id);
}

function draw(id: string): void {
  const l = lists.get(id);
  const table = document.getElementById(id) as HTMLTableElement | null;
  if (!l || !table) {
    return;
  }
  const column = l.columns[l.sortBy];
  const items = [...l.items].sort((a, b) => {
    const x = column.key(a);
    const y = column.key(b);
    const order = typeof x === "number" && typeof y === "number" ? x - y : String(x).localeCompare(String(y));
    return l.descending ? -order : order;
  });
  table.replaceChildren();
  const head = table.createTHead().insertRow();
  l.columns.forEach((c, i) => {
    const th = document.createElement("th");
    th.textContent = c.title;
    th.className = [c.num ? "num" : "", i === l.sortBy ? "sorted" : "", i === l.sortBy && !l.descending ? "up" : ""].join(" ").trim();
    th.addEventListener("click", () => {
      l.descending = l.sortBy === i ? !l.descending : !!c.num;
      l.sortBy = i;
      draw(id);
    });
    head.append(th);
  });
  const body = table.createTBody();
  if (items.length === 0) {
    const td = body.insertRow().insertCell();
    td.colSpan = l.columns.length;
    td.className = "empty";
    td.textContent = l.empty;
    return;
  }
  for (const item of items) {
    const tr = body.insertRow();
    for (const c of l.columns) {
      const td = tr.insertCell();
      if (c.num) {
        td.className = "num";
      }
      td.append(...c.cell(item));
    }
  }
}

const count = (n: number) => [numbers.format(n)];

// ---- windows ----------------------------------------------------------------------

function showStatus(s: Summary): void {
  const rows: [string, string][] = [
    ["Pilots online", numbers.format(s.players)],
    ["Capsuleers simulated", numbers.format(s.bots.total)],
    ["Simulated online", numbers.format(s.bots.online)],
    ["Simulated in space", numbers.format(s.bots.inSpace)],
    ["Corporations", numbers.format(s.corporations)],
    ["Alliances", numbers.format(s.alliances)],
    ["Wars under way", numbers.format(s.wars)],
    ["Ships lost in war", numbers.format(s.lostInFights)],
    ["Sovereign systems", numbers.format(s.systemsHeld)],
    ["Server up", uptime(s.uptimeSeconds)],
  ];
  const dl = document.getElementById("status") as HTMLElement;
  dl.replaceChildren(
    ...rows.flatMap(([label, value]) => {
      const dt = document.createElement("dt");
      dt.textContent = label;
      const dd = document.createElement("dd");
      dd.textContent = value;
      return [dt, dd];
    }),
  );
  const online = s.players + s.bots.online;
  const line = document.getElementById("statusline") as HTMLElement;
  line.textContent = `Status: OK, ${numbers.format(online)} players${s.bots.running ? "" : " (simulation paused)"}`;
}

function uptime(seconds: number): string {
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  return d > 0 ? `${d}d ${h}h ${m}m` : h > 0 ? `${h}h ${m}m` : `${m}m`;
}

async function refresh(): Promise<void> {
  try {
    const [summary, wars, players, systems, alliances, corporations, kills, news] = await Promise.all([
      get<Summary>("/api/summary"),
      get<War[]>("/api/wars"),
      get<Player[]>("/api/players"),
      get<SystemRow[]>("/api/systems"),
      get<AllianceRow[]>("/api/alliances"),
      get<CorporationRow[]>("/api/corporations"),
      get<Kill[]>("/api/kills"),
      get<NewsItem[]>("/api/news"),
    ]);
    showStatus(summary);

    list<War>("wars", [
      { title: "Aggressor", key: (w) => w.declaredBy, cell: (w) => [w.declaredBy, allianceTicker(w.declaredByTicker)] },
      { title: "", key: () => "", cell: () => [span("vs", "hostile")] },
      { title: "Defender", key: (w) => w.against, cell: (w) => [w.against, allianceTicker(w.againstTicker)] },
      { title: "Declared", key: (w) => w.declared ?? 0, cell: (w) => [eveTime(w.declared)] },
      { title: "Ends", key: (w) => w.ends ?? 0, cell: (w) => [eveTime(w.ends)] },
      { title: "Losses", num: true, key: (w) => w.losses, cell: (w) => count(w.losses) },
      { title: "Type", key: (w) => (w.mutual ? 1 : 0), cell: (w) => [span(w.bots ? "mutual (simulated)" : w.mutual ? "mutual" : "declared", "dim")] },
    ], wars, "There are no wars.", 3, true);

    list<Player>("players", [
      { title: "Name", key: (p) => p.name, cell: (p) => [p.name] },
      { title: "Corporation", key: (p) => p.ticker ?? "", cell: (p) => [corpTicker(p.ticker), p.alliance ? span(` ${p.alliance}`, "dim") : ""] },
      { title: "Location", key: (p) => p.system ?? "", cell: (p) => [security(p.systemSecurity ?? 0), ` ${p.system ?? ""}`, p.docked ? span(" (docked)", "dim") : ""] },
      { title: "Ship", key: (p) => p.ship ?? "", cell: (p) => [p.ship ?? ""] },
    ], players, "No pilots are logged in.");

    list<SystemRow>("systems", [
      { title: "System", key: (s) => s.name, cell: (s) => [security(s.security), ` ${s.name}`] },
      { title: "Region", key: (s) => s.region, cell: (s) => [span(s.region, "dim")] },
      { title: "In space", num: true, key: (s) => s.inSpace, cell: (s) => count(s.inSpace) },
      { title: "Docked", num: true, key: (s) => s.docked, cell: (s) => count(s.docked) },
      { title: "Total", num: true, key: (s) => s.inSpace + s.docked, cell: (s) => count(s.inSpace + s.docked) },
    ], systems, "No one is about.", 4, true);

    list<AllianceRow>("alliances", [
      { title: "Alliance", key: (a) => a.name, cell: (a) => [a.name, allianceTicker(a.ticker)] },
      { title: "Holds", key: (a) => a.region ?? "", cell: (a) => [a.region ?? span(a.kind === "players" ? "" : a.kind, "dim")] },
      { title: "Corps", num: true, key: (a) => a.corporations, cell: (a) => count(a.corporations) },
      { title: "Pilots", num: true, key: (a) => a.pilots, cell: (a) => count(a.pilots) },
      { title: "Systems", num: true, key: (a) => a.systems, cell: (a) => (a.systems > 0 ? count(a.systems) : [span("-", "dim")]) },
    ], alliances, "No alliances have been founded.", 3, true);

    list<CorporationRow>("corporations", [
      { title: "Corporation", key: (c) => c.name, cell: (c) => [c.name, corpTicker(c.ticker), c.bots ? "" : span(" (players)", "dim")] },
      { title: "Alliance", key: (c) => c.alliance ?? "", cell: (c) => [allianceTicker(c.alliance)] },
      { title: "Members", num: true, key: (c) => c.members, cell: (c) => count(c.members) },
    ], corporations, "No corporations have been founded.", 2, true);

    list<NewsItem>("news", [
      { title: "Time", key: (n) => n.time, cell: (n) => [eveTime(n.time)] },
      { title: "Report", key: (n) => n.text, cell: (n) => [n.text] },
    ], news, "Nothing has happened in null-sec yet.", 0, true);

    list<Kill>("kills", [
      { title: "Time", key: (k) => k.time ?? 0, cell: (k) => [eveTime(k.time)] },
      { title: "Victim", key: (k) => k.victim ?? "", cell: (k) => [k.victim ?? "", k.victimCorporation ? span(` (${k.victimCorporation})`, "dim") : ""] },
      { title: "Destroyed", key: (k) => k.ship ?? "", cell: (k) => [k.ship ?? "", k.pod ? span(" (pod)", "hostile") : ""] },
      { title: "System", key: (k) => k.system ?? "", cell: (k) => [k.system ?? ""] },
      { title: "Final blow", key: (k) => k.finalBlow ?? "", cell: (k) => [k.finalBlow ?? ""] },
    ], kills, "No kills have been reported.", 0, true);
  } catch (e) {
    const line = document.getElementById("statusline") as HTMLElement;
    line.textContent = `Status: unreachable, retrying (${e instanceof Error ? e.message : String(e)})`;
  }
}

// ---- the neocom clock and the minimize buttons ------------------------------------

function tick(): void {
  const now = new Date();
  (document.getElementById("clock-time") as HTMLElement).textContent = `${pad(now.getUTCHours())}:${pad(now.getUTCMinutes())}`;
  (document.getElementById("clock-date") as HTMLElement).textContent = `${now.getUTCFullYear()}.${pad(now.getUTCMonth() + 1)}.${pad(now.getUTCDate())}`;
}

for (const button of document.querySelectorAll<HTMLButtonElement>(".window .min")) {
  button.addEventListener("click", () => button.closest(".window")?.classList.toggle("minimized"));
}

tick();
setInterval(tick, 1_000);
void refresh();
setInterval(() => void refresh(), REFRESH_MS);
