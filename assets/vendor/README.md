# Vendored dashboard scripts

Served from `/assets/` so the dashboard stays live on a LAN with no internet
access. `build.rs` copies every `.js` here into `OUT_DIR`, next to the
compiled `dashboard.css`.

| File          | Package                | Version | Source                                                          |
| ------------- | ---------------------- | ------- | --------------------------------------------------------------- |
| `htmx.min.js` | htmx                   | 2.0.4   | `https://cdnjs.cloudflare.com/ajax/libs/htmx/2.0.4/htmx.min.js`  |
| `sse.js`      | htmx-ext-sse           | 2.2.4   | `https://cdn.jsdelivr.net/npm/htmx-ext-sse@2.2.4/sse.js`         |

To upgrade, re-download from the source URL with the new version and update
the table.
