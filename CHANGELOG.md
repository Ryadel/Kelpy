# Changelog

Each release is on the [releases page](https://github.com/Ryadel/Kelpy/releases), with one executable per platform,
the install scripts and the signed checksums. Versions follow [Semantic Versioning](https://semver.org).

## 0.3.0 (2026-11-06)

Kelpy now runs on macOS and on Linux ARM64.

- **Platforms**: new executables for Linux ARM64 (`kelpy-linux-aarch64`, static like the x86-64 one), macOS on
  Apple silicon (`kelpy-macos-aarch64`) and macOS on Intel (`kelpy-macos-x86_64`). The install command
  `curl -fsSL https://kelpy.io/install.sh | sh` works on macOS too. The macOS executables are not signed by Apple
  yet: download them with `curl`, as a file downloaded with a browser is blocked by Gatekeeper.
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
