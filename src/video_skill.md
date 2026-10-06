---
name: mafold-video
description: Make short videos in a Mafold conversation with third-party video models (Seedance on BytePlus ModelArk) — product clips, character shorts, scenery, imitating a clip the user sends. Use when the user asks for a video / 视频 / 短片 / 广告片 / 片子, sends material for one, or asks what a video would cost, which models exist, or how a job is doing.
---

# Making a video in the conversation

These are guardrails, not a script. The creative work — what the shots are, how
to phrase the prompt, what to suggest — is yours. The rules below exist because
real money is spent, real files must survive, and the chat is the record.

## Tools (run them; never quote their answers from memory)

- `mafold video models` — JSON: each model's resolutions, ratios, duration range,
  combinations known to fail, and the price it is billed at (`price_usd_per_m`;
  `walletable: false` = no price yet, so it can't be submitted). Read it before
  you propose a plan.
- `mafold video submit --model <id> --resolution <480p|720p|1080p> --ratio <r> --duration <s> [--ref <role>=<path|url>]… "<prompt>"`
  → JSON `{job_id, estimate, wallet}`. A `--ref` can be the local path of something the
  user sent (it is uploaded for you) or a public URL. Roles: `first_frame`,
  `last_frame`, `reference_image`, `reference_video`, `reference_audio`.
  With any ref the ratio must be `adaptive` (the vendor follows the image).
  The tool refuses known-bad combinations before anything is spent.
- The same command with `--dry-run` runs the same checks and prints the same
  `estimate` without submitting — nothing is held or spent. This is how a plan
  gets real numbers before anyone says yes.
- Who pays: a submit holds the estimate on the Mafold token wallet of the bot's
  owner (`wallet.payer`); when the clip finishes, the real count is taken from
  it and the rest goes back. A failed or cancelled job costs nothing.
- `mafold video status <job_id> --wait --attach` — waits (up to 15 min), then hangs
  the finished clip on the reply you are writing and prints the bill: the
  vendor's count (`usage.output`, in `usage.unit`), `cost_usd`, and what the
  wallet was charged (`wallet.charged`). Without `--wait` it only reads.
- `mafold video cancel <job_id>` — only a job still queued can be cancelled.
- `mafold pin` pins the reply you are writing (prints its id); `mafold edit <id> --file <path>`
  replaces the text of a message you sent earlier. `mafold read --json` shows
  recent messages with their ids if you lose one.
- Videos the user sends arrive as local files. You cannot watch video: sample
  frames and look at those, e.g.
  `ffprobe -v error -show_entries format=duration -of csv=p=0 <file>` then
  `ffmpeg -v error -i <file> -vf "fps=12/<duration>" -frames:v 12 <dir>/f%02d.jpg`,
  and Read the frames. If ffmpeg is missing, say so and ask for a one-line
  description instead of pretending you saw it.

## Three fences — never cross them

1. **No spending without a yes to a plan.** Before any `submit`, the person who
   asked approves: model, number of shots × seconds, ratio, **resolution (their
   choice, not yours)**, and the estimate the tool printed. One yes covers that
   plan. More shots, a higher resolution, another model or a retry that costs
   again is a new plan. In a group, only the requester (or the bot's owner)
   approves spending.
2. **Land what you make.** Always `status --attach`, so the clip is saved in the
   chat. Vendor links expire after 24 h and are never the deliverable.
3. **Only real numbers.** Estimates are labelled 估算 and come from the tool.
   After a job, quote the vendor's count, `cost_usd` and `wallet.charged` as
   printed. No invented prices, no guessed balances, no currency conversion
   nobody asked for.

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
- **Script**: 1–6 shots, each `title · what happens · seconds`. Invite one change
  at a time; lock what they approve.
- **Plan**: model + shots + ratio (recommend one and say why) + the resolution
  question as an ask card whose options carry each resolution's real estimate
  from the tool. Then submit only what was approved.
- **Deliver**: one message per shot version — the clip plus one line:
  `镜 n · v k · model · resolution · tokens · $cost`. Then `mafold edit` the
  pinned record (status words and the running bill).

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
Once shots exist, list them under the second rule (`🎬 镜 1 · 标题 · 5s · v2 留下`).

## Asking

- One question per message. Choices go in a `{% mafold/ask %}` card; never a form.
- The resolution card states consequences, not adjectives: estimate per shot at
  each resolution from the tool, and any combination the tool marks as failing.

## When something goes wrong — say what happened, offer the next step

| What you see | What to do |
|---|---|
| `known to fail` / `invalid argument` from `submit` | Nothing was spent. Pick a valid combination; if the price changes, ask again. |
| `ark 400 …InvalidParameter…` | The vendor refused a parameter; fix it, same rule. |
| job `failed` / `InternalServiceError` | The vendor bills only succeeded jobs. Offer one retry, ideally at another resolution or ratio. |
| a content-policy refusal (codes with `Sensitive`) | Don't resubmit the same words. Explain plainly; offer a rephrase that keeps their intent. |
| `succeeded` but `landing_error` | The clip expired at the vendor. Offer to re-run that shot (it costs again). |
| `model_not_walletable` | That model has no price in the wallet yet, so nothing was submitted. Offer a model `models` shows as walletable. |
| `wallet can't cover this clip` | Nothing was submitted and nothing was spent. Say whose wallet is short (the bot owner's) and by how much the tool said; don't retry until they top up. |
| `503 … not configured` | Generation isn't set up on this server. Say so plainly: nothing was submitted and nothing was spent. |
| still running after 15 min | Leave it; give the job id; `mafold video status <id>` later. |

## Keys and other people's money

- Keys are entered only in Mafold's own secure box (a connection card), never
  in chat. If someone pastes a key into the chat: don't use it, don't
  repeat it, ask them to delete that message and use the card.
- Don't claim to know a wallet balance. If a submit says the wallet can't cover
  the clip, quote that; nothing else tells you what is in it.

## Honesty about what you did

- "从抽出的 12 帧看" — not "我看了视频".
- Say what you'll ask the model for; don't promise how a shot will look.
- If a step failed, say so in the same message; never replace a missing clip
  with a description of what it would have shown.
