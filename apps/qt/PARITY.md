# Qt / Swift app contract

The reference is `apps/macos` at repository revision `b199e6f`. The Qt app uses
Rust for application state and I/O, and native Qt Quick for rendering. Its light
layout was compared against the running Swift app, including Home, the compact
sidebar/list, and the continuous conversation reader. The sidebar background
extends through the toolbar. This is a behavior and layout port, not an AppKit
implementation or a claim of identical platform rendering.

| Area | Qt implementation | Coverage |
| --- | --- | --- |
| Home | Activity, sessions/tokens, 24H/7D/30D/all, search and recent conversations | Full-window fixture and chart inspection |
| Browser | Projects, sorting, full-index counts, machine picker, independent machine results, incremental lists | Rust state and full-window tests |
| Filters | Project, provider, timeframe, chats/subagents, explicit permission-review inclusion | Rust state and full-window tests |
| Persistence | Browser filters/project summaries, window geometry, chosen resume destination | Rust state tests and Qt Settings |
| Transport | Private Unix socket handshake, owner/permissions/root/protocol checks, CLI fallback, deadlines and cancellation | Rust client tests with isolated sockets/processes |
| Reader | Newest-page opening, search anchors, paging in both directions, 20-session reading-state cache | Rust state, native reader and full-window tests |
| Find | Complete conversation scan, literal occurrences, wrapping navigation, hidden source expansion | Rust state, native reader and full-window tests |
| Presentation | Injected context, logical tools/results, explicit completion grouping, Markdown and code, bounded large content, full-source copy/raw | Native reader fixture suite |
| Attachments | Typed references, local/embedded images, explicit external URLs, remote path guards | Native reader and platform tests |
| Source | Local read-only UTF-8 previews with UTF-16 line selection, reveal source | Platform and full-window tests |
| Resume | CLI command/cwd, installed terminal destinations, local Codex ChatGPT links on macOS, remote guard | Platform unit tests; actual external app launches are not automated |

## Validation environment and limits

The Docker image qualifies Linux ARM64 with Fedora 44, Qt 6.11.2 and Rust 1.98.1.
Qt Bridge's published Linux target is x86_64; that architecture has not been run
in this task. Platform adapter unit tests also ran on macOS, but the complete Qt
GUI was not built on macOS because a local Qt development installation was not
available. Windows support is not implemented.

Screenshots and full-window tests use synthetic session data and an isolated CLI
fixture. They do not establish performance or provider coverage on a large real
index. The transport tests exercise the app protocol and failure contracts with
isolated socket fixtures; they do not change or restart the user's daemon.

The executable embeds its QML/JavaScript resources. Outside the Linux AppImage,
Qt shared libraries and QML runtime modules remain external dependencies; the
release AppImage vendors them plus the Memex CLI. Signed/notarized macOS
bundles, native Linux distribution packages, and an installer are separate
from this source-level app port.
