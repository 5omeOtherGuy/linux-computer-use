# Implementation boundary and references

Research and local inspection were performed September 5–6, 2026. Public documentation describes capabilities and integration patterns; it does not disclose all ChatGPT desktop internals. This project does not claim to reproduce proprietary orchestration or product UI.

The installed ChatGPT 26.901.41600 contains `@oai/sky` 0.6.26 with a Linux X11 binary, and `@oai/cua` 0.2.4. Direct local tests verified screenshots and pointer movement through Sky. The exposed high-level native app API in this session is unavailable on Linux. The bundled code's existence does not imply that its complete native app interface is enabled here.

This project supplies the missing agent-facing route as an MCP server. It uses a persistent worker, accessibility plus screenshot observations, app-scoped targets, batched Python actions, structured failures and independent cancellation/recovery. XTEST supplies physical input, AT-SPI supplies semantic UI operations, and Pillow supplies in-memory screenshots. This makes tools available to Codex without modifying OpenAI's bundled plugin or the shared skill catalog.

Primary references checked during the work:

- [OpenAI Codex MCP documentation](https://developers.openai.com/codex/mcp): local stdio server registration and tool integration.
- [OpenAI computer-use integration guide](https://developers.openai.com/api/docs/guides/tools-computer-use-integration): persistent execution and integration patterns.
- [OpenAI computer-use guide](https://developers.openai.com/api/docs/guides/tools-computer-use): action/observation loop and screenshot handling.
- [ChatGPT computer-use documentation](https://learn.chatgpt.com/docs/computer-use): publicly documented product capabilities and supported desktop environments.
- [OpenAI computer-use sample](https://github.com/openai/openai-cua-sample-app): public reference implementation, inspected at commit `f2a3dc523ae406f9b704f9a420a05402a63b4522`; no sample source is vendored here.
- [MCP Python SDK](https://github.com/modelcontextprotocol/python-sdk): official transport, initialization instructions, structured tool output and image content. This project pins SDK 1.29.1 instead of relying on a moving major version.
- [Python Xlib documentation](https://python-xlib.github.io/): X11 input and window primitives.
- [xdotool typing implementation](https://github.com/jordansissel/xdotool/blob/main/cmd_type.c): its 12 ms default helped inform pacing for temporary Unicode mappings after live tests exposed app-side event timing races.
- [AT-SPI EventListener](https://gnome.pages.gitlab.gnome.org/at-spi2-core/libatspi/class.EventListener.html): accessibility event subscription.
- [XDG RemoteDesktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html): relevant to a future native Wayland backend; not implemented here.

Native Wayland requires a compositor/portal input and capture path, session consent handling, coordinate mapping and separate acceptance tests. It cannot be honestly marked supported just by detecting a Wayland environment or an XWayland display. Full product parity also requires validating app coverage and implementing user takeover/visibility controls in the host application. Those host capabilities cannot be added merely by registering an MCP server.
