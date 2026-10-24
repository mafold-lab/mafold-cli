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
    // `providerOffered`). Google's app is in Testing — Google lets only listed
    // test accounts consent — so its rollout is an account list, not a percent.
    FlagMeta {
        key: "provider:google",
        default: false,
        dev_default: true,
        label: "Google connection",
        description: "Offers Google (Gmail + Calendar) under Add connection. Its Google app is in \
                      Testing: only invited test accounts can consent.",
        owner: "ops",
        added: "2026-09",
        clients: &[Web],
        server: NotRead,
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
        description: "The garden button opens mafold/garden and 收进园地 writes its pins, on web and phone \
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
}
