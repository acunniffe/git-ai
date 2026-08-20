# Git AI packaging

This directory contains MSI and PKG installer scaffolding for Git AI.

Package outputs must install `git-ai` only. They must not install a `git`
wrapper, `git.exe` shim, `git-og`, or any other executable that changes Git
command routing. Per-user trace2 and editor/agent setup remains the
responsibility of `git-ai install-hooks`, which both installers invoke with
`--env` to also apply the per-user shell/PATH configuration.

The release workflow builds signed/notarized production packages when the
required Apple and Azure signing secrets are configured. Dry-run releases can
build unsigned packages for validation. macOS releases include Intel, Apple
Silicon, and universal PKGs.

The Windows MSI is per-user: it installs under
`%USERPROFILE%\\.git-ai\\bin` and changes only that user's `PATH` (the MSI
owns the user PATH entry and removes it on uninstall; `install-hooks --env`
additionally configures Git Bash profiles when Git Bash is installed). It has
no all-users or Administrator install mode. The macOS PKG copies its bundled
binary into the active console user's `~/.git-ai/bin`, then runs setup as that
user, including adding `~/.git-ai/bin` to the user's shell PATH configs. It
fails if no valid console user is logged in or per-user setup fails.

For an enterprise endpoint, pass configuration to the MSI when installing:

```powershell
msiexec /i git-ai-windows-x64.msi API_BASE=https://usegitai.com API_KEY=your-api-key
```

These values configure only the installing user's Git AI config. They are
hidden from MSI logs, but command-line arguments can still be visible to local
process inspection and shell history. Use your endpoint-management secret
mechanism when available.
