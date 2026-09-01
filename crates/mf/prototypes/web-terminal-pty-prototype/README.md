# PROTOTYPE — Real Agent PTY in Web

Throwaway technical prototype for Wayfinder issue #7. It validates a browser
xterm surface against real local Codex and Claude Code processes through a
temporary Node/node-pty bridge.

This is **not** the production Rust gateway. It intentionally has no durable
persistence, API-key management, Root Mode, or remote access.

Install once:

```powershell
pnpm install
```

Run from this directory:

```powershell
pnpm prototype
```

Open <http://127.0.0.1:4187/>.

The production design will replace `server.mjs` with Rust while preserving the
validated terminal interaction and protocol semantics.
