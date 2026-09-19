# Security

This is a hobby emulator. It reads a ROM file you point it at, writes a save file next to that ROM, and makes no network connections. The browser build runs entirely in your tab and uploads nothing.

If you find a way a crafted ROM or save file can do something beyond crashing the emulator (write outside the ROM's folder, run code on the host, escape the WebAssembly sandbox), please report it privately through GitHub's [private vulnerability reporting](https://github.com/x0cero/gba/security/advisories/new) rather than a public issue. Plain crashes, hangs and rendering bugs are fine as normal issues.

Only the latest release gets fixes.
