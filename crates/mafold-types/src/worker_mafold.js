// `./mafold.js` — what a Mafold app's backend Worker needs from the platform,
// shipped by the builder beside every Worker it writes and by `mafold sites
// worker helper` to an agent writing one on a computer (`.docs/app-backend-v1.md`
// §5; the garden's `mafold-garden-beta/worker/mafold.js` is the same verbs).
//
//   · verifyLaunch(request, env) — who is calling: the launch token
//     (`Authorization: Bearer <jwt>`, HS256 with the app's secret, bound as
//     MAFOLD_APP_ID / MAFOLD_APP_SECRET) → { id, owner_id, username,
//     display_name, conv }. People's pages send `Mafold.initData`; agents'
//     calls carry their own. Key data on `id`: a username can change (a
//     person's once a month, an agent's once a day), the id never does.
//   · ownerOf(username) — a bot `ada:helper` belongs to `ada` (its id: `owner_id`).
//   · migrate(db, migrations) — bring the tables up to date on first request.
//   · publish(env, { to | conv, event, data }) — tell the people who have the
//     app open that something changed (`appPublish`); they reload what moved.
//   · roster(request, env, conv) — who is in a conversation, with ids and
//     roles (the app declares `roster`).
//   · idsOf(env, usernames) — the ids of names rows were keyed on before
//     tokens carried one (`appUserIds`), to move those rows to.

export class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

const enc = new TextEncoder();

function b64urlBytes(s) {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4);
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function b64url(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

const keys = new Map();
async function hmacKey(secret, use) {
  const k = `${use}:${secret}`;
  if (!keys.has(k)) {
    keys.set(k, await crypto.subtle.importKey("raw", enc.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, [use]));
  }
  return keys.get(k);
}

/** The verified caller: { id, owner_id, username, display_name, avatar_url, conv }.
 *  `owner_id` is whose data this is: a person's own id, an agent's person's
 *  (null for an agent with no person behind it). */
export async function verifyLaunch(request, env, nowSecs = Math.floor(Date.now() / 1000)) {
  if (!env.MAFOLD_APP_ID || !env.MAFOLD_APP_SECRET) throw new HttpError(500, "this Worker isn't linked to its Mafold app");
  const m = (request.headers.get("authorization") || "").match(/^Bearer\s+([\w-]+)\.([\w-]+)\.([\w-]+)$/i);
  if (!m) throw new HttpError(401, "open this from Mafold (no sign-in token)");
  const [, h, p, s] = m;
  let header, claims;
  try {
    header = JSON.parse(new TextDecoder().decode(b64urlBytes(h)));
    claims = JSON.parse(new TextDecoder().decode(b64urlBytes(p)));
  } catch {
    throw new HttpError(401, "unreadable sign-in token");
  }
  if (header.alg !== "HS256") throw new HttpError(401, "unexpected token algorithm");
  const ok = await crypto.subtle.verify("HMAC", await hmacKey(env.MAFOLD_APP_SECRET, "verify"), b64urlBytes(s), enc.encode(`${h}.${p}`));
  if (!ok) throw new HttpError(401, "sign-in token doesn't verify");
  if (claims.iss !== "mafold" || claims.app !== env.MAFOLD_APP_ID) throw new HttpError(401, "sign-in token is for another app");
  if (typeof claims.exp !== "number" || claims.exp + 60 < nowSecs) throw new HttpError(401, "sign-in expired — reopen from Mafold");
  const username = String((claims.user && claims.user.username) || "").toLowerCase();
  if (!username) throw new HttpError(401, "sign-in token names nobody");
  const id = claims.user.id || null;
  return {
    id,
    owner_id: isBot(username) ? claims.user.owner_id || null : id,
    username,
    display_name: claims.user.display_name || username,
    avatar_url: claims.user.avatar_url || null,
    conv: claims.conv || null,
  };
}

/** `ada:helper` → `ada`; `ada` → `ada`. */
export function ownerOf(username) {
  const i = username.indexOf(":");
  return (i < 0 ? username : username.slice(0, i)).toLowerCase();
}

export function isBot(username) {
  return username.includes(":");
}

let migrated = false;

/** Apply what's missing of `migrations` ([{ id, sql }], in order). */
export async function migrate(db, migrations) {
  if (migrated) return;
  await db.prepare("CREATE TABLE IF NOT EXISTS _mafold_migrations (id TEXT PRIMARY KEY, applied_at INTEGER NOT NULL)").run();
  const { results } = await db.prepare("SELECT id FROM _mafold_migrations").all();
  const done = new Set(results.map((r) => r.id));
  for (const m of migrations) {
    if (done.has(m.id)) continue;
    const stmts = m.sql
      .split(/;\s*(?:\n|$)/)
      .map((s) => s.trim())
      .filter(Boolean)
      .map((s) => db.prepare(s));
    stmts.push(db.prepare("INSERT INTO _mafold_migrations (id, applied_at) VALUES (?, ?)").bind(m.id, Date.now()));
    try {
      await db.batch(stmts);
    } catch (e) {
      const again = await db.prepare("SELECT 1 AS x FROM _mafold_migrations WHERE id = ?").bind(m.id).first();
      if (!again) throw e;
    }
  }
  migrated = true;
}

// api.mafold.com's edge turns away library user-agents.
const UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

function apiBase(env) {
  return String(env.MAFOLD_API || "https://api.mafold.com").replace(/\/+$/, "");
}

/** An app JWT: this app speaking for itself, five minutes. */
async function appToken(env) {
  const now = Math.floor(Date.now() / 1000);
  const h = b64url(enc.encode(JSON.stringify({ alg: "HS256", typ: "JWT" })));
  const p = b64url(enc.encode(JSON.stringify({ iss: "app", app: env.MAFOLD_APP_ID, iat: now, exp: now + 300 })));
  const sig = await crypto.subtle.sign("HMAC", await hmacKey(env.MAFOLD_APP_SECRET, "sign"), enc.encode(`${h}.${p}`));
  return `${h}.${p}.${b64url(new Uint8Array(sig))}`;
}

/** Tell the people who have this app open that something changed: `to` (ids, or
 *  usernames) or `conv` (everyone in a conversation the app is installed in). Their open
 *  copies get `Mafold.on("appEvent", ({event, data}) => …)`; nothing is kept for
 *  anyone not looking. Best effort: a failure is returned, never thrown. */
export async function publish(env, { to, conv, event, data }) {
  try {
    const res = await fetch(`${apiBase(env)}/api/appPublish`, {
      method: "POST",
      headers: { "content-type": "application/json", "user-agent": UA, authorization: `Bearer ${await appToken(env)}` },
      body: JSON.stringify({ to, conv, event, data }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

/** Who is in `conv` — [{ id, username, display_name, avatar_url, role }] — asked with
 *  the caller's own launch token (they must be in it; the app declares `roster`). */
export async function roster(request, env, conv) {
  const token = (request.headers.get("authorization") || "").replace(/^Bearer\s+/i, "");
  const res = await fetch(`${apiBase(env)}/api/appRoster`, {
    method: "POST",
    headers: { "content-type": "application/json", "user-agent": UA },
    body: JSON.stringify({ conv, init_data: token }),
  });
  const data = await res.json().catch(() => null);
  if (!res.ok || !data || !data.ok) throw new HttpError(res.status === 401 ? 401 : 502, "couldn't read the conversation's members");
  return data.result.members;
}

/** `{ <username>: <id> | null }` for names this app keyed rows on before launch
 *  tokens carried an id — what to move those rows to. A name leads to the
 *  account that wrote under it only while nobody else can have it (30 days
 *  after it changes hands), so move them soon after tokens carry ids. */
export async function idsOf(env, usernames) {
  const names = [...new Set(usernames.map((u) => String(u).toLowerCase()).filter(Boolean))];
  const out = {};
  for (let i = 0; i < names.length; i += 200) {
    const res = await fetch(`${apiBase(env)}/api/appUserIds`, {
      method: "POST",
      headers: { "content-type": "application/json", "user-agent": UA, authorization: `Bearer ${await appToken(env)}` },
      body: JSON.stringify({ usernames: names.slice(i, i + 200) }),
    });
    const data = await res.json().catch(() => null);
    if (!res.ok || !data || !data.ok) throw new HttpError(502, "couldn't ask Mafold for its people's ids");
    Object.assign(out, data.result.ids);
  }
  return out;
}

/** Wrap a handler: JSON in, JSON out, `HttpError` → its status. */
export function json(handler) {
  return async (request, env, ctx) => {
    try {
      const out = await handler(request, env, ctx);
      return out instanceof Response ? out : Response.json(out ?? { ok: true });
    } catch (e) {
      const status = e instanceof HttpError ? e.status : 500;
      return Response.json({ error: e && e.message ? e.message : String(e) }, { status });
    }
  };
}
