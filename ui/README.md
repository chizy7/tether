# ui

`index.html` is the whole trace viewer: one page, plain JavaScript, no build
step and nothing fetched from anywhere else. It is built into the `tether`
binary and served by `tether ui <run> [<run-b>]`; the server and its JSON
endpoints are in `crates/tether-cli/src/ui.rs`.

To work on it, edit the file, rebuild (`cargo build -p tether-cli`) and
reload. The page is compiled in, so a change shows only after a rebuild.

Two rules it must keep:

- Everything from a trace goes on the page as text (`textContent`), never as
  markup. A trace holds whatever a model or a tool said.
- One `<style>` and one `<script>`, each carrying `nonce="__NONCE__"`. The
  server replaces that with a fresh value and allows nothing else to run, so
  no inline `style=` or `onclick=` attributes and no second script.
