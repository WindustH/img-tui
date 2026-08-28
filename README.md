# img-tui

Terminal image capability detection and image protocol rendering helpers for ratatui applications.

Zellij 0.45+ Kitty graphics support is detected with the standard KGP query.
When Zellij and its host terminal confirm support, Kitty is preferred without
using unsupported Kitty Unicode placeholders; otherwise callers retain their
Sixel and text fallbacks.

This crate was split out of `gallery-tui` so other TUI applications can reuse the same terminal image probing, protocol overlay tracking, and native image rendering code.
