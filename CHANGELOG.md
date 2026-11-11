# Changelog

Each release is on the [releases page](https://github.com/Ryadel/Kelpy/releases), with one executable per platform,
the install scripts and the signed checksums. Versions follow [Semantic Versioning](https://semver.org).

## Unreleased

- **Terminal UI**: a message sent without a usable AI provider is no longer lost in a conversation that showed
  *Loading…* forever. Kelpy keeps it in the box and says what is missing, such as "No AI provider configured yet:
  choose one in the Web UI or run 'kelpy provider'". A message Kelpy refuses for another reason comes back to the
  box with the reason, and the conversation no longer waits for an answer that never began. An empty conversation
  says so.
- **Terminal UI**: the Web UI's link on the start screen is a real link in Windows Terminal and most terminals,
  opened with Ctrl+click, and a line under it says how to open it. Ctrl+O opens the Web UI in the browser, signed
  in, from any screen and in the classic Command Prompt too; it is not offered over SSH, where the browser would be
  the server's.
- **Terminal UI and Web UI**: Up and Down in the message box bring back the messages already sent in the open
  conversation, as a shell brings back its commands; Down goes forward again to what was being written.
- **Web UI**: the conversation is wider on large screens. While an answer is being thought out, the creature from
  folklore shown meanwhile is easier to read, with its tradition and its source.
- **Web UI**: the first start, without an AI provider, opens on a welcome with Kelpy's logo and what it does,
  beside the choice of how Kelpy connects to AI.
- **Web UI**: the System access and Approval mode menus close as soon as an option is chosen.
- **Restart as administrator**: when an action fails for lack of administrator rights, the Web UI offers to
  restart Kelpy as administrator, under the action and in the System access menu. Windows asks for consent in its
  own prompt; Kelpy restarts at the same address, and the page reloads by itself, still signed in, with the keys,
  sign-ins and secrets as before.
- **Running `kelpy` again**: when Kelpy is already running, it says where (the Web UI's link), since when, its
  process and version, and its home, then offers to open the terminal UI, open the Web UI, stop Kelpy, or leave it
  running. Stopping asks first when an answer is being written. Without a terminal it prints the same facts and
  `kelpy stop`. While Kelpy is starting or being updated, it waits up to half a minute.

## 0.4.1 (2026-11-09)

Fixes to the terminal UI's start screen and the Web UI, and a Windows install command that also runs from the
Command Prompt.

- **Updates**: when GitHub answers the check for the latest release with an error, such as 504, `kelpy update` names
  it.
- **Terminal UI**: on the start screen, the Web UI, the access code, the home, the account and the vault are listed
  with their names in a column, each value going on under itself; what the AI provider still needs is shown whole.
- **Web UI**: every button, menu item and tab shows the pointing hand.
- **Install on Windows**: the command is now `powershell -c "irm https://kelpy.io/install.ps1 | iex"`, which runs from
  the Command Prompt as well as PowerShell.

Updating from 0.4.0: Kelpy offers the update at its next start, or run `kelpy update`.

## 0.4.0 (2026-11-09)

Kelpy now has a terminal UI, remote access over its own SSH server, and files and secrets.

- **Remote access**: `kelpy --ssh [PORT]` also starts Kelpy's own SSH server: keys only, and each connection waits
  until you allow it in the terminal UI or the Web UI (`--allow-key` for unattended use). A connection gets the
  terminal UI, SFTP to the uploaded files and forwarding to the Web UI only, never a command or a shell. Every
  connection is logged in `logs/remote.log`; without another limit Kelpy stops after 1 hour without activity.
- **Windows Server**: with System access Full, Kelpy installs Windows features, such as IIS, with
  `Install-WindowsFeature`, and creates IIS sites: folder, application pool and HTTP binding, checked by a request to
  the site and removed again if a step fails.
- **Terminal UI**: in an interactive terminal, `kelpy` shows the terminal UI in place of the start-up lines: a start
  screen with Kelpy's mark and the recent conversations, the answers as they are written with each action on one line,
  the actions with Ctrl+Y and Ctrl+N to approve or deny, the conversation's modes (Shift+Tab, F2 to F4), and from 120
  columns a side panel with the conversation's state and how much of the model's context it takes (Ctrl+B). Over SSH
  it draws inline: the conversation goes into the terminal's own history and only a footer is redrawn (`--inline`,
  `--fullscreen`). Its colours follow the terminal's background (`--theme dark|light` where the terminal does not
  tell). It stays in step with the Web UI. Closing it stops Kelpy, asking first while an
  answer is being written. `kelpy tui` opens it for a Kelpy already running, which goes on when it is closed.
  `--no-browser` keeps the browser closed. The first run no longer asks which interface to use.
- **Files**: *Files*, in the top bar, uploads files to the server for Kelpy to use (up to 2 GiB each), lists them,
  and downloads and deletes them; the paperclip in the message box, or a file dropped on it, uploads it and writes
  its path in the message. Uploads go to `workspace/files/`, readable by Kelpy's account only.
- **Secrets**: *Secrets*, in the top bar, lists every secret by name and copies its value on request. Secrets you add
  serve every conversation by placeholder, as `{{secret:name}}`; the model is told their names, never their values.
- **Vault**: secrets can be kept across restarts in `data/vault.json`, encrypted with XChaCha20-Poly1305. The vault
  opens with a passphrase (Argon2id) or with an Ed25519 or RSA key of the SSH agent of Kelpy's session, which unlocks
  it when Kelpy starts; it has as many ways to unlock it as you add. The banner shows a `Vault:` line.
- **Unexpected changes** is the new name of what was *Changed outside Kelpy*: the bar above the conversation, and
  its section in the handover report and in the memory page `server.md`.
- **Web UI**: the Approval menu describes Plan and Auto as each works; an action card offers *Switch to Plan* and
  *Switch to Auto* only when they would change the conversation's mode; the notes of a change show its commands as
  code; actions shorter than a tenth of a second read `< 0.1 s`.

## 0.3.0 (2026-11-06)

Kelpy now runs on macOS and on Linux ARM64.

- **Platforms**: new executables for Linux ARM64 (`kelpy-linux-aarch64`, static like the x86-64 one), macOS on
  Apple silicon (`kelpy-macos-aarch64`) and macOS on Intel (`kelpy-macos-x86_64`). The install command
  `curl -fsSL https://kelpy.io/install.sh | sh` works on macOS too. The macOS executables are not signed by Apple:
  download them with `curl`, as a file downloaded with a browser is blocked by Gatekeeper.
- **macOS changes that always ask**: changes to the SSH configuration, the sudo rules and the other files of the
  floor now always ask on macOS too, where `/etc` is a link to `/private/etc`. The packet filter (`/etc/pf.conf`,
  `/etc/pf.anchors`), the application firewall and the network settings in `/Library/Preferences` are added; a
  change to the packet filter rules is checked with `pfctl -nf /etc/pf.conf`.
- **Updates**:
  - The signature now covers the version: a release lists `VERSION` in `SHA256SUMS`, and Kelpy refuses an
    update whose `VERSION` is not the version it asked for.
  - While an update replaces the executable, Kelpy cannot start in the same home, and `kelpy cleanup` waits.
  - The new executable is prepared in the home's `data` folder; no other file next to the executable is
    created, overwritten or deleted.
  - On Windows, if the new executable cannot be put in place, the old one stays.
  - Downloading and checking an update must end within 15 minutes.
- **`kelpy cleanup`** keeps any runtime or update from starting until it has finished, and deletes a file outside
  Kelpy's folders only if it is a Kelpy executable.
- **Releases**: once published, each release is installed with the install command, and updated to from the
  previous release, on clean Ubuntu, macOS and Windows Server machines.

Updating from 0.2.0: Kelpy offers the update at its next start, or run `kelpy update`.

## 0.2.0 (2026-11-05)

Kelpy checks for new versions and installs them, and releases are signed.

- At start Kelpy checks for a new release, for at most 3 seconds, and asks whether to install it. The new version
  then starts in its place. A declined version is not offered again.
- `kelpy update` installs the latest release on request; `--yes` installs without asking.
- To turn the check off, set `check = false` in the `[updates]` section of `config.toml`.
- `SHA256SUMS` is signed (`SHA256SUMS.sig`). Kelpy installs an update only with a valid signature and a matching
  checksum. To check a release by hand, see the
  [wiki](https://github.com/Ryadel/Kelpy/wiki/Installation-and-first-run#download-by-hand).

Updating from 0.1.0: run the install command again.

## 0.1.0 (2026-11-05)

The first release: one executable for Linux x86-64 and Windows x86-64.

- **Install**: `curl -fsSL https://kelpy.io/install.sh | sh` on Linux, `irm https://kelpy.io/install.ps1 | iex` in
  PowerShell, or download the file for your platform and run it. The first run creates the Kelpy home and can add
  Kelpy to the PATH.
- **AI services**: chat in the Web UI with OpenAI, ChatGPT (sign-in with your plan), Anthropic, any
  OpenAI-compatible service, or local models through Ollama.
- **System access** (None, Read-only, Full) and **approval mode** (Ask, Plan, Auto), with every command in the
  Command log.
- **Changes** to files, services, apt packages and databases shown as a diff, backed up, checked, and restorable.
- **Memory**: a memory of the server, the changes made outside Kelpy, and a handover report.
