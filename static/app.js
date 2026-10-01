// Web UI of the Discord bot. A plain ES module without a build step and without inline scripts
// or styles (the Content-Security-Policy forbids both). Text from the API is only ever inserted
// as text nodes, never as HTML.
//
// Screens are registered in `routes`; pages of a server are registered in `guildTabs` with the
// right they need. The chat (`#/chat/...`) is a screen of its own with a full-height layout.
// `#/privacy` shows the user's own data and erases it.

const MAX_ROLES_PER_KIND = 25;

const root = document.getElementById("app");
const account = document.getElementById("account");

/** Creates an element. Strings among the children become text nodes. */
export function h(tag, props = {}, ...children) {
  const element = document.createElement(tag);
  for (const [key, value] of Object.entries(props ?? {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") {
      element.className = value;
    } else if (key.startsWith("on") && typeof value === "function") {
      element.addEventListener(key.slice(2), value);
    } else if (typeof value === "boolean") {
      element[key] = value;
    } else {
      element.setAttribute(key, String(value));
    }
  }
  for (const child of children.flat(Infinity)) {
    if (child === undefined || child === null || child === false) continue;
    element.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return element;
}

export class ApiError extends Error {
  constructor(status, code, message) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

/** Calls the JSON API. Errors carry the server's Japanese message. */
export async function api(method, path, body) {
  const init = { method, headers: { Accept: "application/json" }, credentials: "same-origin" };
  if (body !== undefined) {
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  return request(path, init);
}

/**
 * Uploads a file as the raw request body. Its name goes percent-encoded in X-File-Name, a custom
 * header that cross-site forms cannot send.
 */
export async function uploadFile(path, file) {
  return request(path, {
    method: "POST",
    credentials: "same-origin",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/octet-stream",
      "X-File-Name": encodeURIComponent(file.name),
    },
    body: file,
  });
}

async function request(path, init) {
  let response;
  try {
    response = await fetch(path, init);
  } catch {
    throw new ApiError(0, "network", "サーバーに接続できませんでした。通信状況を確認してください。");
  }
  if (response.status === 204) return null;
  let data = null;
  try {
    data = await response.json();
  } catch {
    // Not JSON (for example a proxy error page).
  }
  if (!response.ok) {
    if (response.status === 401) session.me = null;
    throw new ApiError(
      response.status,
      data?.error ?? `http_${response.status}`,
      data?.message ?? `エラーが発生しました（HTTP ${response.status}）。時間をおいて再試行してください。`,
    );
  }
  return data;
}

/** `me` is undefined until loaded and null when logged out. */
const session = { me: undefined };

async function loadMe(refresh) {
  // After a logout or a 401 the answer is known; asking again would only log another 401.
  if (session.me === undefined || (refresh && session.me !== null)) {
    try {
      session.me = await api("GET", "/api/me");
    } catch (error) {
      if (error.status !== 401) throw error;
      session.me = null;
    }
  }
  return session.me;
}

/** Pages of a server, shown to users holding the right that `visible` checks. */
export const guildTabs = [
  { id: "roles", label: "ロール設定", visible: (rights) => rights.configure, render: rolesTab },
  {
    id: "knowledge",
    label: "ナレッジ",
    // Only while the knowledge base is enabled on the server (`knowledge` in /api/me).
    visible: (rights) => rights.manage_kb && session.me?.knowledge === true,
    render: knowledgeTab,
  },
];

const GUILD_PATH = /^\/guilds\/(\d{1,20})(?:\/([a-z-]+))?$/;
const CHAT_PATH = /^\/chat(?:\/(\d{1,20})(?:\/(\d{1,20}))?)?$/;
const PRIVACY_PATH = /^\/privacy$/;

export const routes = [
  { path: /^\/$/, refresh: true, view: homeView },
  { path: GUILD_PATH, view: guildView },
  { path: CHAT_PATH, view: chatView },
  { path: PRIVACY_PATH, view: privacyView },
];

/** Guilds where the user may use the bot (and so the chat). */
function chatGuilds() {
  return session.me?.guilds.filter((guild) => guild.access?.use) ?? [];
}

// A server page opened while logged out (for example the link of /config show) is reopened
// after the login, which always returns to "/". sessionStorage survives the trip to Discord
// within the tab; without it the user simply lands on the server list.
const RETURN_KEY = "return-to";

function returnable(path) {
  return GUILD_PATH.test(path) || CHAT_PATH.test(path) || PRIVACY_PATH.test(path);
}

function rememberReturn() {
  const path = location.hash.replace(/^#/, "");
  try {
    if (returnable(path)) {
      sessionStorage.setItem(RETURN_KEY, path);
    } else {
      sessionStorage.removeItem(RETURN_KEY);
    }
  } catch {
    // Storage disabled.
  }
}

function takeReturn() {
  try {
    const path = sessionStorage.getItem(RETURN_KEY);
    sessionStorage.removeItem(RETURN_KEY);
    return path !== null && returnable(path) ? path : null;
  } catch {
    return null;
  }
}

let rendering = 0;
/** Set by a view that must stop work when it is left (the chat's running answer). */
let leaveView = null;

async function render() {
  const current = ++rendering;
  if (leaveView) {
    leaveView();
    leaveView = null;
  }
  const path = location.hash.replace(/^#/, "") || "/";
  const route = routes.find((candidate) => candidate.path.test(path));
  let content;
  try {
    const me = await loadMe(route?.refresh);
    const back = me && path === "/" ? takeReturn() : null;
    if (back) {
      // The hashchange renders it.
      location.replace(`#${back}`);
      return;
    }
    if (!me) {
      content = loginView();
    } else if (!route) {
      content = messageView("ページが見つかりません。");
    } else {
      content = await route.view(...path.match(route.path).slice(1));
    }
  } catch (error) {
    content = session.me === null ? loginView() : errorView(error);
  }
  // A newer navigation started while this one was loading.
  if (current !== rendering) return;
  renderAccount();
  // The chat fills the window; other screens keep the page layout.
  document.body.classList.toggle("chat-mode", content.classList.contains("chat"));
  root.replaceChildren(content);
  const heading = root.querySelector("h1");
  document.title = heading ? `${heading.textContent} - AI Discussion Bot` : "AI Discussion Bot";
}

function renderAccount() {
  const me = session.me;
  if (!me) {
    account.replaceChildren();
    return;
  }
  account.replaceChildren(
    h("a", { href: "#/" }, "サーバー"),
    chatGuilds().length > 0 && h("a", { href: "#/chat" }, "チャット"),
    h("a", { href: "#/privacy" }, "あなたのデータ"),
    h("span", { class: "user" }, me.user.name),
    h("button", { type: "button", class: "link", onclick: logout }, "ログアウト"),
  );
}

async function logout() {
  try {
    await api("POST", "/auth/logout");
  } catch (error) {
    root.replaceChildren(errorView(error));
    return;
  }
  session.me = null;
  if (location.hash && location.hash !== "#/") {
    location.hash = "#/";
  } else {
    render();
  }
}

function loginView() {
  return h(
    "section",
    { class: "login" },
    h("h1", {}, "ログイン"),
    h(
      "p",
      {},
      "Discord アカウントでログインすると、Bot の利用を許可されたサーバーで AI とチャットでき、サーバー管理者は Bot を使えるロールを設定できます。",
    ),
    h("p", {}, h("a", { class: "button primary", href: "/auth/login", onclick: rememberReturn }, "Discord でログイン")),
    h(
      "p",
      { class: "muted" },
      "ログイン時に Discord から読み取るのは、ユーザー ID・名前と参加しているサーバーの一覧だけです。Discord のアクセストークンはすぐに無効化し、保存しません。詳しくは",
      h("a", { href: "/privacy" }, "プライバシーポリシー"),
      "をご覧ください。",
    ),
  );
}

function messageView(message, ...extra) {
  return h("section", {}, h("p", {}, message), extra, h("p", {}, h("a", { href: "#/" }, "サーバー一覧へ")));
}

function errorView(error) {
  const message = error instanceof ApiError ? error.message : "予期しないエラーが発生しました。";
  return h(
    "section",
    { class: "error" },
    h("p", { class: "status error" }, message),
    h("p", {}, h("button", { type: "button", onclick: () => render() }, "再読み込み")),
  );
}

// The user's own data: counts, then erasure behind a confirmation step. The server erases the
// web sessions too, so a successful erasure ends with the user logged out.
async function privacyView() {
  return privacyScreen(await api("GET", "/api/privacy"));
}

function privacyScreen(data) {
  const rows = [
    ["/talk の記録（質問と回答）", data.talk_runs],
    ["Web チャットの会話", data.web_conversations],
    ["Web のログイン（この画面を含む）", data.web_sessions],
    ["登録者としてあなたの ID と名前が記録されたナレッジ資料", data.kb_documents],
    ["設定者としてあなたの ID が記録されたロール設定", data.guild_roles],
  ];
  const status = h("p", { class: "status", role: "status" });
  const agree = h("input", { type: "checkbox" });
  const erase = h("button", { type: "button", class: "danger", disabled: true }, "削除する");
  const start = h("button", { type: "button", class: "danger" }, "データを削除する…");
  const cancel = h("button", { type: "button" }, "やめる");
  const confirmPanel = h(
    "div",
    { class: "confirm", hidden: true },
    h("p", {}, "削除すると元に戻せません。削除の後は、この画面からもログアウトされます。"),
    h("label", {}, agree, "上の内容を確認し、削除することに同意します"),
    h("div", { class: "actions" }, erase, cancel, status),
  );
  agree.addEventListener("change", () => {
    erase.disabled = !agree.checked;
  });
  start.addEventListener("click", () => {
    start.hidden = true;
    confirmPanel.hidden = false;
  });
  cancel.addEventListener("click", () => {
    agree.checked = false;
    erase.disabled = true;
    status.textContent = "";
    confirmPanel.hidden = true;
    start.hidden = false;
  });
  erase.addEventListener("click", async () => {
    erase.disabled = true;
    cancel.disabled = true;
    status.className = "status";
    status.textContent = "削除しています…";
    try {
      const result = await api("POST", "/api/privacy/delete", { confirm: "DELETE" });
      session.me = null;
      renderAccount();
      root.replaceChildren(erasedView(result));
    } catch (error) {
      if (error.status === 401) {
        render();
        return;
      }
      status.className = "status error";
      status.textContent = error.message;
      erase.disabled = !agree.checked;
      cancel.disabled = false;
    }
  });
  return h(
    "section",
    { class: "privacy" },
    h("h1", {}, "あなたのデータ"),
    h("p", { class: "muted" }, "このBotが保存している、あなたに関するデータの件数です（すべてのサーバーの合計）。"),
    h(
      "table",
      { class: "counts" },
      h(
        "tbody",
        {},
        rows.map(([label, count]) => h("tr", {}, h("th", { scope: "row" }, label), h("td", {}, `${formatNumber(count)} 件`))),
      ),
    ),
    h("h2", {}, "データの削除"),
    h(
      "ul",
      {},
      h("li", {}, "/talk の記録、Web チャットの会話（メッセージを含む）、Web のログインを削除します。"),
      h("li", {}, "ナレッジ資料とロール設定はサーバーのものなので残し、記録されたあなたの ID と名前だけを消します。"),
      h("li", {}, "回答を作成中の /talk の記録は残ります。回答が終わってから、もう一度削除してください。"),
      h("li", {}, "Discord のチャンネルに投稿された回答のメッセージは削除されません。"),
      h("li", {}, "バックアップには最長 35 日残りますが、バックアップから復元した場合も削除し直します。"),
    ),
    h("p", { class: "muted" }, "Discord の /privacy delete でも同じ削除ができます。詳しくは", h("a", { href: "/privacy" }, "プライバシーポリシー"), "をご覧ください。"),
    h("div", { class: "actions" }, start),
    confirmPanel,
  );
}

function erasedView(result) {
  return h(
    "section",
    { class: "privacy" },
    h("h1", {}, "削除しました"),
    h(
      "p",
      {},
      `/talk の記録 ${formatNumber(result.talk_runs)} 件、Web チャットの会話 ${formatNumber(result.web_conversations)} 件、Web のログイン ${formatNumber(result.web_sessions)} 件を削除し、ナレッジ資料 ${formatNumber(result.kb_documents)} 件とロール設定 ${formatNumber(result.guild_roles)} 件からあなたの ID と名前を消しました。`,
    ),
    result.talk_runs_in_progress > 0 &&
      h(
        "p",
        { class: "status warn" },
        `回答を作成中の /talk の記録 ${formatNumber(result.talk_runs_in_progress)} 件は残っています。回答が終わってから、もう一度削除してください。`,
      ),
    h("p", {}, "ログアウトしました。"),
    h("p", {}, h("a", { class: "button", href: "#/" }, "トップへ")),
  );
}

function badge(text, kind) {
  return h("span", { class: `badge ${kind}` }, text);
}

function homeView() {
  const { guilds } = session.me;
  const list =
    guilds.length === 0
      ? h(
          "p",
          { class: "empty" },
          "このアカウントで開けるサーバーはありません。Bot が導入され、運営者が利用を許可したサーバーだけが表示されます。",
        )
      : h("ul", { class: "cards" }, guilds.map(guildCard));
  return h(
    "section",
    {},
    h("h1", {}, "サーバー"),
    list,
    h(
      "p",
      { class: "muted" },
      "サーバーの一覧はログインした時点のものです。新しく参加したサーバーを表示するには、ログアウトしてからログインし直してください。",
    ),
  );
}

function guildCard(guild) {
  const rights = guild.access;
  const tabs = rights ? guildTabs.filter((tab) => tab.visible(rights)) : [];
  const badges = rights
    ? [
        rights.use ? badge("Bot を利用できます", "ok") : badge("Bot を利用できません", "off"),
        rights.manage_kb && badge("ナレッジ管理", "ok"),
        rights.configure && badge("設定を変更できます", "ok"),
      ]
    : [badge("権限を確認できませんでした。時間をおいて再読み込みしてください", "warn")];
  return h(
    "li",
    { class: "card" },
    h("div", { class: "card-title" }, guild.name),
    h("div", { class: "badges" }, badges),
    (rights?.use || tabs.length > 0) &&
      h(
        "div",
        { class: "card-actions" },
        rights?.use && h("a", { class: "button primary", href: `#/chat/${guild.id}` }, "チャット"),
        tabs.length > 0 && h("a", { class: "button", href: `#/guilds/${guild.id}` }, rights.use ? "管理" : "開く"),
      ),
  );
}

async function guildView(id, tabId) {
  const guild = session.me.guilds.find((candidate) => candidate.id === id);
  if (!guild) return messageView("このサーバーは表示できません。");
  if (!guild.access) return messageView("権限を確認できませんでした。時間をおいて再読み込みしてください。");
  const tabs = guildTabs.filter((tab) => tab.visible(guild.access));
  if (tabs.length === 0) return messageView("このサーバーで開ける画面はありません。");
  const tab = tabs.find((candidate) => candidate.id === tabId) ?? tabs[0];
  return h(
    "section",
    {},
    h("p", { class: "crumb" }, h("a", { href: "#/" }, "← サーバー一覧")),
    h("h1", {}, guild.name),
    h(
      "nav",
      { class: "tabs" },
      tabs.map((candidate) =>
        h(
          "a",
          {
            href: `#/guilds/${guild.id}/${candidate.id}`,
            class: candidate === tab ? "tab active" : "tab",
            "aria-current": candidate === tab ? "page" : null,
          },
          candidate.label,
        ),
      ),
    ),
    await tab.render(guild),
  );
}

async function rolesTab(guild) {
  return roleEditor(await api("GET", `/api/guilds/${guild.id}/roles`));
}

const ROLE_KINDS = [
  {
    key: "use",
    title: "利用ロール",
    help: "選んだロールを持つ人が /talk を使えます。1つも選ばないと、このサーバーでは誰も使えません（サーバー管理者も含みます）。全員に許可するときは @everyone を選びます。",
  },
  {
    key: "manage",
    title: "ナレッジ管理ロール",
    help: "選んだロールを持つ人が、このサーバーのナレッジ（/talk が参照する資料）を Web 画面で登録・削除できます。サーバー管理権限を持つ人は、選ばなくても管理できます。",
  },
];

function roleEditor(data, notice) {
  // @everyone (the guild's own ID) first: it is the usual choice for "everyone may use it".
  const everyone = data.roles.filter((role) => role.id === data.guild.id);
  data = { ...data, roles: [...everyone, ...data.roles.filter((role) => role.id !== data.guild.id)] };
  const known = new Set(data.roles.map((role) => role.id));
  const status = h("p", { class: notice ? "status ok" : "status", role: "status" }, notice);
  const save = h("button", { type: "submit", class: "primary" }, "保存");
  const groups = ROLE_KINDS.map((kind) => roleGroup(kind, data, known));
  const refresh = () => {
    for (const group of groups) group.update();
    save.disabled = groups.some((group) => group.selected().length > MAX_ROLES_PER_KIND);
  };
  const filter = h("input", {
    type: "search",
    class: "filter",
    placeholder: "ロール名で絞り込み",
    "aria-label": "ロール名で絞り込み",
    oninput: () => {
      const query = filter.value.trim().toLowerCase();
      for (const item of root.querySelectorAll(".role-list li")) {
        item.hidden = query !== "" && !item.dataset.name.includes(query);
      }
    },
  });
  const form = h(
    "form",
    {
      class: "roles",
      onchange: refresh,
      onsubmit: async (event) => {
        event.preventDefault();
        save.disabled = true;
        status.className = "status";
        status.textContent = "保存しています…";
        try {
          const saved = await api("PUT", `/api/guilds/${data.guild.id}/config/roles`, {
            use: groups[0].selected(),
            manage: groups[1].selected(),
          });
          form.replaceWith(roleEditor(saved, "保存しました。"));
        } catch (error) {
          if (error.status === 401) {
            render();
            return;
          }
          status.className = "status error";
          status.textContent = error.message;
          refresh();
        }
      },
    },
    h(
      "p",
      { class: "muted" },
      "ロールの設定を変更できるのは、サーバーのオーナーと「管理者」「サーバー管理」権限を持つ人だけです。Discord の /config コマンドでも同じ設定を変更できます。",
    ),
    data.roles.length > 12 && filter,
    groups.map((group) => group.element),
    h("div", { class: "actions" }, save, status),
  );
  refresh();
  return form;
}

function roleGroup(kind, data, known) {
  const configured = data[kind.key];
  const missing = configured.filter((id) => !known.has(id)).length;
  const counter = h("span", { class: "counter" });
  const warning = h("p", { class: "status warn" });
  const boxes = data.roles.map((role) =>
    h("input", { type: "checkbox", name: kind.key, value: role.id, checked: configured.includes(role.id) }),
  );
  const items = data.roles.map((role, index) => {
    const dot = h("span", { class: "dot", "aria-hidden": "true" });
    if (role.color) dot.style.backgroundColor = `#${role.color.toString(16).padStart(6, "0")}`;
    const name = role.id === data.guild.id ? "@everyone（全員）" : role.name;
    const item = h(
      "li",
      {},
      h(
        "label",
        {},
        boxes[index],
        dot,
        h("span", { class: "role-name" }, name),
        role.managed && h("span", { class: "tag" }, "連携"),
      ),
    );
    item.dataset.name = name.toLowerCase();
    return item;
  });
  const selected = () => boxes.filter((box) => box.checked).map((box) => box.value);
  const update = () => {
    const count = selected().length;
    counter.textContent = `${count} / ${MAX_ROLES_PER_KIND}`;
    counter.className = count > MAX_ROLES_PER_KIND ? "counter over" : "counter";
    const messages = [];
    if (count > MAX_ROLES_PER_KIND) messages.push(`選べるのは${MAX_ROLES_PER_KIND}個までです。`);
    if (kind.key === "use" && count === 0) {
      messages.push("利用ロールが選ばれていません。このままでは、このサーバーで誰も Bot を使えません。");
    }
    if (missing > 0) {
      messages.push(`Discord で削除されたロールが${missing}個設定されています。保存すると設定から外れます。`);
    }
    warning.textContent = messages.join(" ");
    warning.hidden = messages.length === 0;
  };
  const element = h(
    "fieldset",
    { class: "role-group" },
    h("legend", {}, kind.title, " ", counter),
    h("p", { class: "muted" }, kind.help),
    warning,
    h("ul", { class: "role-list" }, items),
  );
  return { element, selected, update };
}

// ---- Knowledge base (M3) ----

const STATUS_LABELS = {
  processing: ["処理中", "warn"],
  ready: ["利用できます", "ok"],
  failed: ["失敗", "error"],
};
const KIND_LABELS = { text: "テキスト", markdown: "Markdown", pdf: "PDF" };
const POLL_MS = 5000;

// 1024-based, labelled as the server's messages are (KiB, MiB).
function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function formatNumber(value) {
  return Number(value).toLocaleString("ja-JP");
}

function formatTime(iso) {
  const date = new Date(iso);
  const sameDay = date.toDateString() === new Date().toDateString();
  return sameDay
    ? date.toLocaleTimeString("ja-JP", { hour: "2-digit", minute: "2-digit" })
    : date.toLocaleString("ja-JP", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function providerState(provider) {
  const until = provider.until ? formatTime(provider.until) : "";
  switch (provider.state) {
    case "rate_limited":
      return [`レート制限のため ${until} ごろまで待機しています（自動で再開します）`, "warn"];
    case "auth_error":
      return [`API キーが受け付けられませんでした。${until} ごろに再試行します。管理者に設定の確認を依頼してください`, "error"];
    case "daily_limit":
      return [`1日の送信量（設定値）に達したため ${until} ごろまで待機しています`, "warn"];
    default:
      return ["利用できます", "ok"];
  }
}

async function knowledgeTab(guild) {
  return knowledgeView(guild, await api("GET", `/api/guilds/${guild.id}/kb/documents`));
}

function knowledgeView(guild, initial) {
  const base = `/api/guilds/${guild.id}/kb/documents`;
  let data = initial;
  let timer = null;
  // Each navigation renders anew; a newer render means this view was left.
  const generation = rendering;
  const status = h("p", { class: "status", role: "status" });
  const usage = h("p", { class: "muted" });
  const providers = h("ul", { class: "providers" });
  const tbody = h("tbody");
  const empty = h("p", { class: "empty" }, "まだ資料が登録されていません。");
  const tableWrap = h(
    "div",
    { class: "table-wrap" },
    h(
      "table",
      { class: "documents" },
      h(
        "thead",
        {},
        h("tr", {}, h("th", {}, "資料"), h("th", {}, "状態"), h("th", {}, "進み具合"), h("th", {}, "登録"), h("th", {}, "操作")),
      ),
      tbody,
    ),
  );
  const preview = h("section", { class: "preview", hidden: true, "aria-live": "polite" });
  const input = h("input", { type: "file", "aria-label": "登録するファイル" });
  const submit = h("button", { type: "submit", class: "primary" }, "アップロード");
  const limitsNote = h("p", { class: "muted" });

  const show = (message, kind = "") => {
    status.className = kind ? `status ${kind}` : "status";
    status.textContent = message ?? "";
  };

  const fail = (error) => {
    if (error.status === 401) {
      render();
      return;
    }
    show(error instanceof ApiError ? error.message : "予期しないエラーが発生しました。", "error");
  };

  const reload = async () => {
    try {
      update(await api("GET", base));
    } catch (error) {
      fail(error);
    }
  };

  // Polls every 5 seconds while a document is processing, until the tab is left.
  const schedule = () => {
    clearTimeout(timer);
    if (!data.documents.some((document) => document.status === "processing")) return;
    timer = setTimeout(async () => {
      if (generation !== rendering || !element.isConnected) return;
      await reload();
    }, POLL_MS);
  };

  const form = h(
    "form",
    {
      class: "upload",
      onsubmit: async (event) => {
        event.preventDefault();
        const file = input.files?.[0];
        if (!file) {
          show("ファイルを選んでください。", "error");
          return;
        }
        const name = file.name.toLowerCase();
        if (!data.limits.extensions.some((extension) => name.endsWith(extension))) {
          show(`登録できるのは ${data.limits.extensions.join("、")} のファイルだけです。`, "error");
          return;
        }
        if (file.size === 0) {
          show("ファイルが空です。", "error");
          return;
        }
        if (file.size > data.limits.max_upload_bytes) {
          show(`ファイルは ${formatBytes(data.limits.max_upload_bytes)} までです（このファイルは ${formatBytes(file.size)}）。`, "error");
          return;
        }
        if (name.endsWith(".pdf") && file.size > data.limits.max_pdf_bytes) {
          show(`PDF は ${formatBytes(data.limits.max_pdf_bytes)} までです（このファイルは ${formatBytes(file.size)}）。ファイルを分割してください。`, "error");
          return;
        }
        submit.disabled = true;
        show(name.endsWith(".pdf") ? "アップロードして本文を取り出しています（PDF は時間がかかります）…" : "アップロードしています…");
        try {
          const document = await uploadFile(base, file);
          form.reset();
          show(`「${document.title}」を登録しました。バックグラウンドで処理しています。`, "ok");
          await reload();
        } catch (error) {
          fail(error);
        } finally {
          submit.disabled = false;
        }
      },
    },
    h("label", { class: "file" }, input),
    submit,
  );

  const action = (label, handler, kind) =>
    h(
      "button",
      {
        type: "button",
        class: kind ? `small ${kind}` : "small",
        onclick: async (event) => {
          const button = event.currentTarget;
          button.disabled = true;
          try {
            await handler();
          } catch (error) {
            fail(error);
          } finally {
            button.disabled = false;
          }
        },
      },
      label,
    );

  const showPreview = async (document) => {
    const result = await api("GET", `${base}/${document.id}/preview`);
    preview.replaceChildren(
      h("h2", {}, `プレビュー: ${result.title}`),
      h(
        "p",
        { class: "muted" },
        result.truncated
          ? `取り出した本文の先頭 ${formatNumber([...result.text].length)} 文字です（全体は ${formatNumber(result.char_count)} 文字）。文字化けしていないか確認してください。`
          : "取り出した本文の全体です。文字化けしていないか確認してください。",
      ),
      h("pre", {}, result.text),
      h("button", { type: "button", onclick: () => (preview.hidden = true) }, "閉じる"),
    );
    preview.hidden = false;
    preview.scrollIntoView({ block: "nearest" });
  };

  const row = (document) => {
    const [label, kind] = STATUS_LABELS[document.status] ?? [document.status, "off"];
    const progress = document.progress.map((item) =>
      h(
        "div",
        { class: "progress" },
        h("span", {}, `${item.provider}: ${formatNumber(item.embedded)} / ${formatNumber(document.chunk_count)}`),
        h("progress", { max: Math.max(document.chunk_count, 1), value: item.embedded }),
      ),
    );
    const actions = [action("プレビュー", () => showPreview(document))];
    if (document.status === "failed") {
      actions.push(
        action("再試行", async () => {
          await api("POST", `${base}/${document.id}/retry`);
          show(`「${document.title}」を再試行します。`, "ok");
          await reload();
        }),
      );
    }
    actions.push(
      action(
        "削除",
        async () => {
          if (!confirm(`「${document.title}」を削除しますか？ 取り出した本文と検索用のデータもすべて削除され、元に戻せません。`)) return;
          await api("DELETE", `${base}/${document.id}`);
          preview.hidden = true;
          show(`「${document.title}」を削除しました。`, "ok");
          await reload();
        },
        "danger",
      ),
    );
    return h(
      "tr",
      {},
      h(
        "td",
        {},
        h("div", { class: "doc-title" }, document.title),
        h(
          "div",
          { class: "muted" },
          `${document.file_name} · ${KIND_LABELS[document.kind] ?? document.kind} · ${formatBytes(document.byte_size)} · ${formatNumber(document.char_count)}文字 · ${formatNumber(document.chunk_count)}チャンク`,
        ),
      ),
      h("td", {}, badge(label, kind), document.error && h("div", { class: `note ${document.status === "failed" ? "error" : "warn"}` }, document.error)),
      h("td", {}, progress),
      h("td", {}, h("div", {}, document.uploaded_by ?? "（不明）"), h("div", { class: "muted" }, formatTime(document.created_at))),
      h("td", { class: "row-actions" }, actions),
    );
  };

  const update = (next) => {
    data = next;
    const u = data.usage;
    usage.textContent = `資料 ${formatNumber(u.documents)} / ${formatNumber(u.max_documents)} 件 · チャンク ${formatNumber(u.chunks)} / ${formatNumber(u.max_chunks)}（Bot 全体 ${formatNumber(u.total_chunks)} / ${formatNumber(u.max_total_chunks)}）`;
    const limits = data.limits;
    const pdf = limits.extensions.includes(".pdf")
      ? `PDF は ${formatBytes(limits.max_pdf_bytes)}・${formatNumber(limits.max_pdf_pages)} ページまで、画像だけの PDF は不可。`
      : "";
    limitsNote.textContent = `登録できるファイル: ${limits.extensions.join("、")}（${formatBytes(limits.max_upload_bytes)}・本文 ${formatNumber(limits.max_text_chars)} 文字まで。${pdf}）`;
    input.setAttribute("accept", data.limits.extensions.join(","));
    providers.replaceChildren(
      ...data.providers.map((provider, index) => {
        const [text, kind] = providerState(provider);
        return h(
          "li",
          {},
          h("span", { class: "provider-name" }, `${index + 1}. ${provider.label}`),
          " ",
          badge(text, kind),
        );
      }),
    );
    tbody.replaceChildren(...data.documents.map(row));
    empty.hidden = data.documents.length > 0;
    tableWrap.hidden = data.documents.length === 0;
    schedule();
  };

  const element = h(
    "div",
    { class: "knowledge" },
    h(
      "p",
      { class: "status warn" },
      "登録した資料の内容は、このサーバーで /talk を使う人への回答に引用されることがあり、回答はチャンネルに公開されます。個人情報や外部に出せない情報を含む資料は登録しないでください。",
    ),
    h(
      "p",
      { class: "muted" },
      "資料は /talk の回答の参考にされ、使われた資料の名前が回答の末尾に表示されます。/talk の knowledge オプションを false にすると参照しません。",
    ),
    h("h2", {}, "資料を登録する"),
    form,
    limitsNote,
    status,
    h("h2", {}, "登録済みの資料"),
    usage,
    h("h3", {}, "埋め込みプロバイダー（上から順に使用）"),
    providers,
    h(
      "p",
      { class: "muted" },
      "登録した資料は、プロバイダーの送信量の上限に合わせて少しずつ処理されます。無料枠のプロバイダーでは、大きな資料に数時間から数日かかることがあります。いずれかのプロバイダーで全チャンクの処理が終わると「利用できます」になり、/talk で参照されます。",
    ),
    empty,
    tableWrap,
    preview,
  );
  update(initial);
  return element;
}

// ---- Chat (M4) ----

const MAX_QUESTION_CHARS = 4000;
/** Streaming answers are redrawn at most this often. */
const PAINT_MS = 100;
/** How close to the bottom (px) counts as following the answer. */
const FOLLOW_SLACK = 48;
const CHAT_GUILD_KEY = "chat-guild";

/**
 * Answers are Markdown from the AI, which reads untrusted material (web pages, documents), so
 * they are sanitized: no raw HTML from marked, no images, forms, styles, frames, SVG or MathML,
 * and only http(s) links, which open in a new tab without a referrer.
 */
const PURIFY = {
  USE_PROFILES: { html: true },
  FORBID_TAGS: [
    "img", "style", "iframe", "form", "input", "svg", "math", "button", "textarea", "select",
    "option", "video", "audio", "source", "picture", "object", "embed", "link", "meta", "base",
  ],
  FORBID_ATTR: ["style"],
  ALLOWED_URI_REGEXP: /^https?:\/\//i,
  ALLOW_DATA_ATTR: false,
  RETURN_DOM_FRAGMENT: true,
};

let markdown;

function escapeHtml(text) {
  return text.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

/** Markdown to a sanitized fragment; `null` if the libraries did not load (text is shown then). */
function markdownRenderer() {
  if (markdown !== undefined) return markdown;
  const { marked, DOMPurify } = globalThis;
  if (!marked?.Marked || !DOMPurify?.sanitize) {
    markdown = null;
    return markdown;
  }
  const parser = new marked.Marked({ gfm: true, breaks: true, async: false });
  // HTML written in the Markdown is shown as text.
  parser.use({ renderer: { html: (token) => escapeHtml(token.text) } });
  DOMPurify.addHook("afterSanitizeAttributes", (node) => {
    if (node.tagName === "A") {
      node.setAttribute("target", "_blank");
      node.setAttribute("rel", "noopener noreferrer nofollow");
    }
  });
  markdown = (text) => DOMPurify.sanitize(parser.parse(text), PURIFY);
  return markdown;
}

function renderMarkdown(text) {
  const render = markdownRenderer();
  return render ? render(text) : h("p", { class: "plain" }, text);
}

function loadPreference(key) {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function savePreference(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Storage disabled.
  }
}

function hostOf(url) {
  try {
    return new URL(url).host;
  } catch {
    return "";
  }
}

function webLink(source) {
  const label = source.title || source.url;
  if (!/^https?:\/\//i.test(source.url)) return label;
  return h(
    "span",
    {},
    h("a", { href: source.url, target: "_blank", rel: "noopener noreferrer nofollow" }, label),
    " ",
    h("span", { class: "muted" }, hostOf(source.url)),
  );
}

const TOOL_LABELS = { web_search: "Web検索", web_fetch: "ページ取得" };

/** Server-sent events from a fetch() body (EventSource cannot POST). */
async function readEvents(body, onEvent) {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  const flush = () => {
    let end;
    while ((end = buffer.indexOf("\n\n")) >= 0) {
      const block = buffer.slice(0, end);
      buffer = buffer.slice(end + 2);
      let name = "message";
      const data = [];
      for (const line of block.split("\n")) {
        if (line === "" || line.startsWith(":")) continue;
        const colon = line.indexOf(":");
        const field = colon < 0 ? line : line.slice(0, colon);
        let value = colon < 0 ? "" : line.slice(colon + 1);
        if (value.startsWith(" ")) value = value.slice(1);
        if (field === "event") name = value;
        else if (field === "data") data.push(value);
      }
      if (data.length === 0) continue;
      let parsed;
      try {
        parsed = JSON.parse(data.join("\n"));
      } catch {
        continue;
      }
      onEvent(name, parsed);
    }
  };
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    flush();
  }
  buffer += decoder.decode();
  flush();
}

async function chatView(guildId, conversationId) {
  const guilds = chatGuilds();
  if (guilds.length === 0) {
    return messageView(
      "チャットを使えるサーバーがありません。サーバーの管理者が許可したロールを持っている場合は、ログインし直すと表示されることがあります。",
    );
  }
  if (!guildId) {
    const remembered = loadPreference(CHAT_GUILD_KEY);
    const target = guilds.find((guild) => guild.id === remembered) ?? guilds[0];
    location.replace(`#/chat/${target.id}`);
    return h("p", { class: "muted" }, "読み込み中…");
  }
  const guild = guilds.find((candidate) => candidate.id === guildId);
  if (!guild) return messageView("このサーバーではチャットを使えません。");
  savePreference(CHAT_GUILD_KEY, guild.id);
  const [listing, detail] = await Promise.all([
    api("GET", `/api/conversations?guild=${guild.id}`),
    conversationId
      ? api("GET", `/api/conversations/${conversationId}`).catch((error) => {
          if (error.status === 404) return null;
          throw error;
        })
      : null,
  ]);
  if (conversationId && !detail) {
    return messageView("この会話は見つかりません。削除されたか、保存期間を過ぎた可能性があります。", h("p", {}, h("a", { href: `#/chat/${guild.id}` }, "新しい会話を始める")));
  }
  if (detail && detail.guild_id !== guild.id) {
    location.replace(`#/chat/${detail.guild_id}/${detail.id}`);
    return h("p", { class: "muted" }, "読み込み中…");
  }
  return chatScreen(guild, guilds, listing, detail);
}

function chatScreen(guild, guilds, initialListing, detail) {
  let listing = initialListing;
  let conversation = detail;
  /** The answer this tab is generating: { controller, view }. */
  let active = null;
  let follow = true;

  const status = h("p", { class: "status", role: "status" });
  const showStatus = (message, kind = "") => {
    status.className = kind ? `status ${kind}` : "status";
    status.textContent = message ?? "";
  };

  // Sidebar.
  const select = h(
    "select",
    {
      "aria-label": "サーバー",
      onchange: () => {
        location.hash = `#/chat/${select.value}`;
      },
    },
    guilds.map((candidate) => h("option", { value: candidate.id, selected: candidate.id === guild.id }, candidate.name)),
  );
  const list = h("ul", { class: "conversations" });
  const usage = h("p", { class: "muted usage" });
  const sidebar = h(
    "aside",
    {
      class: "chat-sidebar",
      "aria-label": "会話",
      // On narrow screens the list is a panel; following one of its links closes it.
      onclick: (event) => {
        if (event.target.closest("a")) screen.classList.remove("sidebar-open");
      },
    },
    h("label", { class: "field" }, h("span", { class: "muted" }, "サーバー"), select),
    h("a", { class: "button primary new-chat", href: `#/chat/${guild.id}` }, "新しい会話"),
    list,
    usage,
    h(
      "p",
      { class: "muted small-print" },
      "会話は最終更新から一定期間（既定 30 日）保存され、その後自動で削除されます。一覧からいつでも削除できます。",
      h("a", { href: "/privacy" }, "プライバシーポリシー"),
      " · ",
      h("a", { href: "/terms" }, "利用規約"),
    ),
  );

  // Messages.
  const messages = h("div", {
    class: "chat-messages",
    role: "log",
    "aria-live": "polite",
    onscroll: () => {
      follow = messages.scrollHeight - messages.scrollTop - messages.clientHeight < FOLLOW_SLACK;
    },
  });
  const keepUp = () => {
    if (follow) messages.scrollTop = messages.scrollHeight;
  };
  const empty = h(
    "div",
    { class: "chat-empty" },
    h("p", {}, `「${guild.name}」の Bot に質問できます。`),
    h(
      "p",
      { class: "muted" },
      "「Web検索」を選ぶと、AI が必要に応じて Web を検索して出典付きで回答します。「ナレッジ」はこのサーバーに登録された資料を参照します。Discord のチャンネルの投稿は参照しません。回答は誤りを含むことがあります。",
    ),
  );

  const titleText = () => conversation?.title ?? "新しい会話";
  const heading = h("h1", {}, titleText());
  const toggle = h(
    "button",
    {
      type: "button",
      class: "sidebar-toggle",
      "aria-expanded": "false",
      onclick: () => {
        const open = !screen.classList.contains("sidebar-open");
        screen.classList.toggle("sidebar-open", open);
        toggle.setAttribute("aria-expanded", String(open));
      },
    },
    "会話一覧",
  );

  // Composer.
  const textarea = h("textarea", {
    rows: 3,
    placeholder: "メッセージを入力（Enter で送信、Shift+Enter で改行）",
    "aria-label": "メッセージ",
    oninput: () => updateCounter(),
    onkeydown: (event) => {
      // Not while an input method is composing (Enter then confirms the conversion).
      if (event.key === "Enter" && !event.shiftKey && !event.isComposing && event.keyCode !== 229) {
        event.preventDefault();
        if (!active) form.requestSubmit();
      }
    },
  });
  const counter = h("span", { class: "counter" });
  const webBox = h("input", { type: "checkbox" });
  const knowledgeBox = h("input", { type: "checkbox" });
  const knowledgeLabel = h("label", { class: "option" }, knowledgeBox, "ナレッジ");
  const sendButton = h("button", { type: "submit", class: "primary send" }, "送信");
  const updateCounter = () => {
    const count = [...textarea.value].length;
    counter.textContent = `${count.toLocaleString("ja-JP")} / ${MAX_QUESTION_CHARS.toLocaleString("ja-JP")}`;
    counter.className = count > MAX_QUESTION_CHARS ? "counter over" : "counter";
  };
  const setGenerating = (generating) => {
    sendButton.textContent = generating ? "停止" : "送信";
    sendButton.className = generating ? "danger send" : "primary send";
    sendButton.disabled = false;
  };
  const form = h(
    "form",
    {
      class: "composer",
      onsubmit: (event) => {
        event.preventDefault();
        if (active) {
          stop();
        } else {
          send();
        }
      },
    },
    textarea,
    h(
      "div",
      { class: "composer-bar" },
      h("label", { class: "option" }, webBox, "Web検索"),
      knowledgeLabel,
      counter,
      sendButton,
    ),
    status,
  );

  const screen = h(
    "div",
    { class: "chat" },
    sidebar,
    h(
      "section",
      { class: "chat-main" },
      h("header", { class: "chat-header" }, toggle, heading, h("span", { class: "muted guild-name" }, guild.name)),
      messages,
      form,
    ),
  );

  const renderList = () => {
    list.replaceChildren(
      ...listing.conversations.map((item) => {
        const current = conversation?.id === item.id;
        return h(
          "li",
          { class: current ? "active" : "" },
          h(
            "a",
            { href: `#/chat/${guild.id}/${item.id}`, "aria-current": current ? "page" : null, class: item.title ? "" : "untitled" },
            item.title ?? "新しい会話",
          ),
          h(
            "span",
            { class: "item-actions" },
            h("button", { type: "button", class: "small", title: "名前を変更", onclick: () => rename(item) }, "名前"),
            h("button", { type: "button", class: "small danger", title: "削除", onclick: () => remove(item) }, "削除"),
          ),
        );
      }),
    );
    if (listing.conversations.length === 0) {
      list.append(h("li", { class: "muted none" }, "まだ会話はありません。"));
    }
    const { daily_used: used, daily_limit: limit } = listing;
    usage.textContent = `直近24時間の送信: ${used} / ${limit} 件`;
    usage.className = used >= limit ? "usage status warn" : "muted usage";
    knowledgeLabel.hidden = !listing.knowledge;
  };

  const refreshList = async () => {
    try {
      listing = await api("GET", `/api/conversations?guild=${guild.id}`);
      if (conversation) {
        const updated = listing.conversations.find((item) => item.id === conversation.id);
        if (updated) {
          conversation = { ...conversation, ...updated };
          heading.textContent = titleText();
          document.title = `${titleText()} - AI Discussion Bot`;
        }
      }
      renderList();
    } catch (error) {
      if (error.status === 401) render();
    }
  };

  const rename = async (item) => {
    const title = prompt("会話の名前（100文字まで）", item.title ?? "");
    if (title === null) return;
    try {
      await api("PATCH", `/api/conversations/${item.id}`, { title });
      showStatus("名前を変更しました。", "ok");
      await refreshList();
    } catch (error) {
      if (error.status === 401) return render();
      showStatus(error.message, "error");
    }
  };

  const remove = async (item) => {
    const name = item.title ?? "新しい会話";
    if (!confirm(`「${name}」を削除しますか？ この会話のメッセージはすべて削除され、元に戻せません。`)) return;
    try {
      await api("DELETE", `/api/conversations/${item.id}`);
    } catch (error) {
      if (error.status === 401) return render();
      showStatus(error.message, "error");
      return;
    }
    if (conversation?.id === item.id) {
      location.hash = `#/chat/${guild.id}`;
    } else {
      showStatus(`「${name}」を削除しました。`, "ok");
      await refreshList();
    }
  };

  const addUser = (message) => {
    empty.remove();
    const options = [message.web_search && "Web検索", message.knowledge && "ナレッジ"].filter(Boolean);
    const element = h(
      "div",
      { class: "msg user" },
      h("div", { class: "bubble" }, message.content),
      options.length > 0 && h("div", { class: "msg-meta muted" }, options.join(" · ")),
    );
    messages.append(element);
    return element;
  };

  const addAnswer = (message) => {
    empty.remove();
    const view = answerView(message, keepUp);
    messages.append(view.element);
    return view;
  };

  const send = async () => {
    const content = textarea.value;
    if (!content.trim()) {
      showStatus("メッセージを入力してください。", "error");
      return;
    }
    if ([...content].length > MAX_QUESTION_CHARS) {
      showStatus(`メッセージは ${MAX_QUESTION_CHARS.toLocaleString("ja-JP")} 文字までです。`, "error");
      return;
    }
    showStatus("");
    const controller = new AbortController();
    const run = { controller, view: null, stopping: false };
    active = run;
    setGenerating(true);
    try {
      if (!conversation) {
        conversation = await api("POST", `/api/conversations?guild=${guild.id}`);
        history.replaceState(null, "", `#/chat/${guild.id}/${conversation.id}`);
      }
      const options = { web_search: webBox.checked, knowledge: !knowledgeLabel.hidden && knowledgeBox.checked };
      const question = addUser({ content, ...options });
      run.view = addAnswer({ content: "", status: "streaming", sources: [], kb_sources: [], tool_count: 0 });
      textarea.value = "";
      updateCounter();
      follow = true;
      keepUp();
      const response = await fetch(`/api/conversations/${conversation.id}/messages`, {
        method: "POST",
        credentials: "same-origin",
        headers: { Accept: "text/event-stream", "Content-Type": "application/json" },
        body: JSON.stringify({ content, ...options }),
        signal: controller.signal,
      });
      if (!response.ok) {
        let data = null;
        try {
          data = await response.json();
        } catch {
          // Not JSON (for example a proxy error page).
        }
        if (response.status === 401) {
          session.me = null;
          render();
          return;
        }
        // Nothing was stored: take the question back into the box.
        question.remove();
        run.view.element.remove();
        textarea.value = content;
        updateCounter();
        showStatus(data?.message ?? `エラーが発生しました（HTTP ${response.status}）。時間をおいて再試行してください。`, "error");
        return;
      }
      let ended = false;
      await readEvents(response.body, (name, data) => {
        const view = run.view;
        switch (name) {
          case "delta":
            view.append(data.text);
            break;
          case "reset":
            view.reset();
            break;
          case "tool":
            view.tool(data.name, data.detail);
            break;
          case "kb_sources":
            view.knowledge(data.kb_sources);
            break;
          case "sources":
            view.sources(data.sources);
            break;
          case "done":
            ended = true;
            view.finish(data.status);
            break;
          case "error":
            ended = true;
            view.fail(data.message);
            break;
        }
      });
      if (!ended) {
        run.view.fail(run.stopping ? null : "接続が切れました。保存された内容は、ページを再読み込みすると確認できます。");
      }
    } catch (error) {
      if (error?.name === "AbortError") {
        run.view?.finish("stopped");
      } else if (error instanceof ApiError) {
        if (error.status === 401) return render();
        run.view?.element.remove();
        textarea.value = content;
        updateCounter();
        showStatus(error.message, "error");
      } else {
        run.view?.fail("サーバーとの接続が切れました。通信状況を確認してください。");
      }
    } finally {
      if (active === run) active = null;
      setGenerating(false);
      if (screen.isConnected) await refreshList();
    }
  };

  const stop = async () => {
    const run = active;
    if (!run || run.stopping) return;
    run.stopping = true;
    sendButton.disabled = true;
    try {
      // The server saves the text so far and ends the stream with `done`.
      await api("POST", "/api/chat/stop");
    } catch {
      // Closing the stream stops the answer too.
    }
    setTimeout(() => {
      if (active === run) run.controller.abort();
    }, 3000);
  };

  leaveView = () => active?.controller.abort();

  // Earlier messages.
  if (conversation) {
    for (const message of conversation.messages) {
      if (message.role === "user") addUser(message);
      else addAnswer(message);
    }
  }
  if (!conversation || conversation.messages.length === 0) messages.append(empty);
  webBox.checked = false;
  knowledgeBox.checked = true;
  renderList();
  updateCounter();
  setGenerating(false);
  requestAnimationFrame(() => {
    messages.scrollTop = messages.scrollHeight;
    if (!conversation) textarea.focus();
  });
  return screen;
}

/** One answer: its Markdown, what it consulted, and how it ended. */
function answerView(message, keepUp) {
  let text = message.content ?? "";
  let painted = 0;
  let timer = null;
  let toolCount = message.tool_count ?? 0;
  let webSources = message.sources ?? [];
  let documents = message.kb_sources ?? [];
  const body = h("div", { class: "markdown" });
  const activity = h("p", { class: "activity muted" });
  const note = h("p", { class: "status" });
  const consulted = h("div", { class: "consulted" });
  const copy = h(
    "button",
    {
      type: "button",
      class: "small",
      onclick: async () => {
        try {
          await navigator.clipboard.writeText(text);
          copy.textContent = "コピーしました";
        } catch {
          copy.textContent = "コピーできませんでした";
        }
        setTimeout(() => (copy.textContent = "コピー"), 2000);
      },
    },
    "コピー",
  );
  const actions = h("div", { class: "msg-actions" }, copy);
  const element = h("div", { class: "msg assistant" }, activity, body, note, consulted, actions);

  const paint = () => {
    timer = null;
    painted = Date.now();
    body.replaceChildren(renderMarkdown(text));
    keepUp();
  };
  // At most one redraw per PAINT_MS while text streams in.
  const schedule = () => {
    if (timer) return;
    const wait = Math.max(0, painted + PAINT_MS - Date.now());
    timer = setTimeout(paint, wait);
  };
  const showConsulted = () => {
    const groups = [];
    if (webSources.length > 0) {
      groups.push(h("div", { class: "consulted-group" }, h("div", { class: "consulted-title" }, "参照したWeb資料"), h("ol", {}, webSources.map((source) => h("li", {}, webLink(source))))));
    }
    if (documents.length > 0) {
      groups.push(h("div", { class: "consulted-group" }, h("div", { class: "consulted-title" }, "参照したナレッジ資料"), h("ul", {}, documents.map((document) => h("li", {}, document.title)))));
    }
    consulted.replaceChildren(...groups);
    consulted.hidden = groups.length === 0;
    keepUp();
  };
  const showNote = (message, kind) => {
    note.className = kind ? `status ${kind}` : "status";
    note.textContent = message ?? "";
    keepUp();
  };
  const settle = () => {
    clearTimeout(timer);
    activity.textContent = toolCount > 0 ? `Web検索・ページ取得: ${toolCount} 回` : "";
    actions.hidden = text.trim() === "";
    element.removeAttribute("aria-busy");
    // Last, so that following the answer scrolls to its final size.
    paint();
  };

  const view = {
    element,
    append(delta) {
      if (text === "") activity.textContent = "";
      text += delta;
      schedule();
    },
    reset() {
      text = "";
      schedule();
    },
    tool(name, detail) {
      toolCount += 1;
      activity.textContent = `${TOOL_LABELS[name] ?? "ツール"}中: ${detail}`;
    },
    knowledge(list) {
      documents = list ?? [];
      showConsulted();
    },
    sources(list) {
      webSources = list ?? [];
      showConsulted();
    },
    finish(status) {
      settle();
      if (status === "stopped") showNote("回答を停止しました。", "warn");
    },
    fail(message) {
      settle();
      if (message) showNote(message, "error");
      else showNote("回答を停止しました。", "warn");
    },
  };

  showConsulted();
  switch (message.status) {
    case "streaming":
      if (text === "") {
        element.setAttribute("aria-busy", "true");
        activity.textContent = "考えています…";
        actions.hidden = true;
      } else {
        settle();
      }
      if (message.id !== undefined) {
        // Being generated in another tab or window.
        showNote("この回答は別のタブまたはウィンドウで作成中です。完了後に再読み込みすると表示されます。", "warn");
      }
      break;
    case "stopped":
      settle();
      showNote("回答を停止しました。", "warn");
      break;
    case "failed":
    case "interrupted":
      settle();
      showNote(message.error ?? "回答を作成できませんでした。", "error");
      break;
    default:
      settle();
  }
  return view;
}

window.addEventListener("hashchange", () => render());
render();
