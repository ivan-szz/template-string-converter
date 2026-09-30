# Template String Converter

A Zed extension that automatically converts string quotes to backticks when a template expression (`${...}`) is typed inside a JavaScript or TypeScript string.

## Example

```typescript
// Type this:
const greeting = "hello ${name}"

// Automatically becomes:
const greeting = `hello ${name}`
```

## Installation

Search for "Template String Converter" in Zed's extension page. The extension downloads the prebuilt language server for your platform from this repository's GitHub releases (macOS arm64/x64, Linux arm64/x64, Windows x64) and keeps it up to date.

If a `template-string-converter-lsp` binary is on your `PATH`, it is used instead of the download. That is also the way to go on other platforms:

```bash
cargo install --git https://github.com/ivan-szz/template-string-converter template-string-converter-lsp
```

Building requires a C compiler (the tree-sitter grammars are compiled from C).

### Development

1. Clone this repo.
2. Build and install the LSP binary with `cargo install --path lsp-server`, or build it with `cargo build --release` inside `lsp-server/` and put `lsp-server/target/release/` on your `PATH`.
3. In Zed: Cmd+Shift+P → "Install Dev Extension" → select the cloned directory.

> **Note:** After rebuilding the LSP binary, restart the language server in Zed (Cmd+Shift+P → "editor: restart language server") so it picks up the new build. On Windows the binary may be locked while Zed is running it.

### Releasing the language server

Bump `version` in `lsp-server/Cargo.toml`, commit, then push a matching tag (e.g. `v0.0.3`). The `Release LSP` workflow builds the binary for every supported platform and publishes a GitHub release with the archives the extension downloads.

## How it works

The extension runs a lightweight LSP server that watches for `${` typed inside `"` or `'` strings. When detected, it sends an edit that:

1. Replaces both surrounding quotes with backticks, turning the string into a template literal (backticks already inside the string are escaped).
2. Inserts the closing `}` when you typed the `{` and it isn't already followed by one. Typing `$` in front of an existing `{name}` converts the string without adding a brace.

The server uses incremental document sync and only reacts to short typed insertions, so pasting code, reloading a file or undoing a conversion never triggers it. Whether the `$` really sits inside a string literal is decided with a [tree-sitter](https://tree-sitter.github.io/) parse: `${` in JSX text, comments, escaped (`\${`), in JSX attributes or import paths is left alone.

Edits are sent with the document version when the client supports it, so an edit computed against stale text is rejected instead of landing at the wrong offset.

The server inserts the `}` itself rather than relying on Zed's auto-pairing, because Zed only auto-closes `{` when the following character is in `autoclose_before` (whitespace, quotes, brackets) — never before a word character.

Set `TEMPLATE_STRING_CONVERTER_LOG=1` in the environment to get debug logs on stderr.

## Supported languages

- JavaScript
- TypeScript
- JSX
- TSX

## License

Apache 2.0
