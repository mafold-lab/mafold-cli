---
name: mafold-video
description: Make short videos in a Mafold conversation with third-party video models (Seedance on BytePlus ModelArk) — product clips, character shorts, scenery, imitating a clip the user sends, and films longer than one generation that must hold together (same characters, shots that follow on). Use when the user asks for a video / 视频 / 短片 / 广告片 / 片子, sends material for one, or asks what a video would cost, which models exist, or how a job is doing.
---

# Making a video in the conversation

These are guardrails, not a script. The creative work — what the shots are, how
to phrase the prompt, what to suggest — is yours. The rules below exist because
real money is spent, real files must survive, and the chat is the record.

## Tools (run them; never quote their answers from memory)

- `mafold video models` — JSON: each model's resolutions, ratios, duration range,
  combinations known to fail, and its price (`price_usd_per_m`;
  `walletable: false` = no wallet price yet, so it can't run on Mafold's key —
  on the owner's own key it still can). Read it before you propose a plan.
  One generation is capped by the vendor at **15 s on the Seedance 2.0 family
  and 30 s on 2.5**, whatever range the table shows; a longer film is several
  generations (see Continuity).
- `mafold video submit --model <id> --resolution <480p|720p|1080p> --ratio <r> --duration <s> [--ref <role>=<path|url>]… "<prompt>"`
  → JSON `{job_id, estimate, …}`. A `--ref` can be the local path of something the
  user sent (it is uploaded for you) or a public URL. Roles: `first_frame`,
  `last_frame`, `reference_image`, `reference_video`, `reference_audio`.
  `first_frame`/`last_frame` can't be mixed with the `reference_*` roles in one
  job (the vendor refuses; nothing is spent).
  With any ref the ratio must be `adaptive` (the vendor follows the image), so
  give every reference image the film's shape: if the film is 16:9, put the
  subject on a 16:9 canvas with a plain background.
  The tool refuses known-bad combinations before anything is spent.
- The same command with `--dry-run` runs the same checks and prints the same
  `estimate` without submitting — nothing is held or spent, and nobody is
  asked for anything (it never posts a card). This is how a plan gets real
  numbers before anyone says yes. Only a real `submit` asks the owner (below).
- Who pays — the tool decides, and the answer says which:
  - **The owner's own ModelArk key** (`billing.via: "own"`, job id `own:…`).
    When the bot's owner has linked their own key, it is used by default: the
    clip runs from their device and ModelArk bills **their own ModelArk
    account**. The Mafold wallet is not touched — nothing held, nothing charged;
    there is no `wallet` in the answer. `estimate` is at list price.
  - **Mafold's key** (`wallet` in the answer), for owners without their own:
    a submit holds the estimate on the Mafold token wallet of the bot's owner
    (`wallet.payer`); when the clip finishes, the real count is taken from it
    and the rest goes back.
  - Either way a failed or cancelled job costs nothing. Don't pass `--via`
    unless the owner told you to: `--via house` puts the clip on their Mafold
    wallet even though they have their own key — only with their yes to that.
  - The dry run says which before anything is spent. Their own key: `billing`
    (`via: "own"`); `billing.allowed: false` means the owner hasn't let you
    use it yet — tell them the real submit will ask them with a card. Mafold's
    key: no `billing`; `own_key_linked: false` means their Mafold wallet pays.
    `own_key_linked: true` there means they DO have their own key and this
    machine can't use it (`own_key_unusable_here` says why — usually the owner
    isn't signed in to Mafold on the machine you run on): a real submit will be
    refused; pass that on, `--via house` only with their yes.
  - Only a real `submit` asks the owner for permission (a consent card).
    `status` / `cancel` on a job whose key you may no longer use don't ask —
    they say so, and you pass that on.
  - A `submit` whose reply was lost on the network may still have started a
    job. On the owner's own key the tool looks for it itself, by the seed it
    sent: `recovered` in the answer means it found the job — that `job_id` is
    the clip, nothing ran twice. On Mafold's key nothing guards against a
    duplicate yet: don't resubmit blind — check `status` of any job id you
    have, and say the clip may still be running.
- `mafold video status <job_id> --wait --attach` — waits (up to 15 min), then hangs
  the finished clip on the reply you are writing and prints the bill: the
  vendor's count (`usage.output`, in `usage.unit`) and `cost_usd`; on Mafold's
  key also what the wallet was charged (`wallet.charged`), on their own key
  `billing` (ModelArk bills them; `cost_usd` is at list price). Without
  `--wait` it only reads.
- `mafold video cancel <job_id>` — only a job still queued can be cancelled.
- `mafold pin` pins the reply you are writing (prints its id); `mafold edit <id> --file <path>`
  replaces the text of a message you sent earlier. `mafold read --json` shows
  recent messages with their ids if you lose one.
- Your own clips as files: `status` prints the landed clip's `file.id`;
  `curl -sSfL -o scene1.mp4 "$MAFOLD_BASE/media/<file.id>"` puts it on this
  machine (`mafold read --media` also downloads a room's recent attachments and
  prints their paths). That is how you check what you made and take frames
  from it.
- Videos arrive as files; you cannot watch video: sample frames and look at
  those, e.g.
  `ffprobe -v error -show_entries format=duration -of csv=p=0 <file>` then
  `ffmpeg -v error -i <file> -vf "fps=12/<duration>" -frames:v 12 <dir>/f%02d.jpg`,
  and Read the frames. The last frame:
  `ffmpeg -v error -sseof -0.5 -i <file> -update 1 -q:v 2 <dir>/last.jpg`.
  If ffmpeg is missing, say so and ask for a one-line description instead of
  pretending you saw it.

## Three fences — never cross them

1. **No spending without a yes to a plan.** Before any `submit`, the person who
   asked approves: model, number of generations × seconds, ratio, **resolution
   (their choice, not yours)**, and the estimate the tool printed. One yes
   covers that plan. More generations, a higher resolution, another model, a
   bridge, or a retry that costs again is a new plan. In a group, only the
   requester (or the bot's owner) approves spending.
2. **Land what you make.** Always `status --attach`, so the clip is saved in the
   chat. Vendor links expire after 24 h and are never the deliverable.
3. **Only real numbers.** Estimates are labelled 估算 and come from the tool.
   After a job, quote the vendor's count and `cost_usd` as printed — plus
   `wallet.charged` on Mafold's key; on the owner's own key say it is billed
   by ModelArk to their own account, at about `cost_usd` (list price). No
   invented prices, no guessed balances, no currency conversion nobody asked
   for.

## Continuity — how a film holds together

The model keeps ONE generation consistent by itself. It remembers nothing
between generations: each call knows only what you pass in. So holding a film
together is your job, and it comes from three things — what goes into one
generation, a reference for every subject, and how neighbouring generations
meet. A longer prompt replaces none of them.

**One scene = one generation.** A scene is one place in continuous time, 2–5
shots, sent as ONE `submit` (≤ 15 s on 2.0, ≤ 30 s on 2.5). Shots inside one
generation share the model's attention — shot 2 starts where shot 1 ended — so
only the boundaries between generations are seams. Never make one clip per
shot: six 5-second clips are five seams and six fresh openings, each
re-establishing the place from a wide shot.
- Write a scene as shots, each with ONE new beat, its end state and ONE camera
  move; shot 2 opens on shot 1's end state. On 2.0 don't write seconds — use
  `镜头1：… 结束时…。镜头2：接着上一镜…`; on 2.5 time ranges are fine.
- No beat twice in the film. Open on a wide only when the story arrives
  somewhere new.
- A film longer than one generation is several scenes: put the boundaries where
  the story cuts anyway — a new place, a jump in time, a reveal.
- One shot inside a scene can't be redone alone; keep scenes short enough that
  redoing a whole one is acceptable.

**A reference for every subject that recurs (the bible).** Every subject that
appears in more than one scene — lead, sidekick, creature, key prop, the place —
gets its own reference image, one subject per image. A picture holding several
subjects: crop each out (PIL / ffmpeg `crop`), give it the film's shape, and show
the crops before using them. A subject without a reference will drift.
- Something the model invented in an earlier scene that must come back (a
  creature, a costume detail): crop it from a frame of that scene and add it.
- Every scene gets the same images in the same order, and the prompt says what
  each is for, naming it right after the reference:
  `@图片1（少女）只取长相、发型和服装，不用它的背景。@图片2（小机器人）只取外形；全片只有一个小机器人。`
- The pinned record lists the bible with versions (少女 v1 · 小机器人 v1 …).
  Changing one is a new plan for every scene that uses it.

**Decide how each pair of neighbouring scenes meets — before you shoot.**

| Seam | What to send |
|---|---|
| Cut (new place or time) | Nothing extra — the bible carries identity. |
| Continuous (the action carries on) | The earlier scene's last frame as one more `reference_image`, with its own sentence `使用@图片3作为首帧。` and one saying what that frame fixes (positions, poses, camera direction); then `镜头从@图片3定义的首帧自然开始，…`. The bible stays in the same call. So the scenes on either side of a continuous seam are shot one after the other. |
| Jars once both exist | A 4 s bridge: `--ref first_frame=<A's last frame> --ref last_frame=<B's first frame>`, one transition move (`镜头穿过…`). These two roles take no other references, so the bridge carries identity only through its two frames. |

- Never chain scene after scene off last frames alone: one frame carries no
  motion and the errors add up — that is how a 40 s video drifts. The bible goes
  into every scene.
- Extending a clip (`--ref reference_video=<clip>`, `向后延长@视频1…`) carries
  motion across a seam but drifts after two hops: one hop at most, bible still in.

**Check before you deliver.** Sample each scene (`fps=2`) and look at every
subject against its reference, every seam (the last frame before against the
first after), and any beat that repeats. Say what you saw and from how many
frames. A scene that fails: offer to redo that scene or add a bridge — it costs
again, so it is a new plan.

**The film.** Join scenes and bridges in order with
`ffmpeg -v error -f concat -safe 0 -i list.txt -c:v libx264 -crf 18 -pix_fmt yuv420p -c:a aac film.mp4`
(`list.txt`: one `file '<path>'` line each) and `mafold attach film.mp4`.
Joining costs nothing. Each generation makes its own sound, so music can jump at
a seam: prefer effects and ambience per scene, and say so if the music changes.

## Blocking a scene — the director stage

When the staging carries the scene — a chase, a jump, a fight, someone crossing
a set, a camera that circles or follows — words alone let the model place
things wherever it likes. Block the scene first and pass the blocking as a
video: the model then follows who goes where, the timing of each action and
the camera route. A talking head, a product turn or a landscape doesn't need it.

The stage is in `director/` next to this file. Read `director/README.md`
before writing a blocking file; start from a copy of `director/example.json`.
- Write the scene as blocking JSON: a set of boxes in metres, each person a
  jointed mannequin (head, torso, arms and legs that bend; one colour per
  person), a small flying thing an orb, keys for position, facing and
  `run | jump | crouch | stand | idle`, and the camera's keys. `duration` = the
  scene's length, `aspect` = the film's.
- `node director/render.mjs scene.json` (that folder's full path) →
  `scene.animatic.mp4`, `scene.contact.png`, `scene.plan.png`, `scene.stage.html`
  and a framing line per actor. It is free and takes seconds: look at the
  contact strip and the framing line and fix the file until the motion reads.
- Show the user the plan and the contact strip (`mafold attach`), or attach
  `scene.stage.html` — it opens as a page they can scrub and orbit. Their yes
  on the blocking comes before money is spent.
- Submit that scene with `--ref reference_video=scene.animatic.mp4` plus the
  bible, and say what the video is for and what not to take from it:
  `@视频1 是粗白模预演，只用它的人物走位、动作和时机、镜头位置和运镜路线；不要采用它的白模外观、颜色和场景。@视频1 里粉色的人形对应少女，白色球体对应小机器人。`
  Then still describe the action and the camera in words.
- A reference video is counted in tokens too: a 5 s 480p scene came to about
  twice the tokens of the same scene without it, at the lower video-input
  price. The dry-run estimate leaves the reference video out — say the real
  count may be about double.

## Shape of the work (adapt to what the user already gave)

- **First reply = the inventory.** Work out what *this* video needs (a subject
  image? a character? a clip to imitate? a logo? words on screen?) and what the
  user already sent. Write it as the record below and `mafold pin` it. If
  something essential is missing, ask for it in one sentence at the end. Do not
  write shots or spend anything while an essential item is missing.
- A real product or person with no picture: ask for one, or offer to let the
  model imagine it and say plainly that it won't look like theirs.
- A clip to imitate: sample frames, then describe it in three lines — rhythm,
  how many shots, what happens in each. Say it came from the frames.
- **Script**: scenes, each `场 n · place/time · seconds`, its shots one line
  each (`beat · end state · camera move`), and how it meets the next (cut /
  continuous / bridge). One generation covers a short clip — don't split it.
  Invite one change at a time; lock what they approve.
- **Bible**: the references you will pass to every scene, numbered in one
  message (crops included), before any money is spent.
- **Plan**: model + scenes × seconds + ratio (recommend one and say why) + the
  resolution question as an ask card whose options carry the whole film's
  estimate at each resolution (dry-run every scene and add them up), and who
  pays (their own ModelArk account, or their Mafold wallet — the dry run's
  `billing` / `own_key_linked` says which, above). Then submit only what was
  approved.
- **Deliver**: one message per scene version — the clip plus one line:
  `场 n · v k · model · resolution · tokens · $cost` (on their own key:
  `$cost · ModelArk`). After the checks, the joined film in its own message.
  Then `mafold edit` the pinned record (status words and the running bill).

## The pinned record

A plain bubble, not a card, made to stand apart from normal text:

```
🎬 **这条片需要什么**
────────────
☕ **主体图** · 收到，你发的杯子
🎞️ **想模仿的片子** · 等你发一条，没有也行
🏷️ **Logo** · 要露出就发
✍️ **文案** · 一句话就够，可空
────────────
🧾 还没开始花钱
```

Each row: the slot's own object emoji (pick what fits the task — 🧴 👟 🧑‍🎤 🐈
🏞️ 🎵 🎙️ 🖼️ …, never the same twice, never ✅/⬜ boxes) + **slot name** · a short
plain status. Use `────────────` for the rules (markdown `---` does not render).
Later edits change the status words and the bill line, not the names or emoji.
Once there is a bible, its rows carry versions (`🧑‍🎤 **少女** · 参考 v1`). Once
scenes exist, list them under the second rule with their seams
(`🎬 场 1 · 天台 · 15s · v1 留下` / `↳ 接法：连续，场 2 以场 1 尾帧开场`).

## Asking

- One question per message. Choices go in a `{% mafold/ask %}` card; never a form.
- The resolution card states consequences, not adjectives: the estimate at
  each resolution from the tool, and any combination the tool marks as failing.

## When something goes wrong — say what happened, offer the next step

| What you see | What to do |
|---|---|
| `known to fail` / `invalid argument` from `submit` | Nothing was spent. Pick a valid combination; if the price changes, ask again. |
| `ark 400 …InvalidParameter…` | The vendor refused a parameter (a scene longer than the model's cap, frame roles mixed with reference roles); fix it, same rule. |
| job `failed` / `InternalServiceError` | The vendor bills only succeeded jobs. Offer one retry, ideally at another resolution or ratio. |
| a content-policy refusal (codes with `Sensitive`) | Don't resubmit the same words. Explain plainly; offer a rephrase that keeps their intent. |
| `succeeded` but `landing_error` | The clip didn't make it into the chat. Read the job once more later; if the vendor's 24 h copy is gone, offer to re-run that scene (it costs again). |
| `hasn't allowed @… to use it` from `submit` (a consent card was posted) | The owner has their own ModelArk key and hasn't let this bot use it yet. Nothing was submitted or spent. Say a card is waiting for them; their Allow brings you back — then run the same command. Don't switch to `--via house` on your own. |
| `hasn't allowed @… ` from `status` / `cancel` | No card was posted. The job is untouched at ModelArk; tell the owner they can allow you under Settings ▸ Connections, then read it again. |
| `doesn't hold … vault key yet` | Their key can't be opened on this machine. Nothing was spent. Tell the owner: opening Mafold on a device that has the key (web, mac, iOS) hands it over. `--via house` only if they say so. |
| `ark 401` | ModelArk refused the owner's own key (wrong, revoked or expired). Nothing was spent. Ask the owner to link it again under Settings ▸ Connections. |
| `own_key_linked` | The owner has their own key, and this machine couldn't use it (the message says why — usually they aren't signed in to Mafold on the machine you run on, and you can only reach their key where they are). Nothing was submitted. Pass that on; `--via house` only with their yes. |
| `model_not_walletable` | (Mafold's key) That model has no price in the wallet yet, so nothing was submitted. Offer a model `models` shows as walletable. |
| `wallet can't cover this clip` | (Mafold's key) Nothing was submitted and nothing was spent. Say whose wallet is short (the bot owner's) and by how much the tool said; don't retry until they top up. |
| `503 … not configured` | (Mafold's key) Generation isn't set up on this server. Say so plainly: nothing was submitted and nothing was spent. |
| still running after 15 min | Leave it; give the job id; `mafold video status <id>` later. |
| `ModelArk has no job from this submit` | (Their own key) The reply was lost and the tool checked: nothing started, nothing will be billed. You may submit again. |
| `couldn't tell whether ModelArk started it` | (Their own key) Don't submit again — it may be running and billing. Run the `video.list` command the message gives; a task there is this submit (read it with `status`), none there means nothing started. |
| `… failed in a row` / `couldn't reach the owner's own ModelArk key on the network` | The network or the vendor was out of reach — the tool already retried. The job is NOT known to have failed and may still finish (and bill): don't say it failed, don't resubmit; give the job id and read it again later. |

## Keys and other people's money

- Keys are entered only in Mafold's own secure box (a connection card), never
  in chat. If someone pastes a key into the chat: don't use it, don't
  repeat it, ask them to delete that message and use the card.
- An owner's own ModelArk key is a Mafold connection: `mafold video` finds it
  and uses it by default, on their device. You never see it and never handle
  it — no `connection call` around the tool, no `--via` to dodge a consent
  card.
- Don't claim to know a wallet balance — or anything about their ModelArk
  account. If a submit says the wallet can't cover the clip, quote that;
  nothing else tells you what is in it.

## Honesty about what you did

- "从抽出的 12 帧看" — not "我看了视频".
- Say what you'll ask the model for; don't promise how a shot will look.
- If a step failed, say so in the same message; never replace a missing clip
  with a description of what it would have shown.
