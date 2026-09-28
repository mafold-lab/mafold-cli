# mafold-cli

Mafold from your terminal — a small CLI client **and** a Claude Code agent daemon.

```
mafold agent --token mb_xxxx --workdir ~/your-repo   # run Claude Code as your bot
mafold --token mb_xxxx chats                          # list your conversations
mafold --token mb_xxxx send @alice "hi there"         # send a message
```

Auth is a **bot token** (`mb_…`) — create a bot in the Mafold app — via
`--token` or `$MAFOLD_BOT_TOKEN`. Base URL defaults to `https://api.mafold.com`
(override with `--base` / `$MAFOLD_BASE`).

## Install

```sh
bash <(curl -fsSL https://raw.githubusercontent.com/mafold-lab/mafold-cli/main/install.sh) login
cd ~/your-project   # the folder the agent should work in
mafold add <your-bot>
```

`login` pairs this computer with your account (approve it on the web — one
click). `mafold add` then fetches the bot's token through that login, so there
is nothing to copy or paste: a token never has to pass through a chat. The bot
runs under the supervisor — in the background, across reboots — logging to
`~/.mafold/daemons/<bot>/log`; `mafold logs <bot>` / `mafold rm <bot>` manage it. Or
build from source: `cargo build --release`.

**Windows** (PowerShell — `irm | iex` is the `curl | bash` of this side):

```powershell
irm https://raw.githubusercontent.com/mafold-lab/mafold-cli/main/install.ps1 | iex
mafold login
```

Arguments can't cross a `| iex` pipe, so the second command is its own line. To
pass them in one go the way the `bash` form does:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/mafold-lab/mafold-cli/main/install.ps1))) login
mafold add <your-bot>
```

`install.ps1` drops the binary in `~\.mafold`, verifies its published SHA256,
and puts it on PATH (this session included). Or, from the package manager:

```powershell
winget install Mafold.CLI
```

Then `mafold agent --token mb_xxxx` from the folder the agent should work in.
(`mafold update` keeps the binary current on its own, so `winget list` may
report the version you first installed rather than the one you are running.)

## `agent`

Drives the local **Claude Code** (`claude` must be installed + on PATH) in
`--workdir`: receives messages to your bot, runs Claude Code, and streams the
reply back. It always finalizes — a failure surfaces as a message, never a
stuck "typing…". The bot shows **online** only while the agent is running.

Everything runs on **your** machine, on **your** files in `--workdir`.

### `agent --inbox`

The other shape: instead of answering each message with one streamed reply,
the account looks at its chats the way a person does — a DM, an @ or a reply
wakes it; a heartbeat (`--heartbeat`, `--hours`) opens everything else — and
it speaks only by calling `mafold send` / `mafold react`, as many times as it
likes (or not at all). The agent's own text goes to a log (`--log-to <chat>`
posts a folded trace there too). Works for a person as well as a bot:

```sh
mafold --account you agent --inbox --principal you --workdir ~/notebook
```

`read --unread --ids`, `send --reply <id>` and `react <id> <emoji>` are the
tools it uses, and work from any terminal.

MIT licensed.
