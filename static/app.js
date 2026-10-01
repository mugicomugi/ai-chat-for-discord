// Web UI of the Discord bot. A plain ES module without a build step and without inline scripts
// or styles (the Content-Security-Policy forbids both). Text from the API is only ever inserted
// as text nodes, never as HTML.
//
// Screens are registered in `routes`; pages of a server are registered in `guildTabs` with the
// right they need, so knowledge management (M3) and the chat (M4) are added the same way.

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
];

export const routes = [
  { path: /^\/$/, refresh: true, view: homeView },
  { path: /^\/guilds\/(\d{1,20})(?:\/([a-z-]+))?$/, view: guildView },
];

let rendering = 0;

async function render() {
  const current = ++rendering;
  const path = location.hash.replace(/^#/, "") || "/";
  const route = routes.find((candidate) => candidate.path.test(path));
  let content;
  try {
    const me = await loadMe(route?.refresh);
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
      "Discord アカウントでログインすると、Bot を導入しているサーバーでのあなたの権限を確認でき、サーバー管理者は Bot を使えるロールを設定できます。",
    ),
    h("p", {}, h("a", { class: "button primary", href: "/auth/login" }, "Discord でログイン")),
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
    tabs.length > 0 && h("a", { class: "button", href: `#/guilds/${guild.id}` }, "開く"),
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
    help: "選んだロールを持つ人が、今後追加するナレッジ（資料）を管理できます。サーバー管理権限を持つ人は、選ばなくても管理できます。",
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

window.addEventListener("hashchange", () => render());
render();
