//! The feature-flag registry (.docs/feature-flags.md) — every key, its
//! compile-time default and its metadata, in ONE place both sides read.
//!
//! A flag is born HERE (invariant ①: the server record is a rollout override
//! only, never a second default). The client core resolves the flags its
//! clients read from this table (`mafold-core/src/flags.rs`); the api reads it
//! for the flags it gates on — what a key means when it has no record — and
//! the admin portal lists it, so what the page says a missing record does is
//! the declaration the server obeys, not a description of it.
//!
//! Lives in the wire crate, not the core, because the api reads it too. Delete
//! an entry together with its gate code when the feature hits 100%.

use serde::Serialize;

/// A client that reads a flag (gates UI or behaviour on it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagClient {
    Web,
    /// The phone app (mafold-rn).
    Phone,
    /// The bot daemon (mafold-cli), evaluated as the bot's own handle.
    Daemon,
}

/// Whether the api gates on a flag, and what it decides for a key with no
/// record. The two directions really differ by key — deleting a record turns
/// some features ON server-side and others OFF — so each flag says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagServer {
    /// The api never reads it; only clients do.
    NotRead,
    /// No record ⇒ the server lets everyone through (a cutover that changed
    /// nothing until someone wrote a record).
    OpenWithoutRecord,
    /// No record ⇒ the server keeps it shut (something opened on purpose:
    /// a launch, a new way to spend money).
    ClosedWithoutRecord,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct FlagMeta {
    pub key: &'static str,
    /// Production default on the clients (when neither an override nor a
    /// server value exists).
    pub default: bool,
    /// Default in dev/internal builds (`set_flags_dev(true)`) — preserves the
    /// old devmode.ts behavior of dev-only tabs without magic overrides.
    pub dev_default: bool,
    pub label: &'static str,
    pub description: &'static str,
    /// Who owns the flag's lifecycle (flags are temporary — see the doc §6).
    pub owner: &'static str,
    /// yyyy-mm added — lets a periodic check surface stale flags.
    pub added: &'static str,
    /// Which clients read it. Empty = only the api does; the client core
    /// leaves such a flag out of what it resolves.
    pub clients: &'static [FlagClient],
    pub server: FlagServer,
}

use FlagClient::{Daemon, Phone, Web};
use FlagServer::{ClosedWithoutRecord, NotRead, OpenWithoutRecord};

/// The registry. Keys must match the server's control-plane records and the
/// gate code on every client.
pub const KNOWN_FLAGS: &[FlagMeta] = &[
    // `provider:<id>` holds a connection provider back for a cohort: "add a
    // connection" offers it only where the flag resolves on (web's
    // `providerOffered`; the onboarding guide asks the same before it names
    // Google). Google's app is in Testing — Google lets only listed
    // test accounts consent — so its rollout is an account list, not a percent.
    FlagMeta {
        key: "provider:google",
        default: false,
        dev_default: true,
        label: "Google connection",
        description: "Offers Google (Gmail + Calendar) under Add connection. Its Google app is in \
                      Testing: only invited test accounts can consent. The onboarding guide reads \
                      it too, to offer Gmail only where the card can open it.",
        owner: "ops",
        added: "2026-09",
        clients: &[Web],
        server: ClosedWithoutRecord,
    },
    // ModelArk (a user's OWN Seedance key) is held back until the video line's
    // e2e passes on devapi (@realopsdu 2026-10-06). Registered BEFORE the
    // provider lands: the connection pack goes live within 15 minutes of the
    // merge that adds a provider, while this gate reaches the web only with
    // its next release — so the gate has to be out there first.
    FlagMeta {
        key: "provider:modelark",
        default: false,
        dev_default: true,
        label: "ModelArk connection",
        description: "Offers BytePlus ModelArk (Seedance video, on your own key) under Add \
                      connection. Held back until the video line launches.",
        owner: "linsky",
        added: "2026-10",
        clients: &[Web],
        server: NotRead,
    },
    // `modelBot:<handle>` is an official model bot's LAUNCH: the server keeps
    // the bot coming-soon (off the roster, silent in its DMs) for everyone the
    // flag is not on for, and a key with no record is CLOSED there — a launch
    // is opened on purpose (`Store::model_bot_live_for`). Named by the bot's
    // own profile (`templates/official/<handle>.json` `launch_flag`); clients
    // read nothing from it, the roster they fetch already reflects it.
    FlagMeta {
        key: "modelBot:gemini",
        default: false,
        dev_default: false,
        label: "Gemini official bot",
        description: "Opens @gemini (Gemini on the router's Google seat, paid from the reader's \
                      google wallet). Until it is on for you, @gemini stays coming soon.",
        owner: "ops",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    FlagMeta {
        key: "starterTutorial",
        default: false,
        dev_default: true,
        label: "Starter tutorial",
        description: "Three guided starter quests in Activity Center.",
        owner: "ops",
        added: "2026-09",
        clients: &[Web],
        server: OpenWithoutRecord,
    },
    FlagMeta {
        key: "showIds",
        default: false,
        dev_default: false,
        label: "Show chat & message IDs",
        description: "Profiles show a copyable chat id; message menus show the message id.",
        owner: "ops",
        added: "2026-07",
        clients: &[Web, Phone],
        server: NotRead,
    },
    FlagMeta {
        key: "gardenApps",
        default: false,
        dev_default: true,
        label: "Moments & Garden",
        description: "Reveals the bottom tab switcher (Moments / Chats / Garden) under the chat list.",
        owner: "ops",
        added: "2026-07",
        clients: &[Web, Phone],
        server: OpenWithoutRecord,
    },
    FlagMeta {
        key: "gardenOnePath",
        default: false,
        dev_default: false,
        label: "New garden (mafold/garden)",
        description: "The garden opens mafold/garden and 收进园地 writes its pins, on web and phone \
                      alike. Off = the pre-switch path (web: opsdu/garden-beta + the old garden room; \
                      phone: the garden DM). Turn on for everyone once garden.mafold.app is switched, \
                      then delete the old path with this flag.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    FlagMeta {
        key: "gardenWorksV2",
        default: false,
        dev_default: false,
        label: "Garden activity v2",
        description: "动态 also shows other people's agents when they answer you, marked with whose they \
                      are, and the ones writing to you right now; card type definitions are no longer works. \
                      (Its work series were never shipped — gardenWorksV3 replaced them, linsky 10-08.) \
                      Read by getAppViewerProfile for the viewer.",
        owner: "linsky",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // linsky 10-08: 「生成的东西忠实地全部放进来,不让 agent 判断」.
    FlagMeta {
        key: "gardenWorksV3",
        default: false,
        dev_default: false,
        label: "Garden works v3 (every output)",
        description: "The garden's 作品 are everything the person's own agents produced, judged by nobody: \
                      every HTML card and every file on every finished message they sent, wherever the \
                      person can still read it (getAppViewerProfile `outputs`, listViewerOutputs, \
                      getViewerOutput). Card type definitions are no longer works.",
        owner: "linsky",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // linsky 11-03 收藏 rework (#1487), put behind a flag after the fact (opsdu 11-05:
    // 「新功能要 write behind flags,不然会影响发版节奏」).
    FlagMeta {
        key: "gardenSavedV2",
        default: false,
        dev_default: true,
        label: "Garden 收藏 v2 (fixed shelves)",
        description: "The garden's 收藏 on five fixed shelves (网站, 文章&书籍, 图片, 视频, 消息) \
                      with real cards, a website's from getViewerLinkCards (its share image and a \
                      real screenshot). Read by getAppViewerProfile for the viewer and gates \
                      getViewerLinkCards. Off = the garden's old 收藏 page, and getViewerLinkCards \
                      answers no cards. (收藏进园地 left the chat menus outright in #1497.)",
        owner: "linsky",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // opsdu 11-07: what he called 「建站功能 garden 里面被删了」 is 作品's sites —
    // a site a mini app runs on folds into the app (#816), and none has a picture.
    FlagMeta {
        key: "gardenSites",
        default: false,
        dev_default: true,
        label: "Garden 作品: 站点 row",
        description: "A 站点 row at the top of the garden's 作品 tab: every *.mafold.app site of                       the person and their agents, the mini app that runs on it, whether the plan                       holds it back, and a real screenshot (the 收藏 site cards' page_shots track,                       one per deploy). Read by getAppViewerProfile (`sites`). Off = no `sites`,                       no screenshot asked for, and 作品 as before.",
        owner: "linsky",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // Owner 10-06, after seeing it live: "样式还行但不能这样直接上,要隐藏".
    // Off until the owner turns it on. A phone binary built before this entry doesn't
    // know the key and resolves it off too; turning it ON for phones therefore
    // needs the next app@ binary.
    FlagMeta {
        key: "premiumRing",
        default: false,
        dev_default: false,
        label: "Premium avatar ring",
        description: "The blue → pink ring around a Premium member's avatar (heartbeat ripples while \
                      they are online, still while offline, opened where the online dot sits), on web \
                      and phone alike. Off = no ring; the crown by the name is unaffected.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // Read by the daemon (`mafold-cli`), not a UI: a bot evaluates as its own
    // handle, and an allowlist entry naming a person covers their bots too —
    // so it is switched on per person, from the server, with nothing to set on
    // the Windows machine itself.
    FlagMeta {
        key: "windowsBackground",
        default: false,
        dev_default: false,
        label: "Windows background tasks",
        description: "On Windows, an agent's background Bash is started by the daemon with no \
                      console window, outlives the turn and is reported back in a new turn — \
                      as on macOS/Linux. Off = background Bash stays turn-scoped, as before.",
        owner: "ops",
        added: "2026-10",
        clients: &[Daemon],
        server: NotRead,
    },
    // Owner 10-22 (via realopsdu): the daemon's own «⏳ 后台还在跑» / «超过 2 小时»
    // messages go; what happens to a background task is handed to the agent,
    // which decides what to say. Read by the daemon, per person like the one
    // above; off until verified on dev and released.
    FlagMeta {
        key: "bgtaskEvents",
        default: false,
        dev_default: false,
        label: "Background tasks: events to the agent",
        description: "Each background task is watched on its own clock until it exits — no two-hour \
                      cut-off — and what happens to it (finished, killed by its timeout, log gone \
                      quiet, running very long) goes to the agent, which decides whether to say \
                      anything. The daemon posts no «still running» / «over two hours» messages of \
                      its own. Off = the old watcher: one clock per channel, those messages, and \
                      watching stops after two hours.",
        owner: "ops",
        added: "2026-10",
        clients: &[Daemon],
        server: NotRead,
    },
    // Ops 11-13 (the mafold/brain card, demo → "Build it"): an agent's turn
    // shows its work in one card that stops at a set height, the agent talks
    // to the room with `mafold send` while it works, and the turn ends on its
    // summary with the steps folded under it. Read by the daemon as the bot,
    // per person like the two above; off until seen on the real page.
    FlagMeta {
        key: "brainCard",
        default: false,
        dev_default: false,
        label: "Agents: the brain card",
        description: "While an agent works, its thinking, reads, edits and commands go into one \
                      card that stops at a set height and scrolls, with the time, tokens and Stop \
                      under it. The agent tells the room what matters with `mafold send` as it \
                      goes, and the card moves below each message it sends, so it stays last. \
                      When the agent finishes, the card gives way to its summary, with the steps \
                      folded under it. Off = the reply grows as the agent works and its trail \
                      folds into a lid above the answer, as before.",
        owner: "ops",
        added: "2026-11",
        clients: &[Daemon],
        server: NotRead,
    },
    // Owner 10-09: the left rail is fine as the wide-screen default, but it
    // goes out behind this flag, off, with its own line on the release card —
    // on only once the owner has clicked it there.
    FlagMeta {
        key: "folderRail",
        default: false,
        dev_default: false,
        label: "Folder tabs on the left",
        description: "On a wide screen the chat-list folder tabs become a rail down the column's left \
                      edge (mafold-mac's sidebar rail; the desktop shell's window lights at its head), \
                      and Settings > Chat Folders offers left / top. Off = the strip across the top, \
                      no choice offered.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web],
        server: NotRead,
    },
    // realopsdu 10-11: people convert by hand because nothing says payments
    // already convert. The wallet's head says it now; off until verified on
    // dev and released.
    FlagMeta {
        key: "walletAutoFirst",
        default: false,
        dev_default: false,
        label: "Wallet: auto convert first",
        description: "The wallet states auto convert in its head (the switch, the funding pocket) and \
                      Convert drops from the action pills to one row under it; the Settings tab goes, \
                      the starter tutorial's convert scene teaches auto convert, and the Premium card \
                      moves from the wallet to Settings. Off = the old layout. (Activity names auto \
                      converts either way — that reads the server's `auto` field, not this flag.)",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // The wallet re-laid (ops 10-29, #钱包页改版 demo v3.2). Implies the
    // auto-convert-first behaviour above whatever that flag says.
    FlagMeta {
        key: "walletV2",
        default: false,
        dev_default: false,
        label: "Wallet: re-laid page",
        description: "A top row (Search, Model prices, How it works), the balance with provider pills \
                      instead of the slider, four round buttons (Deposit, Transfer, Details, Convert), \
                      an Activity card that opens with the auto-convert switch and shows each auto \
                      convert under the payment it funded, then Holdings and Authorized to spend as \
                      cards; on web one band across the top and 2–3 card columns. Off = today's page.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // The re-laid profile page (RN + web Profile). Its tab bar asks the api
    // `getProfileTabs`; an api without the route just means no tab is left out.
    FlagMeta {
        key: "profileV2",
        default: false,
        dev_default: false,
        label: "Profile: re-laid page",
        description: "One header layout with or without a banner (avatar + actions, name, @handle · \
                      presence, bio, who runs a bot), clear-history in ⋯, and a tab bar that keeps \
                      only the tabs with something in them. Off = the old layout.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // Sending pictures as `.docs/thumbnail-v1.md` Phase 2: the ≤2560 view and
    // the inline strip go up, the original only when "Original" is ticked. Off =
    // the picked file goes up as it is. Irreversible per send (an original not
    // uploaded is never on the server), hence a flag tried by the owner first.
    FlagMeta {
        key: "thumbSend",
        default: false,
        dev_default: false,
        label: "Pictures: send view + strip",
        description: "A picture is sent as its view (fitted into 2560, screenshots lossless, photos q85) \
                      with a 40px inline strip; the composer offers \"Original\" (off, not remembered) \
                      to send the untouched file beside it. Off = the picked file is uploaded as is.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone, Daemon],
        server: NotRead,
    },
    // Inline media in replies (`.docs` design: 回复嵌图方案稿 v4): open_url lists
    // a page's pictures, `unfurlUrl` answers, and the media/cite cards are
    // offered to agents.
    FlagMeta {
        key: "inlineMedia",
        default: false,
        dev_default: false,
        label: "Inline media in replies",
        description: "Agents can put a page's real pictures in a reply: open_url lists them, unfurlUrl \
                      answers, and the media / cite cards are offered. Off = none of it.",
        owner: "ops",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // Room retirement (#1203/#1204) as hosted bots teach it: @mafold's app
    // builder and the bundled product help send one-chat data (a poll, a game,
    // a roll call) to an html card's shared state and lasting data to an app's
    // own backend, instead of a room. web@0.1.171 / app.js@30 can't write that
    // card state or deliver `appEvent`, so it stays shut until the clients
    // that can are live (card 18) — realopsdu 10-29.
    FlagMeta {
        key: "roomRetireHosted",
        default: false,
        dev_default: false,
        label: "Hosted bots teach room-free apps",
        description: "@mafold's app builder answers a poll, a game or a roll call with an html card that \
                      keeps shared state, and builds shared apps with their own backend instead of a room; \
                      the hosted product help says the same. Read for the person the turn answers to. \
                      Off = the builder and the help teach the room, as before.",
        owner: "ops",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // The api reads it for Moments push notifications: a person without the
    // tab must not be pinged about a feed they can't open (`push.rs`).
    FlagMeta {
        key: "moments",
        default: false,
        dev_default: true,
        label: "Moments",
        description: "The Moments tab (author feed + composer). WIP — gated separately from gardenApps so Garden can ship without exposing unfinished Moments.",
        owner: "ops",
        added: "2026-07",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // ── Read by the api only ──────────────────────────────────────────────
    // The admin portal's Premium switch (`adminPremiumPause` / `Reopen`) is
    // this record: `off` stops new sales, deleting it reopens them.
    FlagMeta {
        key: "premiumSales",
        default: false,
        dev_default: false,
        label: "Premium checkout",
        description: "New members can start Premium. Off = sales paused; existing members keep theirs. \
                      The Overview's Premium switch writes this same record.",
        owner: "ops",
        added: "2026-10",
        clients: &[],
        server: OpenWithoutRecord,
    },
    FlagMeta {
        key: "topups",
        default: false,
        dev_default: false,
        label: "Wallet top-up packs",
        description: "Offers wallet top-up packs (Stripe, or the App Store from an iPhone). Also needs \
                      Premium checkout open.",
        owner: "ops",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    FlagMeta {
        key: "iap",
        default: false,
        dev_default: false,
        label: "App Store in-app purchase",
        description: "Offers App Store purchases (Premium, top-ups) to the account instead of Stripe.",
        owner: "ops",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    FlagMeta {
        key: "iapSandbox",
        default: false,
        dev_default: false,
        label: "App Store sandbox credit",
        description: "Accounts whose App Store sandbox (TestFlight) purchases are credited like real ones.",
        owner: "ops",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // Stripe's checkout (ops 11-04: "pivot all payment methods to waffo").
    // Off = nobody starts a new Stripe purchase; a Stripe subscription that
    // already runs keeps its billing portal and its webhook. A person's
    // switch, so going back takes no release.
    FlagMeta {
        key: "stripeCheckout",
        default: false,
        dev_default: false,
        label: "Sell through Stripe",
        description: "Stripe's checkout for Premium, the Pro Pass and credit. Off = Waffo sells on the \
                      web and Android instead; anyone already subscribed through Stripe keeps Manage.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: OpenWithoutRecord,
    },
    // Waffo Pancake, the merchant of record for Alipay and WeChat Pay (ops
    // 11-02): its buttons on the Premium and top-up cards, and the server's
    // `waffo|` checkout. The webhook credits whatever it was opened for.
    FlagMeta {
        key: "waffo",
        default: false,
        dev_default: false,
        label: "Pay with Alipay / WeChat Pay",
        description: "Offers Waffo's checkout (Alipay, WeChat Pay, cards) on the Premium and top-up \
                      cards: monthly Premium, 1 or 12 prepaid months that don't renew, and credit. \
                      Prices are before tax, which Waffo adds. Never offered inside the iPhone app.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    FlagMeta {
        key: "imageGen",
        default: false,
        dev_default: false,
        label: "Image generation tool",
        description: "Agents get the image tool (Gemini image models, paid from the person's wallet). A \
                      new way to spend someone's wallet is only ever opened on purpose.",
        owner: "ops",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // The activity center's two new tabs (owner 10-21, demo 800e27fa; plan
    // DEV #desktop优化 3efe3021). Two flags, not one: the 电脑 tab ships with
    // phase one, the 回报 tab needs phase two's billing ids first, and neither
    // should wait for the other. Both gate the panel only — the cards answer
    // nobody but the asker about their own computers and replies either way.
    FlagMeta {
        key: "activityComputer",
        default: false,
        dev_default: true,
        label: "Activity center: Computers tab",
        description: "The activity center's dial becomes 电脑 / 回报 / 任务 and the 电脑 tab shows the \
                      mafold/computers card: what needs you, your bots at work on each computer, and \
                      (in the desktop app) this computer's controls; Settings gets a Computers section. \
                      Off = the old dial.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    FlagMeta {
        key: "activityReport",
        default: false,
        dev_default: true,
        label: "Activity center: Reports tab",
        description: "The 回报 tab shows the mafold/receipts card: today's replies by your bots and to \
                      you, what each cost per currency (or own subscription), its timeline and settlement. \
                      Needs activityComputer for the three-tab dial. Off = the tab is not shown.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // New Agent in three steps (ops 10-29, DEV #新建agent流程). It replaces
    // the New Agent editor for everyone, so it waits behind this until ops
    // says ship to all; the old editor stays whole for flag-off and is removed
    // only then. createBot's `sign_in` is optional and only the panel sends
    // it, so the server needs no gate of its own.
    FlagMeta {
        key: "newAgentFlow",
        default: false,
        dev_default: true,
        label: "New Agent in three steps",
        description: "New Message's ✦ mode is New Agent in three steps, in the same modal (web: its \
                      460 × ≤680 card; phone: its full-screen sheet): ① who (avatar, name, username) \
                      ② where (Mafold Cloud or one of your computers, online or not) ③ runtime and \
                      what it signs in with. Off = the old editor (web) / the New Bot form (phone).",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // Onboarding step 3 (ops 10-29 "Build it"): @mafold sets up a newcomer's
    // first AI. Behind this until ops has seen it work end to end on dev; it
    // is separate from newAgentFlow, which ops opens to everyone on his own
    // call. Only the api reads it: it decides whether @mafold has the tool
    // and the instructions, and the card's tap re-checks it. The card's
    // «Set it up myself» door opens whatever New Agent the reader has.
    FlagMeta {
        key: "onboardingAgent",
        default: false,
        dev_default: false,
        label: "@mafold sets up your first AI",
        description: "@mafold can propose an AI for the person it is talking to — name, persona, \
                      Run on Mafold paid from their wallet — as a card. One tap on Create builds it \
                      in their namespace (bot_plans: one approval, used once); «Set it up myself» \
                      opens New Agent with Run on Mafold picked. Off = @mafold points at New Agent \
                      in words.",
        owner: "ops",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // One request card for every consent (ops 11-11, on opus48's 14-card set):
    // Mafold asks, the request is held by the server beside the card, and the
    // tap names only a choice among what the card offered. Only the api reads
    // it — it decides which card a request is posted as, keyed on whoever is
    // being asked; the card draws whatever it is sent.
    FlagMeta {
        key: "consentV2",
        default: false,
        dev_default: false,
        label: "Requests: one consent card",
        description: "A request to use a connection, spend from a wallet, read a chat or build \
                      apps is posted as one card, mafold/consent: who asks, what of yours, why in \
                      its own words, and the length, cap or scopes as choices, the asked one picked. \
                      The request is held by the server; a tap carries only the choice. Allowed \
                      cards say what was given and until when, and can take it back. Off = the four \
                      cards of before.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // The update pill and its loading screen (owner 10-22, demo upd-demo v4:
    // copy 「A 说清楚」, loading screen 「A 回到徽章」). Two flags, not one: the
    // loading screen is seen on every open and may go out on its own, ahead
    // of the pill or without it. The desktop app loads the web, so it reads
    // the Web entry. A phone binary built before these entries has no default
    // for them, but the server's record for an account still reaches it (the
    // core carries server-only keys), so opening one for an account needs no
    // app@. Rolling out = the record set on; deleting it turns the flag off.
    FlagMeta {
        key: "updatePill",
        default: false,
        dev_default: true,
        label: "Update pill",
        description: "When a newer build is ready (the web page, the phone's JS bundle, an App Store or \
                      APK version, the desktop app), the activity center's badge in the bottom tab bar \
                      stretches into a pill over the tab switcher that says what it is and offers to \
                      update; Settings > Updates shows the same state. Off = as before: the web reloads \
                      quietly at the next page switch, a phone bundle takes over at the next cold start, \
                      Android checks only from Settings, the desktop app only notifies from the system \
                      and the tray.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    FlagMeta {
        key: "bootCurtain",
        default: false,
        dev_default: true,
        label: "Loading screen: back to the badge",
        description: "From the first frame the app opens on its own ground colour with the Mafold icon \
                      in the middle (a spinner joins below it only when loading is slow); once the first \
                      screen is ready the icon flies down into the activity center's badge. Off = as \
                      before: web shows nothing, then a spinner after 400 ms; the phone shows a plain \
                      background until the first screen.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // 更新胶囊的「预览」(本人 10-24 拍「预览入口要」,更新胶囊 PV):胶囊只在真有新版时伸,本人手上的
    // 正式包这几天等不到一个 —— 给要试的号开,设置 ▸ 更新 底下几行往这台机器的胶囊里喂假的(真组件、
    // 假来源,client-shared/updatePillDemo.ts 两端一份)。生产关、dev 开:开发包、dev 环境天然看得到,
    // 原来计划里「设置 ▸ 开发者」的演示行由它替代,不另做。跟 updatePill 一样老壳也读得到服务端记录。
    FlagMeta {
        key: "updatePillPreview",
        default: false,
        dev_default: true,
        label: "Update pill: preview rows",
        description: "Only for accounts that want to try the update pill. Settings > Updates shows a few \
                      preview rows at the bottom (with updatePill on too); tapping one feeds this \
                      device's pill a pretend update — new web page, desktop app, in-place bundle, App \
                      Store, install package, waiting while busy, up to date, reset — so the real pill \
                      stretches out of the badge. Nothing really reloads, swaps the bundle, opens the \
                      store or downloads. Off = Settings > Updates as before.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // 谁能私聊我(本人 10-25 点头):Premium 可设「只收 Premium」,认证号还能设「只收名单里的人」。
    // 服务端按设置的那个人(AI 号按它的主人)看这个开关:没记录 ⇒ 存着的设置一概不生效,开关就是
    // 总闸;两端的设置项也跟着它。被拦的那一边(资料页的说明、锁住的输入框)不看开关 —— 服务端
    // 拦了,谁都该看到为什么。
    FlagMeta {
        key: "dmPrivacy",
        default: false,
        dev_default: true,
        label: "Who can message me",
        description: "Settings > Privacy and Security gets \"Who can message me\": Premium members (and \
                      verified accounts) may allow only Premium members; verified accounts may allow \
                      only the people and groups on a list. People they've chatted with keep writing; \
                      new people who don't qualify can't start a chat with them or add them to groups. \
                      An AI account is judged by its owner. Off = the setting isn't offered and any \
                      stored one is not enforced.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // The Settings ▸ Connections redesign (ops 10-29 "Build it", demo v3: tiles
    // under category tabs, search, every supported service, several accounts
    // per service). Server-read because the page brings two routes no other
    // surface has — giving one of your agents access from your own page and
    // choosing which account it uses first — and those answer only where this
    // is on, so nothing can call them before the owner opens the page.
    FlagMeta {
        key: "connectionsPage",
        default: false,
        dev_default: true,
        label: "Connections: the new page",
        description: "Settings > Connections becomes one page of tiles: every supported service under \
                      tabs (All, Agent logins, Model keys, Apps, Computers, Secrets) with a search box, \
                      what needs you first, several accounts per service, and per account which of \
                      your agents may use it, until when, and which account each agent uses first. \
                      The phone gets the same page. The api answers grantConnectionAccess and \
                      setConnectionOrder only where this is on. Off = the old Connections list.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // Garden and the Activity Center as one drawer (ops 11-08, demo rounds
    // 1–5 in their DM's #设计系统统一): the M slides the panel out of the
    // window's left edge. (The header's garden button it replaced is gone
    // for everyone, ops 11-18.) The web's; the phone reads activityDrawerPhone.
    FlagMeta {
        key: "activityDrawer",
        default: false,
        dev_default: false,
        label: "Activity: one drawer for Garden and the Activity Center",
        description: "Tapping the M slides one panel out of the window's left edge, as tall as the \
                      chat list and as wide as the list up to its new-chat button, with Now, \
                      Agents, Works and Tasks. The M rolls into its bottom-right corner and the \
                      tab bar steps aside while it's open. Off = the Activity Center that grows \
                      out of the M.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web],
        server: NotRead,
    },
    // linsky/opsdu 11-14: a link in a chat message draws a card under it, the
    // way WeChat draws a shared link — the page's name, title, words and
    // picture, from the same page cache the garden's site cards read
    // (`resolvePage`). The composer shows the card before it goes, with a ✕
    // to send the link bare. Server-read: `resolvePage` answers for pages
    // with this on (as it does with inlineMedia), and `sendMessage` keeps the
    // sender's `link_preview_options` only from someone it is on for.
    FlagMeta {
        key: "chatLinkCards",
        default: false,
        dev_default: true,
        label: "Chat: link cards",
        description: "A link in a message draws a card under it: the site's name and icon, the \
                      page's title, a line of what it says and its picture (resolvePage, the \
                      cache the garden's site cards share). A message that is only a link becomes \
                      the card. The composer shows the card above the box before sending, with a \
                      ✕ to send the link bare and a switch when the text has several links. \
                      Off = links stay links, as before.",
        owner: "linsky",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // linsky 11-13: the old garden's 收藏, native in the drawer that replaced the garden page —
    // a 收藏 tab between Works and Tasks, read from the garden's pins (GET /api/pins).
    FlagMeta {
        key: "drawerSaved",
        default: false,
        dev_default: true,
        label: "Activity drawer: 收藏 (Saved)",
        description: "A 收藏 tab in the Activity drawer, between Works and Tasks: what you saved to \
                      the garden (select ▸ forward ▸ 园地, and the old garden's collection) on its \
                      five fixed shelves — 网站, 文章&书籍, 图片, 视频, 消息 — each with its own card. \
                      Read from the garden's own pins; a website's card comes from \
                      getViewerLinkCards (gardenSavedV2). Off = the drawer without 收藏. The web's \
                      drawer (activityDrawer) and the phone's (activityDrawerPhone) alike; on the \
                      phone a document's cover is the server's drawing of its first page \
                      (fileCovers), else a book's face.",
        owner: "linsky",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // linsky 11-17: the avatar being chosen wears a ring in its own colours, like
    // Dia's tile frame — the folded list, the forward picker and drop rows alike.
    FlagMeta {
        key: "avatarHalo",
        default: false,
        dev_default: true,
        label: "Avatar halo while choosing",
        description: "Where the reader is choosing a chat by its avatar — the folded chat list, \
                      the forward picker's rows, a chat row a file is dragged over — the avatar \
                      gets a thin ring in its own colours, each where it sits in the picture \
                      (lib/avatarHalo.ts). Under the pointer the ring turns, a soft glow breathes \
                      outside it and the avatar comes forward a little; once chosen it slows to a \
                      stop and the glow goes out; the folded list's open chat keeps it, still. \
                      Off = the accent ring and the accent breathing circle, as before.",
        owner: "linsky",
        added: "2026-11",
        clients: &[Web],
        server: NotRead,
    },
    // linsky 11-19 「这个分享页太阳痿了…完全让人没有分享欲」, six versions; opsdu 11-20
    // 「1 及格了」— the 名片 (business card). opsdu 11-19 in the same thread: Premium
    // on the phone is a page of its own, like the other Settings rows.
    FlagMeta {
        key: "shareCard",
        default: false,
        dev_default: true,
        label: "Share card + Premium page (phone)",
        description: "The QR button at the top of Settings opens the share card full screen: a \
                      white card on the homepage's ground — the profile's cover, avatar, name and \
                      badges, @handle, bio, the Nth-user number, and the profile's code with the \
                      app icon in its middle. The card leans toward a finger that holds it and \
                      settles back flat. Below it: Save image, Copy link, Share. On the phone \
                      layout the Premium row opens a Settings page of its own (back arrow, \
                      title) instead of a sheet. Off = the small QR sheet and the Premium sheet, \
                      as before.",
        owner: "linsky",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // linsky 11-18 (demo v3, msg 8f7ff775): the drawer's cards go into a chat by
    // dragging them there; opsdu 11-18: nothing goes without a confirm.
    FlagMeta {
        key: "drawerDrag",
        default: false,
        dev_default: true,
        label: "Activity drawer: drag cards into a chat",
        description: "A card in the Activity drawer's 收藏 and 作品 can be dragged out. Beside an \
                      open chat: let go over its messages and a confirm asks before it goes; let \
                      go on the composer and it waits there to be sent (a file as an attachment, a \
                      website as its address). Without a chat beside it (a narrow window): the \
                      drawer steps aside, the chat list is where it goes, and the me pill at the \
                      top says to whom and holds the confirm. Files go by their id (no upload), \
                      websites as their address (chatLinkCards draws the card), saved messages \
                      and cards as a forward. Off = the cards only open, as before.",
        owner: "linsky",
        added: "2026-11",
        clients: &[Web],
        server: NotRead,
    },
    // The desktop window's own corners (ops 11-07: "desktop's four corners
    // line up with the chat list's"). The page hands this to the shell (cap
    // `window-corners`), which makes the window accordingly at its next launch:
    // see-through or not is decided when a window is made. mafold-desktop
    // windowCorners.ts has why the line round a dark window stays.
    FlagMeta {
        key: "windowCorners",
        default: false,
        dev_default: false,
        label: "Desktop: window corners follow the chat list",
        description: "macOS desktop app: the main window's four corners follow the chat list's \
                      frame, 30pt, in the wide layout (a phone-width window and full screen keep \
                      the system's). The window keeps its shadow, and so the line macOS draws \
                      round a dark window. Takes effect the next time the app starts. Off = the \
                      system's window.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web],
        server: NotRead,
    },
    // Channels in every conversation (ops 2026-10-29: "let the discussion
    // forums be a default feature open at every conversation (even human to
    // human)"). Nothing to switch on: the first channel a flag holder makes
    // turns channels on for that chat, for everyone in it — `is_forum` keeps
    // meaning "this chat has channels", so people without the flag (and older
    // apps) see exactly the forums that really have channels. Server-read:
    // `createChannel` takes a chat that isn't a forum yet only from a caller
    // this is on for.
    FlagMeta {
        key: "forumEverywhere",
        default: false,
        dev_default: false,
        label: "Channels in every chat",
        description: "Every conversation (groups, DMs with people and with agents, your saved notes) \
                      shows the #general pill from the start; tapping it lists #general and New \
                      channel. The first channel you make turns channels on for that chat, for \
                      everyone in it — no Open discussion switch. The api lets createChannel turn a \
                      chat into a forum only where this is on, and an agent it is on for is told \
                      any chat can hold channels. Off = channels only where someone opened \
                      discussion.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone, Daemon],
        server: ClosedWithoutRecord,
    },
    // Work an agent hands another goes in a thread under the message that
    // handed it out, and only the conclusion comes back to the timeline
    // (linsky 2026-10-31, `.docs/a2a-threads.md`). Daemon: where a reply lands,
    // sessions per thread, the prompt, the `--up` reminder. Server-read: the
    // hosted dispatcher's landing and who a thread reply wakes.
    FlagMeta {
        key: "a2aThreads",
        default: false,
        dev_default: false,
        label: "Agent work in threads",
        description: "When an agent @-s another agent in a group, the reply goes in a thread under \
                      that message, each thread keeps its own session, agents report back once and \
                      the conclusion goes on the timeline. The thread bar says whether it's still \
                      being worked on, and only the people in a thread get a push for it. Off = \
                      agents answer each other on the timeline.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone, Daemon],
        server: ClosedWithoutRecord,
    },
    // The model switch on the usage card (ops 2026-10-29: "the usage card
    // needs a place to switch model"). No client reads it: the card asks the
    // api for its menu (`model|menu`), and the answer says whether the switch
    // is there for this reader — a card can't read flags, and the same card
    // goes to every app at once.
    FlagMeta {
        key: "usageModelSwitch",
        default: false,
        dev_default: false,
        label: "Usage card: switch model",
        description: "Run details (tap the usage chip at the end of an agent's reply) gets a Change \
                      link on its Model row: pick the agent's model and effort right there, saved \
                      from the next reply. The owner picks for this chat (default) or every chat; \
                      anyone else only for their own replies, and only where the owner lets them. \
                      The api answers the card's model| taps only where this is on.",
        owner: "ops",
        added: "2026-10",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // The 新朋友 card (ops 10-24 design, built 10-30): a new account's way to
    // its first AI and its first friend, at the end of the chat list. Gone for
    // good once closed, once both are done, or once the list holds 9 chats —
    // that bit is a mark in the account's first-run record
    // (`recordTutorialEvent` `friends.closed`), which this opens on its own.
    FlagMeta {
        key: "newFriends",
        default: false,
        dev_default: false,
        label: "Chat list: New friends card",
        description: "A card after the last chat (2.5 rows tall) with two tiles: Create your AI \
                      (opens New Agent) and Add a friend (opens New Message's search; its link \
                      copies your invite link). A tile turns into a ✓ once you have a chat with \
                      your own AI / with a person. ✕ closes it; it also goes by itself once both \
                      are done or once you have 9 chats, and never comes back on any device. The \
                      api lets `recordTutorialEvent` take `friends.closed`, and `getTutorial` \
                      answer, where this is on.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // The wallet fuelling an agent on your own computer (PR 4b, 10-31): New
    // Agent offers "From your tokens" for Mafold Harness, and each
    // reply spends through a one-reply router key the api mints for it.
    // Server-read on the owner: createBot takes `sign_in: wallet` on a
    // computer, and `mintReplyKey` answers, only where this is on — so a
    // client showing the row early can't make an agent nothing can fuel.
    FlagMeta {
        key: "walletOnComputers",
        default: false,
        dev_default: false,
        label: "New Agent: the wallet on a computer",
        description: "New Agent on one of your computers offers From your tokens as the sign-in for \
                      Mafold Harness. Each reply the agent starts for you gets its own \
                      router key (DeepSeek chat models only, dead when the reply ends, on Stop, or \
                      after 6 h); what it spends comes off your wallet under the agent's monthly \
                      cap and shows on the reply's result card. Only you start its wallet replies. \
                      The api takes createBot with the wallet on a computer, and answers \
                      mintReplyKey, only where this is on for the owner.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // OpenCode Go (ops 10-31): Mafold Harness signed in with your OpenCode Go
    // plan. The key is pasted once in Settings ▸ Connections (the one
    // pasted-key exception, ops 10-29) and opened by your computer for each
    // reply. Server-read on the owner: createBot takes `sign_in: opencode-go`
    // only where this is on. (Your own OpenCode install rode on it too until
    // it went, 11-20.)
    FlagMeta {
        key: "openCodeGo",
        default: false,
        dev_default: false,
        label: "New Agent: OpenCode Go",
        description: "New Agent on one of your computers offers OpenCode Go as the sign-in for \
                      Mafold Harness. The Go key is pasted once in Settings ▸ Connections and \
                      opened by your computer for each reply. The api takes createBot with \
                      OpenCode Go only where this is on for the owner.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // Mafold Harness on Claude Code (ops 11-13, "add a mafold harness fuel
    // option: claude code"): New Agent offers Claude Code as Mafold Harness's
    // sign-in, and each model step runs on the real `claude` on that computer,
    // on the owner's Claude Code login (mafold-cli harness/claude_fuel.rs).
    // Server-read on the owner: createBot takes `sign_in: claude` with
    // Mafold Harness only where this is on.
    FlagMeta {
        key: "harnessOnClaude",
        default: false,
        dev_default: false,
        label: "New Agent: Mafold Harness on Claude Code",
        description: "New Agent on one of your computers offers Claude Code as the sign-in for \
                      Mafold Harness. The harness keeps its own prompt, tools and sessions, and \
                      each model step runs on the real Claude Code installed on that computer, on \
                      your Claude Code login there, spending your Claude plan. The api takes \
                      createBot with Mafold Harness on Claude Code only where this is on for the \
                      owner.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // Mafold Harness on Codex (ops 11-23, 「codex as fuel也补充一下(connection 和
    // 本地复用)」): New Agent offers Codex as Mafold Harness's sign-in on a
    // computer, and each model step runs on the Codex Responses API, on the
    // owner's Codex connection or that computer's own Codex login (mafold-cli
    // harness/codex_fuel.rs). Server-read on the owner: createBot takes
    // `sign_in: codex` with Mafold Harness only where this is on, and New
    // Agent's panel learns it from the api's `gates` answer — no client reads it.
    FlagMeta {
        key: "harnessOnCodex",
        default: false,
        dev_default: false,
        label: "New Agent: Mafold Harness on Codex",
        description: "New Agent on one of your computers offers Codex as the sign-in for Mafold \
                      Harness. The harness keeps its own prompt, tools and sessions, and each model \
                      step runs on your ChatGPT Codex plan: the Codex login in your Connections, \
                      else the one Codex keeps on that computer. The api takes createBot with \
                      Mafold Harness on Codex only where this is on for the owner.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // The python tool (ops 10-30 plan, "build first, start it" 11-01): one
    // Cloudflare Container per reply, network off, billed at cost in the
    // `sandbox` currency. The api reads it on the PAYER — the agent's owner —
    // so a record of `Accounts[opsdu]` opens every `opsdu:*` agent at once.
    // No screen reads it: the tool is the agent's. The daemon does, as the
    // bot (its owner's record covers it), to tell its agent about `mafold
    // sandbox` — an agent told of a tool it can't use goes and tries it.
    FlagMeta {
        key: "pySandbox",
        default: false,
        dev_default: false,
        label: "Agents: run Python",
        description: "Your agents can run Python on the files in a chat: each reply gets its own \
                      sandbox (1 vCPU, 3 GiB, network off) that ends with the reply, and the files \
                      it writes come back on the reply. It bills your wallet at Cloudflare's price \
                      in the sandbox currency. The api runs code (sandboxRun) only for agents whose \
                      owner has this on, and for official bots (@claude, @gemini) in turns of yours \
                      — you pay for those, as for their tokens.",
        owner: "ops",
        added: "2026-11",
        clients: &[Daemon],
        server: ClosedWithoutRecord,
    },
    // Files on Moments posts + Link only (ops 11-04, "Moments with files"):
    // share files from a chat by link. The api reads it on the AUTHOR who
    // writes (`moments/postUpsert` refuses files and Link only without it).
    // Reading needs no flag: a link has to open for people with no account.
    // Web and phone read it to offer the file row, Link only and Forward ▸
    // Moments.
    FlagMeta {
        key: "momentFiles",
        default: false,
        dev_default: false,
        label: "Moments: files and Link only",
        description: "A post can carry files, and can be Link only: off your profile, timelines, \
                      notifications, the sitemap and search, at a random URL that can expire. \
                      Forward ▸ Moments opens the composer with a chat's files on it, set to \
                      Link only. Readers never see a file's own address, so hiding or deleting \
                      the post cuts its files off too. An agent can write a post as a draft card, \
                      and the person it's for publishes it with a tap, under their own name.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    FlagMeta {
        key: "momentsFriends",
        default: false,
        dev_default: false,
        label: "Moments: friends only",
        description: "A post can be Friends only: your friends see it on their timeline and \
                      your profile, nobody else does, and it is never on the web, the sitemap or \
                      search. A friend is, for now, someone you have each sent a message to in \
                      your direct chat (your contacts). Parked (linsky + ops 11-20): who counts \
                      as a friend isn't settled, so it stays off and no client offers it.",
        owner: "linsky",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // Moments stay public; what an author gets is one pinned post and a say in
    // how far back everyone else sees their posts (linsky + ops 11-20 「先公开，
    // 加 n 天内可见」). Split from momentsFriends so these open on their own.
    FlagMeta {
        key: "momentsWindow",
        default: false,
        dev_default: false,
        label: "Moments: a pinned post, and how far back people can see",
        description: "You can pin one post to the top of your profile, and limit what others \
                      see of your posts to the last three days, month or half year: older ones \
                      leave timelines, your profile, their links, the web and the sitemap for \
                      everyone but you. A pinned post stays visible however old it is.",
        owner: "linsky",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // Document covers (ops 11-03, "server renders"): a file bubble shows the
    // document's first page instead of its type glyph. The api reads it on
    // the READER who asks (`resolveCovers`) — it pays in Cloudflare browser
    // time, once per document, for everyone after. Web and phone read it to
    // ask at all.
    FlagMeta {
        key: "fileCovers",
        default: false,
        dev_default: false,
        label: "Chat: document covers",
        description: "A file in a chat shows its first page — an HTML or Markdown file's first \
                      screen, a PDF's first page, a deck's first slide — full width when it is \
                      the message's only file, as a square picture beside each name when there \
                      are several. The server draws a document the first time anyone with this \
                      on asks (resolveCovers) and keeps the picture on the file for every reader.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // Passes (ops 10-30, "build it now"): credit only its holder can spend.
    // The api reads it on the SUBSCRIBER: where it's on, a paid Premium
    // period lands as a Premium Pass instead of ordinary, sendable credit
    // (a subscription that already minted ordinary credit keeps doing so).
    // Web and phone read it to show the Passes card, the Pass sheet and the
    // "Passes can't be sent" lines. Spending a Pass needs no flag: no Pass
    // exists until this or `walletPassIssue` makes one.
    // Mafold Pro Pass (ops 11-01): $20 a month for $30 of Claude, a Pass that
    // resets each renewal. Who may buy it — the team first (ops: "only team
    // members use it" while it runs on our Claude subscription).
    FlagMeta {
        key: "proPass",
        default: false,
        dev_default: false,
        label: "Wallet: Mafold Pro Pass",
        description: "Sells the Mafold Pro Pass: $20 a month, $30 of Claude in a Pass only you can \
                      spend, reset each renewal. The wallet's pill and the Pass sheet offer it, and \
                      the api opens its checkout only where this is on.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // The launch event (ops 11-01: "buy Mafold Pro Pass, gift a Mafold
    // Premium"): Premium free while a Pro Pass renews. Off = the gift ends
    // for everyone at once; no end date yet.
    FlagMeta {
        key: "proPassGift",
        default: false,
        dev_default: false,
        label: "Pro Pass gives Premium",
        description: "While someone's Mafold Pro Pass renews, they are a Premium member too. Read \
                      on the subscriber.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    FlagMeta {
        key: "passes",
        default: false,
        dev_default: false,
        label: "Wallet: Passes",
        description: "Premium's monthly credit lands as a Premium Pass: only you can spend it, on \
                      @claude, @gemini, @chatgpt, your own agents, images and video — not on other \
                      people's paid agents. It can't be sent, converted or put in a red packet, and \
                      it resets when the next month's Pass lands. The wallet shows Passes above \
                      Holdings. The api mints Premium as a Pass only where this is on for the \
                      subscriber.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // One balance in dollars (ops 11-10: "废除token经济学 以后用wallet单一个美金即可",
    // then "transfer 换成一个 redeem 否则有审查风险" and Subscriptions between
    // Redeem and Details). The wallet page reads in dollars and loses what
    // only a balance in many models needed, on the web and the phone alike.
    FlagMeta {
        key: "usdWallet",
        default: false,
        dev_default: false,
        label: "Wallet: one balance in dollars",
        description: "The wallet is one balance in US dollars: no provider pills, Holdings, Convert \
                      or Auto convert, and every Activity row in dollars with its model under it. \
                      The buttons are Deposit, Redeem, Subscriptions and Details; Transfer goes. \
                      Redeem takes a code from Mafold. The api redeems codes only where this is on.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // Mafold Mail phase 1 (ops 10-30): `username@mafold.com`, receive only.
    // The api gates every part of it on the MAILBOX's handle — the rcpt hook,
    // the inbound hook, the mail* methods, @mail's commands — so a record of
    // `Accounts[opsdu]` opens opsdu and every `opsdu:*` agent at once. The
    // daemon reads it too: an agent it is on for gets the `mafold mail` line
    // in its tool list. Web and phone don't — the mail lands as cards, and a
    // card can't read flags.
    FlagMeta {
        key: "mailbox",
        default: false,
        dev_default: false,
        label: "Mail: username@mafold.com",
        description: "Mail sent to your handle at mafold.com (an agent's is owner.label@mafold.com, \
                      +tag allowed) is accepted and lands from @mail as a card in your chat with it; \
                      an agent's mail also lands in its owner's chat with @mail. Receive only. Off = \
                      the address is refused at the door and the mail methods answer not available.",
        owner: "ops",
        added: "2026-10",
        clients: &[Daemon],
        server: ClosedWithoutRecord,
    },
    // The profile opens out of the chat's own top pill (ops 10-31): the pill
    // grows down to the composer's bottom at the composer's width, its back
    // button (or, with none, its avatar) turns into the ✕, and the page is the
    // plain page colour. New Message / Group / Agent open and close in under
    // 250ms with it — on phones in-app, no longer the system's slide-up sheet.
    // Behind this until both are live; client-only.
    FlagMeta {
        key: "pillOpen",
        default: false,
        dev_default: true,
        label: "Profile opens from the top pill",
        description: "Tapping a chat's top pill grows the pill itself into the profile (top pill's \
                      top → composer's bottom, composer's width; a phone's shrunk composer counts \
                      at full size), on the page colour; the back button — or the avatar where \
                      there is none — becomes ✕. 200ms open / 180ms close. New Message / Group / \
                      Agent: 200ms / 180ms genie on every client, in-app on phones (back gesture, \
                      keyboard, safe areas). Off = the 620/600ms genie and the phone's system sheet.",
        owner: "ops",
        added: "2026-10",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // ops 11-03: on the desktop app a link opens as a tab on the app page. Needs
    // `appBeside` (the page) and a shell with `site-tabs`; anywhere else it does
    // nothing. Client-only (mafold-web lib/siteLinks.ts).
    FlagMeta {
        key: "inAppBrowser",
        default: false,
        dev_default: true,
        label: "Links open inside the desktop app",
        description: "On the desktop app, a link opens as a tab on the app page instead of in the \
                      default browser: a site tab next to the apps, with back, forward, reload and \
                      an address under the strip, and an address box in its +. ⌘/Ctrl-click, the ↗ \
                      on the tab, or a Settings switch still go to the default browser. Needs a \
                      shell that offers site tabs (older ones keep the default browser). Web in a \
                      browser and the phones: unchanged. Off = every link goes to the default \
                      browser.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web],
        server: NotRead,
    },
    // ops 11-14: an agent uses a site tab on the desktop app, with its cursor
    // on the site and real clicks (mafold-use). The `mafold/browse` card asks;
    // the shell's own dialog lets it in (cap `site-agent`). Needs `inAppBrowser`.
    // Web reads it (mafold-web lib/agentTabs.ts); the api reads it for whether
    // an agent is offered the card (the manifest's `agent.flag`).
    FlagMeta {
        key: "agentTabs",
        default: false,
        dev_default: true,
        label: "Agents can use a tab in the desktop app",
        description: "On the desktop app, an agent's mafold/browse card can open a site tab and, once \
                      you allow it in the app's own dialog, look at the site and click, type and \
                      scroll in it, with its cursor drawn on the site and a Stop bar over it. Never \
                      a password field, never another tab. Needs links in the app (inAppBrowser) and \
                      a shell that offers it. Agents are told about the card only where this is on \
                      for their owner (listCards `offered`). Off = the card says it can't open a \
                      tab here, and agents aren't offered it.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web],
        server: ClosedWithoutRecord,
    },
    // ops 11-17 (11-07 design): Settings ▸ Connections ▸ Browsers — your Chrome
    // and Safari profiles' logins brought into the desktop app's browser, one
    // session per profile; the `chrome` / `safari` registry rows. Server-read:
    // putConnection takes those rows only where this is on.
    FlagMeta {
        key: "browserProfiles",
        default: false,
        dev_default: true,
        label: "Connections: your Chrome and Safari profiles",
        description: "Settings ▸ Connections gets a Browsers tab with Chrome and Safari. In the \
                      desktop app on a Mac it lists the profiles on that computer; Import brings \
                      one's logins (cookies, as they are now) into Mafold's browser, in a session \
                      of that profile's own, after one Keychain prompt (Chrome) or Full Disk \
                      Access (Safari). The logins stay on that computer; the connection row only \
                      names the profile. Site tabs can then open in that profile. Off = no \
                      Browsers tab, and the api refuses those rows.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // ops 11-16: the page of tabs gets the window's three lights, flat (red
    // closes, yellow minimizes into a "N tabs" pill in the folders row, green
    // fills the window); a page with no chat open takes the pane; dragging the
    // chat | page line far left offers "Release to collapse". Phones: links open
    // in a full-screen browser with one small pill at the bottom. Client-only
    // (client-shared/appTabs.ts, mafold-rn lib/phoneBrowser.ts).
    FlagMeta {
        key: "browserPill",
        default: false,
        dev_default: true,
        label: "Tabs page: lights, minimize, phone browser",
        description: "Desktop: the page of tabs (apps and sites) starts with the window's three \
                      lights, flat — red closes it, yellow minimizes it with its tabs still \
                      running into a \"N tabs\" pill left of + in the chat list's folders row, \
                      green fills the window. Clicking the pill lists the minimized tabs; picking \
                      one brings the page back beside the open chat, or over the whole right side \
                      when no chat is open. Dragging the chat | page line far left shows \
                      \"Release to collapse\": letting go closes the chat and the page takes the \
                      right side. Phones: a link opens in a full-screen browser inside the app \
                      with one small address pill at the bottom; tapping it shows ✕ and ⋯, and ✕ \
                      asks Close or Minimize. Off = ✕ and ⤢ discs, and links leave the phone \
                      app for the system browser.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // ops 11-02: opening an app shifts the columns one place left instead of
    // asking Full or Split. Client-only (client-shared/appBeside.ts).
    FlagMeta {
        key: "appBeside",
        default: false,
        dev_default: true,
        label: "Apps open beside the chat",
        description: "On a window with room for two columns (web, the desktop app, tablets in \
                      landscape), tapping an app opens it straight away — no Full / Split \
                      question: the chat list folds, the chat moves into the list's slot at the \
                      list's width (never under 360), and the app slides in from the right and \
                      takes the rest. The chat | app line drags; the list comes back as a third \
                      column, or over the chat when there's no room; ✕ puts everything back, ⤢ \
                      still fills the window. From the Apps tab (no chat) the app takes the big \
                      pane and the list stays. Phones and narrow windows: unchanged. Off = the \
                      Full / Split chooser and the 408 px column.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    FlagMeta {
        key: "richComposer",
        default: false,
        dev_default: true,
        label: "Rich text in the message box",
        description: "Once the words in the message box reach a third line, a full-screen button \
                      appears at its top right. It opens a real editor: headings, tables, lists, \
                      quotes, code and links, shown as they will look sent, with a pill of tools \
                      along the bottom. Settings ▸ General can show formatting in the box itself \
                      too. The message is still markdown, so nothing changes for the api, other \
                      clients or agents. Off = the plain text box, no button.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    FlagMeta {
        key: "siteSearchConsole",
        default: false,
        dev_default: false,
        label: "Search Console on hosted sites",
        description: "Each site in Apps ▸ Manage gets a Google Search Console field: paste the \
                      name of the verification file and the site answers it, through every \
                      deploy. Needs the sites Worker that answers it (sites@) before anyone \
                      can save. Off = no field, and the api refuses to save one.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // ops 11-09 「The page, Visitors, Domains, Show on Google」: a site's page in
    // the drawer gets three sections, each opened on its own once what it
    // stands on is out — the sites Worker that counts views and answers the
    // files (sites@), the zone's custom-hostname setup, Mafold's Search
    // Console. No client reads these: listSites says which sections a person
    // has (`features`), so a page never draws one the api would refuse.
    FlagMeta {
        key: "siteStats",
        default: false,
        dev_default: false,
        label: "Visitors on hosted sites",
        description: "A site's page shows its views and visitors for the last 7 or 30 days and \
                      where they came from, counted by the sites Worker without cookies. Needs \
                      that Worker (sites@) before there is anything to count. Off = no Visitors \
                      section, and getSiteStats refuses.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    FlagMeta {
        key: "siteDomains",
        default: false,
        dev_default: false,
        label: "Own domains on hosted sites",
        description: "A site's owner can connect a domain of their own with one CNAME record; \
                      once Cloudflare has it live with HTTPS it becomes the site's main address. \
                      Needs the sites zone's custom hostnames (fallback origin, the Worker's \
                      catch-all route) and CF_SITES_ZONE_ID on the api. Off = no Domains \
                      section, and the api refuses to add one.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    FlagMeta {
        key: "siteNames",
        default: false,
        dev_default: false,
        label: "Rename a hosted site's free address",
        description: "A site's owner renames its address in Domains (`skyclock-8732.mafold.app` \
                      → `skyclock.mafold.app`); the old address stops answering. Needs the \
                      sites Worker that reads renames (sites@0.1.3) first. Off = no pencil on \
                      the address, and setSiteName refuses.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    FlagMeta {
        key: "siteSearch",
        default: false,
        dev_default: false,
        label: "Show on Google for hosted sites",
        description: "One switch per site, off by default. On, the site answers a robots.txt \
                      and a sitemap.xml unless it ships its own, and every deploy sends its \
                      pages to IndexNow (Bing, Yandex, Naver…) and its sitemap to Google through \
                      Mafold's Search Console (GOOGLE_SEARCH_KEY). Needs the sites Worker that \
                      answers the files (sites@). Off = no switch, and the api refuses to turn \
                      one on.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // ops 11-15 「Show it to new users」 then 「用icon」: a newcomer's Spaces show an
    // example site (Skyclock) beside New space, until they have one of their
    // own. Which site is the example is a setting on the server
    // (`setSiteExample`); this decides who is shown it. listSites says, so no
    // client reads it.
    // Profile covers (ops 11-19, the cover demo's round 3 and then "Build it",
    // "Count: People and agents"): a person's or an agent's profile with no
    // picture shows the default cover — the homepage's ground and "I'm
    // Mafold's Nth user" — and its owner gets Personalize: pick an agent, which
    // designs a cover and sends it back as a draft card; a tap puts it on.
    // The default cover is a published card (mafold/cover-joined) the server
    // puts on every account with nothing of its own, whatever the flag; web and
    // phone read the flag to draw account covers at all; the api reads it on
    // the person who taps a draft (`profile_cover`), and an agent is told about
    // the draft card only while it's on for whoever the turn answers to.
    FlagMeta {
        key: "profileCover",
        default: false,
        dev_default: false,
        label: "Profile covers: default + Personalize",
        description: "A profile with no picture shows a cover of its own: the homepage's ground \
                      and \"I'm Mafold's Nth user\" (people and agents in one count). Its owner \
                      gets Personalize: pick one of their agents or an official one, which designs \
                      a cover and sends it back as a draft card; Use as cover puts it on the \
                      profile. Visitors see Make yours, or who designed it.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    FlagMeta {
        key: "spaceExample",
        default: false,
        dev_default: false,
        label: "Example space for newcomers",
        description: "Someone with no site of their own (nor their agents') sees the example \
                      sites in their Spaces, marked Example, beside New space; its page offers \
                      Make your own. Which sites are examples is set with setSiteExample. \
                      Read by listSites; no client reads it. Off = no example.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // ops 11-06: 「给我抬一下限制」. garden-beta is opsdu's 18th site, past the five
    // Premium publishes, so every garden@ was refused. Per person, so the plan
    // stays what it is for everyone else.
    FlagMeta {
        key: "sitesUncapped",
        default: false,
        dev_default: false,
        label: "No site cap",
        description: "The people on this list can publish every site they and their agents \
                      have, past the plan's cap (1 free, 5 with Premium). Read by the sites \
                      api; no client reads it. Off = the plan's cap.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // ops 11-09 (DM #Username rename + NoBrain harness): an agent's username
    // changes once a day, a person's once a month, the old one held 30 days;
    // a person's change moves everything ("Everything moves"). Behind this
    // until ops has used it; the api refuses `renameAgent` / `renameMe`
    // without a record.
    FlagMeta {
        key: "usernameChange",
        default: false,
        dev_default: true,
        label: "Change a username",
        description: "Customize ▸ Username has Change: an agent's username changes at most \
                      once a day. Settings ▸ Username does the same for a person, once a \
                      month, and everything moves with it: their agents, apps and cards \
                      (republished under the new name), sites, wallet, chats. For 30 days the \
                      old names and ids still lead there — old @mentions, links, mail, cards \
                      in old messages — then they are free (an old app or card id never is). \
                      Off = usernames are read-only and the api refuses renameAgent and renameMe.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // NoBrain (ops 11-09, same DM): New Agent makes the agent at once and it
    // sets itself up in its own chat. Behind this until ops has used it; the
    // api refuses newAgent and the two cards' taps without a record.
    FlagMeta {
        key: "noBrain",
        default: false,
        dev_default: true,
        label: "New Agent: NoBrain",
        description: "New Message's ✦ makes the agent at once — @you:new-ai-with-nobrain, no \
                      brain — or opens the one you have that still has that username, and it sets \
                      itself up in its own chat: avatar, name and username (New Agent's ①), then \
                      where its brain runs (② and ③), as two cards. Typing to it before it has a \
                      brain stays a message and gets a fixed answer. Off = New Agent's panel.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // An html card's full screen belongs to its page (ops 11-10, demo rounds
    // 2–3 in their DM, "default is off"): the app's button leaves the card, and
    // a page that wants full screen calls `mafold.fullscreen.enter()` from a
    // tap. The calls work with this off too; it only takes the app's button
    // away, so every card already sent loses its way into full screen.
    FlagMeta {
        key: "htmlPageFullscreen",
        default: false,
        dev_default: false,
        label: "HTML cards: the page owns full screen",
        description: "An html card in chat has no full-screen button of the app's own: a page \
                      that wants full screen draws its own button and calls \
                      mafold.fullscreen.enter() from a tap. In full screen the app's close button \
                      sits in a strip above the page instead of on it. Off = the app's button on \
                      the card's top-right corner, and on the page in full screen.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // The agent-turn cards in MDS (ops 11-10, "then start build it behind
    // flags" on the Today/New demo in their DM). Cards publish on merge and read
    // no flags, so the host says it instead: on, a card's theme carries
    // `design: "mds"` (cards/sdk `CardTheme`) and the cards that have an MDS
    // look draw it. Off = every card exactly as today.
    FlagMeta {
        key: "cardsMds",
        default: false,
        dev_default: false,
        label: "Cards: the agent cards in MDS",
        description: "The cards an agent's reply is made of draw in MDS: turn summaries, step \
                      groups and background tasks become rows with their steps on a thin line \
                      instead of boxes, a step's short result sits at the end of its row, output \
                      and edits sit in a grey block, questions get raised answers with radio \
                      buttons, and the cost row is plain text. Off = the cards as they were.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // How a Modal comes and goes, after a Telegram for Mac clip (ops 11-11,
    // "Web and Mac" on the demo in their DM): the pressed thing becomes the
    // sheet and back. Off = the line off the pressed edge (#1519). The phone's
    // sheets keep the system's fade either way.
    FlagMeta {
        key: "modalMorph",
        default: false,
        dev_default: false,
        label: "Modals: the pressed thing becomes the sheet",
        description: "A sheet opens out of what you pressed: one box grows from it to the sheet \
                      on a spring with no bounce, wearing its look for the first moments and the \
                      sheet's after that, and the sheet's contents are drawn at their own layout \
                      scaled to the box. Its place stays empty while the sheet is up, and closing \
                      shrinks the sheet back into it. Off = a line off the pressed edge that flies \
                      to the sheet.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web],
        server: NotRead,
    },
    // The chat header's top-right corner as one ☰ (ops 11-11, the html demo in
    // their DM's #右上角汉堡菜单, answered "as shown" on all three questions):
    // the button grows into a solid panel that holds the chat's apps and what
    // its profile opens. Web first; the phone's twin comes with its own PR.
    FlagMeta {
        key: "headerMenu",
        default: false,
        dev_default: false,
        label: "Chat header: one ☰ for the apps and the chat's pages",
        description: "The chat's top-right corner is one ☰ at every width. Tapped, it grows into a \
                      solid panel: search in this chat, every app here plus Add, Media · Files · \
                      Pinned · Channels, the rows for that kind of chat (an agent's Customize and \
                      working directory, a group's members, invite links and permissions, a \
                      person's moments), mute, a new window, and clear history, leave or block. \
                      Off = the app icons and the ⊞ that grows into the app list.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web],
        server: NotRead,
    },
    // The phone's twin of headerMenu (ops 11-12, "☰ panel" on the card about
    // the design work that never reached the app). Its own key because
    // headerMenu is on for everyone already: a phone release would otherwise
    // hand it to every phone at once, before ops has used it on theirs. Both
    // go together once it's on for everyone here too.
    FlagMeta {
        key: "headerMenuPhone",
        default: false,
        dev_default: false,
        label: "Chat header on the phone: one ☰ for the apps and the chat's pages",
        description: "The phone's chat header gets web's ☰ in its top-right corner, in every chat. \
                      Tapped, it grows into a solid panel: search in this chat, every app here plus \
                      Add, Media · Files · Pinned · Channels, the rows for that kind of chat (an \
                      agent's Customize and working directory, a group's members, a person's \
                      moments), mute, and clear history, leave or block. Off = the app launcher, \
                      shown only in chats with apps.",
        owner: "ops",
        added: "2026-11",
        clients: &[Phone],
        server: NotRead,
    },
    // The phone's twin of activityDrawer (ops 11-12, "M drawer: Build it as
    // shown" on the web drawer drawn at phone width). ops then chose "To
    // everyone" for each phone item: this goes on for every phone once the
    // app.js@ that carries it is out. Its own key so the phone's drawer can be
    // turned off without taking the web's with it.
    FlagMeta {
        key: "activityDrawerPhone",
        default: false,
        dev_default: false,
        label: "Activity on the phone: one drawer for Garden and the Activity Center",
        description: "Tapping the M slides one panel out of the screen's left edge, from under the \
                      status bar to the tab bar's foot and as wide as the chat list up to its \
                      new-chat button, with Now, Agents, Works and Tasks. The M rolls into its \
                      bottom-right corner and the tab bar steps aside while it's open. Off = the \
                      Activity Center that grows out of the M.",
        owner: "ops",
        added: "2026-11",
        clients: &[Phone],
        server: NotRead,
    },
    // Moments rows fold long posts the way X does (ops 11-13, "Fold + short
    // leads": a 1,306px @gemini post was 1.5 screens of a phone). The rule is
    // client-shared/momentFold.ts. On for everyone 11-13 (audit #167), then
    // the gate came out: every row folds, the public mafold.com/@ pages too
    // (they have no signed-in reader to ask). Web up to 0.1.230 and app.js up
    // to 71 still read the key, so its record stays on.
    FlagMeta {
        key: "momentFold",
        default: false,
        dev_default: false,
        label: "Moments: fold long posts in the feed",
        description: "Retired: every feed row folds now, its words up to 280 characters (140 \
                      Chinese), then \"… Show more\", and one box under them with its first \
                      picture, clip or card. Only web up to 0.1.230 and app.js up to 71 read \
                      this key: keep it on so they fold too. Off = those show every post in full.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // Stickers rounded out (ops 11-13 20:30, demo OK'd 12:45Z): "Add to my
    // stickers" on any sticker in a chat (linsky 11-06), a bar over the panel
    // (search + your sets as tabs), and Settings > Stickers to manage sets.
    // One shared page on both apps: client-shared/stickerPanel.tsx. The set
    // routes answer everyone — they only touch the caller's own library.
    FlagMeta {
        key: "stickerSets",
        default: false,
        dev_default: false,
        label: "Stickers: sets, search and Add to my stickers",
        description: "Right-click (long-press on the phone) a sticker in a chat to add it to \
                      your stickers. The sticker panel gets a bar: search, then a tab per set. \
                      Settings > Stickers lists your sets: reorder them, rename, delete, move \
                      stickers between sets (on a computer, drag them onto a set), add and \
                      remove single stickers. Off = the plain grid.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // GIF search in the sticker panel (ops 11-14: 「搜索功能接一下Google的那个gif
    //工具」 → Tenor is gone → KLIPY). The search box says "Search KLIPY" and its
    // answers show as their own section under your stickers; a tapped GIF is
    // kept on our CDN (gif.keep) and sent as a sticker. Needs stickerSets (the
    // panel with the bar). The two routes answer everyone: a search costs a
    // cached KLIPY call, a keep is an ingest like any upload.
    FlagMeta {
        key: "stickerGifs",
        default: false,
        dev_default: false,
        label: "Stickers: GIF search (KLIPY)",
        description: "The sticker panel's search box says \"Search KLIPY\" and also finds GIFs: \
                      they show under your own stickers, in KLIPY's order. Tap one to send it; \
                      it is copied to Mafold's CDN first, so it stays in the chat and can be \
                      added to your stickers. Only with stickerSets on. Off = the search finds \
                      your stickers only.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // Drag a sticker to a new place in the composer sheet's sticker panel (ops
    // 11-14: 「这个stickers的composer sheet 我要能拖动表情包来调整位置」, demo
    // OK'd "Build it"). Client only: it moves stickers with sticker.move, the
    // call Settings ▸ Stickers already makes. Needs stickerSets (the panel).
    FlagMeta {
        key: "stickerDrag",
        default: false,
        dev_default: false,
        label: "Stickers: drag to reorder in the sticker panel",
        description: "In the composer's sticker panel, drag a sticker to a new place: within its \
                      set, into another set's section, or onto a set's tab (it goes first there). \
                      On a computer it lifts once the pointer moves; on the phone, hold it first. \
                      A tap still sends. Only with stickerSets on. Off = tap to send only.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // New Folder as rules, Telegram-style (ops 11-19 「优化一下chat folder的新建
    //机制」, demos OK'd "Build both"): chat types in, muted/read and chats by
    // name out, two panes on a computer, a picker page on a phone. Client
    // only: saveFolder keeps the rules whoever saves, and every client works
    // a folder's chats out of them flag or no flag — this gates the editor.
    FlagMeta {
        key: "folderRules",
        default: false,
        dev_default: false,
        label: "Chat folders: include chat types, exclude muted/read",
        description: "New Folder and Edit Folder take chat types (People, Agents, Groups) as well as \
                      chats, and can exclude Muted or Read chats and chats by name; a group joined \
                      later lands in a Groups folder by itself. Two panes on a computer, a picker \
                      page on a phone. Off = the folder is a list of chats you tick.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // Clear history can also clear the other person's side of a one-to-one
    // chat (ops 11-15: 「根本无法delete all」, after an agent kept answering
    // from a chat he had cleared; "Build it"). Client only: the dialog gains
    // "Also delete for @name" and clears through clearHistory, which covers
    // the whole chat instead of the messages the app had loaded.
    FlagMeta {
        key: "clearForBoth",
        default: false,
        dev_default: false,
        label: "Clear history: also delete for the other person",
        description: "In a one-to-one chat, Clear conversation history asks whether to also delete \
                      for the other person. On: the words are taken back for both and neither side \
                      keeps the rows. Either way the whole chat is cleared on the server, not just \
                      what the app had loaded. Off = clears only the loaded messages, for you.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // $1 in every new wallet (ops 11-13 「1$ in every new users wallet」, then
    // "No limit"): minted straight into the wallet at sign-up, no packet to
    // open. Read for the account being registered, so it is turned on for
    // everyone, not per person.
    FlagMeta {
        key: "welcomeDollar",
        default: false,
        dev_default: false,
        label: "$1 in every new wallet",
        description: "A new account starts with $1 in its wallet, minted at sign-up at the \
                      official price, the way an invite's $6 is paid. No limit per device. \
                      Off = a new wallet starts empty.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // The onboarding guide greets newcomers (ops 11-13 「这个变成一个mafold
    // harness的一个plugin 这个onboarding agent会启用」, account @zhenchendu):
    // a new account whose flag is on gets its welcome DM from the guide, who
    // speaks first, instead of @mafold's three fixed messages. Read at sign-up
    // for the account being made, so a rollout is On for everyone, or a list
    // of test accounts on dev.
    FlagMeta {
        key: "onboardingClerk",
        default: false,
        dev_default: false,
        label: "Newcomers greeted by the onboarding guide",
        description: "A new account's welcome DM is with the onboarding guide (@zhenchendu), \
                      who opens it in its own words on the onboarding plugin's handbook. Off = \
                      @mafold's steward line, card and choice card, as before. On for an \
                      account, its apps don't open the Activity Center by themselves on a first \
                      visit, and the web opens the guide's chat instead.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // Sign in with Instagram by DM code (ops 11-17 「想要一个有login with
    // instagram 会感觉更年轻一点」, "Build it"): the login page offers
    // "Continue with Instagram" and Settings ▸ Privacy and Security lists an
    // Instagram row. The signed-out door reads the flag's verdict for no
    // account, so it opens there only with a rollout of On for everyone.
    FlagMeta {
        key: "instagramLogin",
        default: false,
        dev_default: false,
        label: "Sign in with Instagram (DM a code)",
        description: "The login page offers Continue with Instagram: you send a code to \
                      @getmafold on Instagram and you're signed in, or a new account is made from \
                      your Instagram name and picture. Settings ▸ Privacy and Security lists \
                      Instagram, to link it to an account you already have. Off = neither \
                      shows; accounts already linked can still sign in and unlink.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // Staged attachments sync across your devices (ops 11-16: 「如果有
    // attachments 比如有图片的时候 草稿无法多端同步」, "Build it, 25 MB cap").
    // Client only: a picked file uploads as it is staged and saveDraft lists it
    // (the server keeps the list whether or not this is on).
    FlagMeta {
        key: "draftFiles",
        default: false,
        dev_default: false,
        label: "Drafts: staged files sync across your devices",
        description: "A photo or file staged on the input bar uploads right away and shows on \
                      every device you're signed in on, ready to send from any of them. Files \
                      over 25 MB stay on the device that picked them; the others show them as \
                      being on another device. Off = staged files stay on the device that picked \
                      them and upload when you send.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: NotRead,
    },
    // The send flight on the phone (ops 11-19 picked "Telegram iOS" on the demo
    // card, then on web's preview: 「这个可以 你可以merge了 然后把appjs的部分也做
    // 了」). Web ships it on; the phone half moves native layout on the UI thread
    // and hasn't been seen on a device yet, so it starts behind this. Client only.
    FlagMeta {
        key: "sendFlightPhone",
        default: false,
        dev_default: false,
        label: "Phone: Telegram's send motion",
        description: "In the phone app, a message you send grows out of the text field, swings \
                      right, then rises into place in 0.3 s, while the messages above make room \
                      on the same curve; the send button presses down while held. The web app \
                      already does this. Off = the bubble just appears.",
        owner: "ops",
        added: "2026-11",
        clients: &[Phone],
        server: NotRead,
    },
    // Appearance stays on the device; a chat gets its own (ops 11-20: "appearance的
    // 选项 全部保留本地 … 「使用其他设备的壁纸」 需要其他设备在线 … 新增群组内的
    // appearance选项(wallpaper和color)", then "照这个做" on the demo). The
    // wallpaper stops syncing with or without it; this gates the two new ways
    // in and the endpoints behind them (askDeviceWallpapers, setChatAppearance).
    FlagMeta {
        key: "chatAppearance",
        default: false,
        dev_default: false,
        label: "Chat appearance + another device's wallpaper",
        description: "A group or direct chat's profile gets Chat appearance: a wallpaper and a \
                      colour that everyone in it sees over their own, set by whoever may edit \
                      the group's info (either side of a direct chat), with a line in the chat \
                      saying who. Appearance ▸ Chat Wallpaper gets \"Use another device's \
                      wallpaper\", which asks your other open devices for theirs. Off = neither.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // The onboarding modal (ops 11-20: 「start tutorial的modal弹窗 这个是不太合适的
    // 撤掉 … 我需要一个平滑的过渡」, A + 「我需要这个时候有一个大modal」, then the
    // content: 「必须的两步是: (1) 设置我的display name 还有头像 (2) 必须创建一个agent」,
    // "Build it"). Read at the door, with the new account's token, before
    // Continue to Mafold.
    // For everyone (ops 11-23 「现在准备给新版onboarding开全量 … 如果没有看过
    // onboardingModal的 都需要体验一次 … @ mafold … 退化原本的model能力 对于之前
    // 有chat过的 都需要补充一个通知」, then 「@ mafold 不发欢迎内容」): the account's
    // `onboarding.seen` mark (`tutorial.rs`) is what "seen" means, and the api
    // reads the flag for @mafold's part (`brains/steward.rs`, `seed_onboarding`).
    FlagMeta {
        key: "onboardingModal",
        default: false,
        dev_default: false,
        label: "Onboarding modal, once per account",
        description: "An account that has never been through the onboarding modal gets it \
                      once: at sign-up Continue to Mafold grows the door's card into it, and \
                      an account that was already here meets it the next time it opens the \
                      app. Welcome, your name / username / photo (required), what you use AI \
                      for, make your own AI on Mafold Cloud (required; left out for someone \
                      who has one), say hi to it, and who to start with. For the same \
                      accounts @mafold has no model: a new account gets no welcome from it, \
                      a message to it is answered with one line and the person's own AI, and \
                      someone who chatted with it before is told once, when the modal \
                      closes. Off = no modal, and @mafold welcomes and answers as before. \
                      ?onboarding opens the modal as a dry run on any account.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // ops 11-21 「personal agent的接入体验 现在太过粗糙」 + 「知识库可以共享」:
    // an AI you already use (Muse, ChatGPT…) connects through a link and your
    // Approve, and reads the same Mafold guide the other agents get.
    FlagMeta {
        key: "aiConnect",
        default: false,
        dev_default: false,
        label: "Agents: connect an AI you already use",
        description: "Picking API token on an agent's brain card gives a one-sentence link \
                      for the AI you already use instead of a raw key: the AI reads the page \
                      (who it will be, the calls, how to write cards), asks to connect, you tap \
                      Approve on the card, and its next call collects a key made for it alone. \
                      Off = the card points at the token in Customize ▸ Settings, as before.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // ops 11-23 「这个初识消息太差劲了 zhenchendu的这个样式是不错的 尤其下面的气泡组
    // 要复用」, 「Make me an Agent of my own 这个换成Try Different Model」, 「如果用户
    // 通过气泡组来回复了这个HTML, 需要在回复完以后 最后补发一条新的气泡组」, then
    // 「Build it」 on the demo.
    FlagMeta {
        key: "agentStarters",
        default: false,
        dev_default: false,
        label: "New agent: starter bubbles",
        description: "A new Mafold Cloud agent opens its DM with a short hello and a group of \
                      starter bubbles (Try a different model, A new avatar, Make me a toy, Meet \
                      the Agents on Mafold, New colours, What is Mafold?, How did Mafold begin?, \
                      Why Mafold?). A tap sends that line as your reply to the group; the agent \
                      answers it, and a fresh group of the ones not asked yet follows the answer. \
                      Read for the OWNER when the agent is created. Off = the old ready message.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // ops 11-23 「Mafold Cloud的harness的pays with 需要[ mafold wallet ] [ API Key ]
    // [ pick from connections ] apikey需要支持anthropic 和 openai两种格式」, then
    // 「anthropic format 也需要base url」 + "Hand over once".
    FlagMeta {
        key: "cloudOwnKey",
        default: false,
        dev_default: false,
        label: "Mafold Cloud: pay with your own model key",
        description: "Pays with on a Mafold Cloud agent offers Mafold wallet, API key and \
                      Connections. An API key is Anthropic or OpenAI format with its Base URL; \
                      Check key lists the models it can use. A key from Connections is handed \
                      to Mafold Cloud once, by one of your devices that's on, when you pick it. \
                      The key is sealed on the server and replies are billed on it, not the \
                      wallet. The api answers listHarnessHosts with cloud_keys and takes \
                      checkModelKey / createBot with sign_in `key` only where this is on.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // ops 11-23 「Mafold harness要放在左边 然后mafold wallet是默认 并且也要有mafold
    // wallet 也要有mafold cloud的那个model picker 这里的Sign in with 也要Mafold
    // Cloud的同款选项 用apikey / connections」.
    FlagMeta {
        key: "ownKeyOnComputers",
        default: false,
        dev_default: false,
        label: "Computers: Mafold Harness signs in with your own model key",
        description: "Sign in with for Mafold Harness on a computer offers what Pays with offers \
                      on Mafold Cloud: an API key (Anthropic or OpenAI format, with its Base URL) \
                      or a key from Connections, checked against its own model list. A typed key \
                      is sealed on the server and opened for each reply the owner starts \
                      (openModelKey); a Connections key is opened by the computer itself. The \
                      api answers listHarnessHosts with cloud_keys and takes checkModelKey / \
                      createBot with sign_in `key` on a computer only where this is on.",
        owner: "ops",
        added: "2026-11",
        clients: &[],
        server: ClosedWithoutRecord,
    },
    // ops 11-22 「DM里面 这中间的 No messages yet / Say hello 完全可以放一个自定义的card」,
    // 「必须要用户主动设置才行 不要默认覆盖」, then 「可以开始做了」 on the demo.
    // An account's own card in an empty DM, set on purpose and never put on by
    // default. The server gates setting it and answering its taps; clients
    // gate drawing it.
    FlagMeta {
        key: "helloCard",
        default: false,
        dev_default: false,
        label: "Chats: a hello card in an empty DM",
        description: "An account (a person, or an agent through its owner) can pick one published \
                      card as its hello card. A DM with it and no messages yet draws that card \
                      where No messages yet / Say hello would be. An agent's card shows its quick \
                      replies, and its owner also sees usage and the model and folder pill. \
                      Nobody gets one by default. Off = No messages yet, as before.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // ops 11-22 「我需要做一个聊天记录分享页 仅开放DM分享」, then on the demo:
    // either person, messages I pick, 7 days, the words as shown, and kept on
    // its own (not a Moments post).
    FlagMeta {
        key: "chatShare",
        default: false,
        dev_default: false,
        label: "Direct chats: share messages as a page",
        description: "Selecting messages in a direct chat offers Share link: the picked messages \
                      become a page at mafold.com/share/…, drawn as the sharer sees the chat, \
                      that anyone with the link can open for 7 days. Off = no Share link, and \
                      the api makes no new pages (links already made still open).",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // ops 11-22 「我想把这一套体系带给Mafold Harness」 (Claude Code's summary +
    // worklog + layered memory), then on the demo: saving on its own
    // judgment, a Mac agent's memory kept on Mafold with a copy on the Mac,
    // "Build it, flag on for you first" (`.docs/harness-memory-v1.md`).
    FlagMeta {
        key: "harnessMemory",
        default: false,
        dev_default: false,
        label: "Agents: notes, worklog and summary memory",
        description: "An agent on Mafold Harness (official bots, Cloud agents, and Mafold Harness \
                      agents on a computer) keeps its memory of you as notes, one fact each, \
                      saved on its own judgment unless you switch that off; it reads an index of \
                      them every turn, and folds messages that fell out of the chat's last 200 \
                      into a summary. A computer agent's memory is kept on Mafold with a copy on \
                      that computer. The chat menu gets Memory: notes, worklog, summary and \
                      rules. Read for the person whose memory it is. Off = memory is written \
                      only when asked, computer agents keep none, and there is no Memory page.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web, Phone],
        server: ClosedWithoutRecord,
    },
    // ops 11-22 「所有卡片 包括官方的 都必须在sandbox里面运行」 (after Telegram
    // pulled its widgets): card code leaves the app's realm. Demo first, on
    // web, for poll / ask / generating / html (`.docs/card-sandbox-v1.md`).
    FlagMeta {
        key: "cardSandbox",
        default: false,
        dev_default: false,
        label: "Cards run in a sandbox",
        description: "The demo cards (poll, ask, generating, html) run in their publisher's \
                      sandbox — a hidden frame with no network and no reach into the app — \
                      and the app draws what they send. Off = they run in the page, as \
                      every card does today.",
        owner: "ops",
        added: "2026-11",
        clients: &[Web],
        server: NotRead,
    },
];

pub fn meta(key: &str) -> Option<&'static FlagMeta> {
    KNOWN_FLAGS.iter().find(|m| m.key == key)
}

/// What the api decides for `key` when it has no record: open only for a flag
/// registered as [`FlagServer::OpenWithoutRecord`]. Anything else — closed,
/// client-only, or not registered at all (an official bot's `launch_flag`
/// nobody added here) — stays shut.
pub fn server_default(key: &str) -> bool {
    meta(key).is_some_and(|m| m.server == OpenWithoutRecord)
}

/// A key the api gates on, checked at compile time: put every server-side
/// flag key in a `const` through this, and a key that isn't in
/// [`KNOWN_FLAGS`] — or is registered as client-only — fails the build.
///
/// ```ignore
/// pub const FLAG: &str = mafold_types::flags::server_key("topups");
/// ```
pub const fn server_key(key: &'static str) -> &'static str {
    let mut i = 0;
    while i < KNOWN_FLAGS.len() {
        let m = &KNOWN_FLAGS[i];
        if same(m.key, key) {
            if matches!(m.server, NotRead) {
                panic!("this flag is registered as client-only (`server: NotRead`) — the api reads it, so say what it means without a record");
            }
            return key;
        }
        i += 1;
    }
    panic!("the api reads a flag that isn't in KNOWN_FLAGS (mafold-types/src/flags.rs) — register it");
}

const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_tabs_ship_dark_and_separately() {
        // Two keys, so the 电脑 tab (phase one) never waits on the 回报 tab
        // (phase two); off in production until the owner opens them, on in dev
        // so dev accounts see what's being built; read by both panels.
        for k in ["activityComputer", "activityReport"] {
            let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
            assert!(!m.default && m.dev_default, "{k}: off in production, on in dev");
            assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
            assert_eq!(m.server, NotRead, "{k}: gates the panel only");
            assert!(!server_default(k), "{k}: nothing server-side opens without a record");
        }
    }

    #[test]
    fn modal_morph_ships_dark_on_web() {
        // Off everywhere until ops turns it on for someone, dev included: every
        // Modal on the web moves differently. The phone keeps its own sheets.
        let m = meta("modalMorph").expect("modalMorph registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev");
        assert_eq!(m.clients, &[Web], "the web reads it");
        assert_eq!(m.server, NotRead, "only how the page moves");
        assert!(!server_default("modalMorph"), "nothing server-side opens without a record");
    }

    #[test]
    fn activity_drawer_ships_dark_on_web() {
        // Off everywhere until ops turns it on for someone, dev included: it
        // changes where the Activity Center opens. Only the web reads it.
        let m = meta("activityDrawer").expect("activityDrawer registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev");
        assert_eq!(m.clients, &[Web], "the web reads it");
        assert_eq!(m.server, NotRead, "gates the page only");
        assert!(!server_default("activityDrawer"), "nothing server-side opens without a record");
    }

    #[test]
    fn usd_wallet_ships_dark_everywhere() {
        // It takes Transfer and Convert off the wallet page: off until ops
        // turns it on for someone, dev included. Redeeming a code asks it.
        let m = meta("usdWallet").expect("usdWallet registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev");
        assert_eq!(m.clients, &[Web, Phone], "web and phone read it");
        assert_eq!(m.server, ClosedWithoutRecord, "the api redeems codes only where it's on");
        assert!(!server_default("usdWallet"), "nothing server-side opens without a record");
    }

    #[test]
    fn html_page_fullscreen_ships_dark_everywhere() {
        // It takes the app's full-screen button off every html card already
        // sent: off until ops turns it on for someone, dev included. Both
        // clients draw the card; nothing server-side reads it.
        let m = meta("htmlPageFullscreen").expect("htmlPageFullscreen registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev");
        assert_eq!(m.clients, &[Web, Phone], "web and phone read it");
        assert_eq!(m.server, NotRead, "gates the card's frame only");
        assert!(!server_default("htmlPageFullscreen"), "nothing server-side opens without a record");
    }

    #[test]
    fn cards_mds_ships_dark_everywhere() {
        // It redraws the most-seen cards in every chat: off until ops turns it
        // on for someone, dev included. Both hosts hand it to cards through the
        // theme; nothing server-side reads it.
        let m = meta("cardsMds").expect("cardsMds registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev");
        assert_eq!(m.clients, &[Web, Phone], "web and phone read it");
        assert_eq!(m.server, NotRead, "only the cards' look");
        assert!(!server_default("cardsMds"), "nothing server-side opens without a record");
    }

    #[test]
    fn header_menu_ships_dark_everywhere() {
        // It takes the app icons out of every chat's header: off until ops
        // turns it on for someone, dev included. Only the web draws it so far;
        // nothing server-side reads it.
        let m = meta("headerMenu").expect("headerMenu registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev");
        assert_eq!(m.clients, &[Web], "the web reads it");
        assert_eq!(m.server, NotRead, "only the header's corner");
        assert!(!server_default("headerMenu"), "nothing server-side opens without a record");
    }

    #[test]
    fn header_menu_phone_ships_dark_everywhere() {
        // The phone's ☰: off until ops turns it on for someone, dev included,
        // whatever headerMenu says on the web. Only the phone reads it.
        let m = meta("headerMenuPhone").expect("headerMenuPhone registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev");
        assert_eq!(m.clients, &[Phone], "the phone reads it");
        assert_eq!(m.server, NotRead, "only the header's corner");
        assert!(!server_default("headerMenuPhone"), "nothing server-side opens without a record");
    }

    #[test]
    fn activity_drawer_phone_ships_dark_everywhere() {
        // The phone's drawer: off until ops turns it on for someone, dev
        // included, whatever activityDrawer says on the web. Only the phone reads it.
        let m = meta("activityDrawerPhone").expect("activityDrawerPhone registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev");
        assert_eq!(m.clients, &[Phone], "the phone reads it");
        assert_eq!(m.server, NotRead, "only the bottom bar's M");
        assert!(!server_default("activityDrawerPhone"), "nothing server-side opens without a record");
    }

    #[test]
    fn new_agent_in_three_steps_ships_dark() {
        // It replaces New Agent for everyone: off in production until ops says
        // ship to all, on in dev; read by both clients, nothing server-side.
        let m = meta("newAgentFlow").expect("newAgentFlow registered");
        assert!(!m.default && m.dev_default, "off in production, on in dev");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "web and phone read it");
        assert_eq!(m.server, NotRead, "gates the clients only");
        assert!(!server_default("newAgentFlow"), "nothing server-side opens without a record");
    }

    #[test]
    fn moments_friends_ships_dark() {
        // Friends only reaches fewer people than public, but who counts as a
        // friend is still being settled: opened account by account, never by a
        // missing record.
        let m = meta("momentsFriends").expect("momentsFriends registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "web and phone read it");
        assert_eq!(m.server, ClosedWithoutRecord);
        assert!(!server_default("momentsFriends"));
    }

    #[test]
    fn moments_window_ships_dark_on_its_own() {
        // Pinning and "how far back" open without Friends only: account by
        // account, never by a missing record.
        let m = meta("momentsWindow").expect("momentsWindow registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "web and phone read it");
        assert_eq!(m.server, ClosedWithoutRecord);
        assert!(!server_default("momentsWindow"));
    }

    #[test]
    fn mafold_setting_up_your_first_ai_is_shut_without_a_record() {
        // A new way to make an agent that spends the person's wallet: opened on
        // purpose, account by account, never by a missing record.
        let m = meta("onboardingAgent").expect("onboardingAgent registered");
        assert!(!m.default && !m.dev_default);
        assert!(m.clients.is_empty(), "the api decides; no client reads it");
        assert_eq!(m.server, ClosedWithoutRecord);
        assert!(!server_default("onboardingAgent"));
        assert_eq!(server_key("onboardingAgent"), "onboardingAgent");
    }

    #[test]
    fn chat_link_cards_ship_dark() {
        let m = meta("chatLinkCards").expect("chatLinkCards registered");
        assert!(!m.default, "off in production until a record opens it");
        assert_eq!(m.clients, &[Web, Phone]);
        assert_eq!(m.server, ClosedWithoutRecord, "resolvePage and sendMessage read it");
        assert!(!server_default("chatLinkCards"));
    }

    #[test]
    fn one_consent_card_is_shut_without_a_record() {
        // It changes how every grant is asked for and answered: opened on
        // purpose, account by account.
        let m = meta("consentV2").expect("consentV2 registered");
        assert!(!m.default && !m.dev_default);
        assert!(m.clients.is_empty(), "the api decides; no client reads it");
        assert_eq!(m.server, ClosedWithoutRecord);
        assert!(!server_default("consentV2"));
        assert_eq!(server_key("consentV2"), "consentV2");
    }

    #[test]
    fn new_friends_card_ships_dark_everywhere() {
        // Off in production until ops opens it, and off in dev builds too:
        // the api reads it (it opens the card's one mark), and a dev client
        // drawing a card the dev api won't let it close would be a lie.
        let m = meta("newFriends").expect("newFriends registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev builds");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "web and phone draw it");
        assert_eq!(m.server, ClosedWithoutRecord, "the api keeps its mark shut without a record");
        assert!(!server_default("newFriends"));
        assert_eq!(server_key("newFriends"), "newFriends");
    }

    #[test]
    fn search_console_on_sites_ships_dark_everywhere() {
        // Nothing answers the file until the sites Worker that does is out, so
        // it stays shut until ops opens it, dev builds included: a dev client
        // drawing a field the dev api refuses to save would be a lie.
        let m = meta("siteSearchConsole").expect("siteSearchConsole registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev builds");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "web and phone draw the field");
        assert_eq!(m.server, ClosedWithoutRecord, "the api refuses to save without a record");
        assert!(!server_default("siteSearchConsole"));
        assert_eq!(server_key("siteSearchConsole"), "siteSearchConsole");
    }

    #[test]
    fn a_sites_page_sections_ship_dark_and_separately() {
        // Each stands on something released apart from the api (the sites
        // Worker, the zone's setup, Mafold's Search Console), so each opens on
        // its own, dev builds included. The api reads them; no client does.
        for key in ["siteStats", "siteDomains", "siteSearch", "siteNames"] {
            let m = meta(key).unwrap_or_else(|| panic!("{key} registered"));
            assert!(!m.default && !m.dev_default, "{key}: off in production and in dev builds");
            assert!(m.clients.is_empty(), "{key}: listSites says what a page shows");
            assert_eq!(m.server, ClosedWithoutRecord, "{key}: the api refuses without a record");
            assert!(!server_default(key));
            assert_eq!(server_key(key), key);
        }
    }

    #[test]
    fn the_example_space_ships_dark_and_only_the_api_reads_it() {
        let m = meta("spaceExample").expect("spaceExample registered");
        assert!(!m.default && !m.dev_default, "off in production and in dev builds");
        assert!(m.clients.is_empty(), "listSites says whether there is an example");
        assert_eq!(m.server, ClosedWithoutRecord);
        assert!(!server_default("spaceExample"));
        assert_eq!(server_key("spaceExample"), "spaceExample");
    }

    #[test]
    fn update_pill_and_boot_curtain_ship_dark_and_separately() {
        // Two keys, so the loading screen (seen on every open) can go out
        // without the pill and the pill without it; off in production until
        // the owner opens them, on in dev; read by web (which the desktop app
        // loads) and phone. Nothing on the server waits on either.
        for k in ["updatePill", "bootCurtain"] {
            let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
            assert!(!m.default && m.dev_default, "{k}: off in production, on in dev");
            assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
            assert_eq!(m.server, NotRead, "{k}: read by the clients only");
            assert!(!server_default(k), "{k}: nothing server-side opens without a record");
        }
    }

    #[test]
    fn update_pill_preview_ships_dark_beside_the_pill() {
        // 更新胶囊的「预览」行(更新胶囊 PV):只给要试胶囊的号开 —— 生产关;dev 开,开发包、dev 环境
        // 天然看得到(原来计划里「设置 ▸ 开发者」的演示行由它替代,不另做)。网页(桌面壳加载的就是
        // 它)和手机都读,服务端不看。登记紧跟 updatePill / bootCurtain,写法同它们。
        let k = "updatePillPreview";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && m.dev_default, "{k}: off in production, on in dev");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
        assert_eq!(m.server, NotRead, "{k}: read by the clients only");
        assert!(!server_default(k), "{k}: nothing server-side opens without a record");
        assert_eq!((m.owner, m.added), ("ops", "2026-10"), "{k}: owner and month");
        let at = |key: &str| KNOWN_FLAGS.iter().position(|f| f.key == key);
        assert_eq!(at(k), at("bootCurtain").map(|i| i + 1), "{k}: registered right after updatePill / bootCurtain");
    }

    #[test]
    fn in_app_browser_ships_dark_on_web() {
        // ops 11-03: a link is a tab on the app page of the desktop app, whose
        // page is the web's. Production off until it is turned on for someone.
        let k = "inAppBrowser";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && m.dev_default, "{k}: off in production, on in dev");
        assert_eq!(m.clients, &[Web], "{k}: only the web reads it");
        assert_eq!(m.server, NotRead, "{k}: read by the client only");
        assert_eq!((m.owner, m.added), ("ops", "2026-11"), "{k}: owner and month");
    }

    #[test]
    fn app_beside_ships_dark_on_web_and_phone() {
        // ops 11-02: an app opens beside the chat instead of asking Full / Split.
        // Production off until it is turned on for someone; dev builds see it.
        let k = "appBeside";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && m.dev_default, "{k}: off in production, on in dev");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
        assert_eq!(m.server, NotRead, "{k}: read by the clients only");
        assert_eq!((m.owner, m.added), ("ops", "2026-11"), "{k}: owner and month");
    }

    #[test]
    fn avatar_halo_ships_dark_on_web() {
        // linsky 11-17: the own-colour ring round an avatar being chosen.
        // Production off until it is turned on for someone; dev builds see it.
        let k = "avatarHalo";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && m.dev_default, "{k}: off in production, on in dev");
        assert_eq!(m.clients, &[Web], "{k}: only the web reads it");
        assert_eq!(m.server, NotRead, "{k}: read by the client only");
        assert_eq!((m.owner, m.added), ("linsky", "2026-11"), "{k}: owner and month");
    }

    #[test]
    fn drawer_drag_ships_dark_on_web() {
        // linsky 11-18: drag the drawer's cards into a chat, behind a confirm.
        let k = "drawerDrag";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && m.dev_default, "{k}: off in production, on in dev");
        assert_eq!(m.clients, &[Web], "{k}: only the web reads it");
        assert_eq!(m.server, NotRead, "{k}: read by the client only");
        assert_eq!((m.owner, m.added), ("linsky", "2026-11"), "{k}: owner and month");
    }

    #[test]
    fn share_card_ships_dark_on_web_and_phone() {
        // linsky 11-19 / opsdu 11-20: the QR button's share card, and Premium as
        // a Settings page of its own on the phone layout.
        let k = "shareCard";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && m.dev_default, "{k}: off in production, on in dev");
        assert_eq!(m.clients, &[Web, Phone], "{k}: web and phone read it");
        assert_eq!(m.server, NotRead, "{k}: read by the clients only");
        assert_eq!((m.owner, m.added), ("linsky", "2026-11"), "{k}: owner and month");
    }

    #[test]
    fn harness_memory_ships_dark_and_the_api_keeps_it_shut() {
        // ops 11-22: notes, worklog and summary for Mafold Harness, "flag on
        // for you first" — off everywhere, dev included, until a record opens it.
        let k = "harnessMemory";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off in production and in dev");
        assert_eq!(m.clients, &[Web, Phone], "{k}: the Memory page on web and the phone");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates memory on it, shut without a record");
        assert_eq!((m.owner, m.added), ("ops", "2026-11"), "{k}: owner and month");
    }

    #[test]
    fn dm_privacy_is_a_server_gate_that_ships_dark() {
        // The api enforces a stored setting only where this resolves on, so
        // with no record nothing is enforced and nothing is offered.
        let k = "dmPrivacy";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && m.dev_default, "{k}: off in production, on in dev");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn connections_page_is_a_server_gate_that_ships_dark() {
        // The page's two new routes answer only where this resolves on.
        let k = "connectionsPage";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && m.dev_default, "{k}: off in production, on in dev");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn wallet_on_computers_is_a_server_gate_that_ships_dark() {
        // createBot and mintReplyKey read it on the owner; dev too stays off
        // until a record opens it, since a dev wallet call spends a real seat.
        let k = "walletOnComputers";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off until a record opens it, dev included");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn opencode_go_is_a_server_gate_that_ships_dark() {
        // createBot and listHarnessHosts read it on the owner; dev stays off
        // too until a record opens it for someone.
        let k = "openCodeGo";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off until a record opens it, dev included");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn harness_on_claude_is_a_server_gate_that_ships_dark() {
        // createBot reads it on the owner; dev stays off too until a record
        // opens it, since a turn on it spends a real Claude plan.
        let k = "harnessOnClaude";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off until a record opens it, dev included");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn own_key_on_computers_is_a_server_gate_that_ships_dark() {
        // createBot and openModelKey read it on the owner; the panel learns it
        // from the api's `gates` answer, not from a client registry. Dev stays
        // off too until a record opens it.
        let k = "ownKeyOnComputers";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off until a record opens it, dev included");
        assert!(m.clients.is_empty(), "{k}: only the api reads it");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn harness_on_codex_is_a_server_gate_that_ships_dark() {
        // createBot, listHarnessHosts and New Agent's `gates` read it on the
        // owner; dev stays off too until a record opens it, since a turn on it
        // spends a real ChatGPT plan.
        let k = "harnessOnCodex";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off until a record opens it, dev included");
        assert!(m.clients.is_empty(), "{k}: the panel hears it from the api");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn usage_model_switch_is_a_server_gate_no_client_reads() {
        // The card learns it from the api's answer to `model|menu`; a client
        // resolving it would be a second source that can disagree.
        let k = "usageModelSwitch";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off until a record opens it, dev included");
        assert!(m.clients.is_empty(), "{k}: only the api reads it");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn forum_everywhere_is_a_server_gate_that_ships_dark() {
        // createChannel turns a chat into a forum only where this resolves on;
        // a dev build doesn't show the pill everywhere while dev's server
        // still says "not a forum".
        let k = "forumEverywhere";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off until a record opens it, dev included");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
        assert!(m.clients.contains(&Daemon), "{k}: the daemon words its agents' channel advice on it");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn wallet_v2_ships_dark_on_both_clients() {
        // ops 10-29: "Build it behind a flag". Off everywhere, dev included,
        // until a record opens it for named accounts; the server sends the
        // payment link (`funds`) to every client either way.
        let k = "walletV2";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off until a record opens it, dev included");
        assert!(m.clients.contains(&Web) && m.clients.contains(&Phone), "{k}: web and phone read it");
        assert_eq!(m.server, NotRead, "{k}: gates the page only");
        assert!(!server_default(k), "{k}: nothing server-side opens without a record");
    }

    #[test]
    fn mailbox_is_a_server_gate_that_ships_dark() {
        // Who gets whose mail is the mapping's and the ACL's to decide; the
        // flag only hides it — and with no record it hides it from everyone,
        // dev included, until ops opens it account by account.
        let k = "mailbox";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off until a record opens it, dev included");
        assert_eq!(m.clients, &[Daemon], "{k}: the daemon words its agents' tool list on it; web and phone see only cards");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn py_sandbox_is_a_server_gate_that_ships_dark() {
        // The api runs code only for agents whose owner has it; the daemon
        // tells its agent about `mafold sandbox` on the same answer, so an
        // agent is never told of a tool the api would refuse it.
        let k = "pySandbox";
        let m = meta(k).unwrap_or_else(|| panic!("{k} registered"));
        assert!(!m.default && !m.dev_default, "{k}: off until a record opens it, dev included");
        assert_eq!(m.clients, &[Daemon], "{k}: the daemon words its agents' prompt on it; no screen reads it");
        assert_eq!(m.server, ClosedWithoutRecord, "{k}: the api gates on it");
        assert!(!server_default(k), "{k}: shut without a record");
        assert_eq!(server_key(k), k);
    }

    #[test]
    fn keys_are_unique_and_described() {
        for (i, m) in KNOWN_FLAGS.iter().enumerate() {
            assert!(KNOWN_FLAGS[..i].iter().all(|o| o.key != m.key), "{} twice", m.key);
            assert!(!m.label.is_empty() && !m.description.is_empty() && !m.owner.is_empty(), "{}", m.key);
            assert!(m.added.len() == 7 && m.added.as_bytes()[4] == b'-', "{}: added is yyyy-mm", m.key);
            // A flag nobody reads has no business here.
            assert!(!m.clients.is_empty() || m.server != NotRead, "{} is read by nobody", m.key);
        }
    }

    #[test]
    fn an_absent_record_opens_only_what_says_so() {
        assert!(server_default("gardenApps"));
        assert!(server_default("premiumSales"));
        assert!(!server_default("topups"));
        assert!(!server_default("modelBot:gemini"));
        assert!(!server_default("showIds"), "client-only");
        assert!(!server_default("modelBot:someone-new"), "unregistered ⇒ shut");
    }

    #[test]
    fn server_key_is_the_key() {
        const K: &str = server_key("inlineMedia");
        assert_eq!(K, "inlineMedia");
    }

    #[test]
    fn the_phone_send_flight_ships_dark_and_only_the_phone_reads_it() {
        let m = meta("sendFlightPhone").expect("sendFlightPhone registered");
        assert!(!m.default && !m.dev_default, "unseen on a device: on for someone first");
        assert_eq!(m.clients, &[Phone], "web flies every send already");
        assert_eq!(m.server, NotRead);
    }
}
