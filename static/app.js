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

export const routes = [
  { path: /^\/$/, refresh: true, view: homeView },
  { path: GUILD_PATH, view: guildView },
];

// A server page opened while logged out (for example the link of /config show) is reopened
// after the login, which always returns to "/". sessionStorage survives the trip to Discord
// within the tab; without it the user simply lands on the server list.
const RETURN_KEY = "return-to";

function rememberReturn() {
  const path = location.hash.replace(/^#/, "");
  try {
    if (GUILD_PATH.test(path)) {
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
    return path !== null && GUILD_PATH.test(path) ? path : null;
  } catch {
    return null;
  }
}

let rendering = 0;

async function render() {
  const current = ++rendering;
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

window.addEventListener("hashchange", () => render());
render();
