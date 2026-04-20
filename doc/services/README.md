# Always-on daemon: systemd / launchd unit examples

By default the bridge auto-spawns a per-project daemon on first
contact and the daemon idle-exits 30 minutes after the last bridge
disconnects. That's the right shape for most users — no resident
process when no agent is running.

If you'd rather keep one daemon resident per project (or one global
daemon for an always-loaded project root), drop one of the units
below into your user-level service manager.

## Linux: systemd user service

Copy `mcp-cli-daemon.service` into `~/.config/systemd/user/`, edit
the `Environment=MCP_CLI_ROOT=...` line to point at the project you
want served, then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now mcp-cli-daemon.service
journalctl --user -u mcp-cli-daemon -f
```

The unit:
* runs `mcp-cli-daemon` with `--idle-timeout 0` (never auto-exit),
* sets `--root` from the `MCP_CLI_ROOT` environment variable,
* uses the protocol-derived per-cwd socket path (the bridge will
  find it without configuration as long as the agent's CWD matches
  `MCP_CLI_ROOT`),
* restarts on failure with a 5-second backoff.

For one daemon per project, copy the unit per project and template
its name (`mcp-cli-daemon@<slug>.service`) — systemd handles the
fan-out via `%i`.

## macOS: launchd user agent

Copy `mcp-cli-daemon.plist` into `~/Library/LaunchAgents/` (rename
to e.g. `app.mcp-cli.daemon.plist` if you prefer), edit the
`MCP_CLI_ROOT` value, then:

```sh
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/app.mcp-cli.daemon.plist
launchctl print gui/$UID/app.mcp-cli.daemon
log stream --predicate 'subsystem == "app.mcp-cli.daemon"' --level info
```

The plist runs `mcp-cli-daemon` with `--idle-timeout 0`, redirects
stdout/stderr to `~/Library/Logs/mcp-cli-daemon.log`, and asks
launchd to keep the process alive (`KeepAlive = true`).

## When you might NOT want this

Demand-spawn (the default) is usually the right call:
* It's per-project — the daemon's parse cache, search cache, and
  watch state are scoped to one tree, so memory doesn't grow with
  the number of projects you touch.
* The daemon goes away when you stop using it.
* The bridge auto-respawns it on next use anyway.

The always-on shape only earns its keep when:
* You hit the same project all day and want zero spawn latency
  even on the very first call after machine reboot.
* You want long-lived metrics counters (`metrics.gain` /
  `metrics.tool_latency`) accumulated across agent sessions.
* You're integrating a non-MCP consumer that wouldn't trigger the
  bridge's auto-spawn path.
