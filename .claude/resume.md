# Resume: gitterm-v2

**Last checkpoint:** 2026-02-27 00:15

## You Were Just Working On
Building the Quick Commands feature — app-level saved commands that run in the bottom terminal. Code compiles but hasn't been tested yet.

**Just did:** Got Quick Commands compiling. Need to test it by adding commands to config.json and trying the ⚡ button in the bottom panel.

**Immediate next step:** Test the Quick Commands feature:
1. Add test commands to `~/.config/gitterm/config.json` under `"quick_commands"` array
2. Run the app, click ⚡ in bottom panel header, verify commands run in bottom terminal
3. Commit if working, debug if not

## Completed This Session
- Switched gh auth to traceyt (personal)
- Investigated bottom terminal focus bug — couldn't reproduce, documented likely causes
- Researched ACP/Toad — decided terminal-hosting approach is right for GitTerm
- Built data-driven agent presets (AgentPreset struct, dynamic picker, config.json persistence)
- Default presets: Claude Code, Codex, Gemini CLI (all with resume commands)
- Committed (285b4dd), pushed, bundled for Applications
- Updated README: Starship setup (cross-platform), agent presets docs, keyboard shortcuts
- Fixed CI build: dropped `--features stt`, added Linux deps (libsoup, webkit, xdo)
- Dropped x86_64 macOS target — Apple Silicon only
- All 3 CI platforms now green (macOS aarch64, Windows, Linux)
- Built Quick Commands feature (in progress, compiles, untested):
  - `QuickCommand` struct in config.json (name + command)
  - ⚡ button in bottom panel header shows picker
  - Commands typed into active bottom terminal (creates one if needed)
  - Escape to dismiss picker

## Key Files
- `src/main.rs` — QuickCommand struct (~line 219), RunQuickCommand handler (~line 4896), view_quick_commands_picker (~line 10530), ⚡ button (~line 10860)
- `~/.config/gitterm/config.json` — `quick_commands` array
- `.github/workflows/build.yml` — CI now green on all platforms

## Blockers/Issues
- Quick Commands feature needs testing — add commands to config.json and verify
- Bottom terminal focus bug: intermittent, couldn't reproduce
- Still need `default_agent` config field for setting preferred default agent
