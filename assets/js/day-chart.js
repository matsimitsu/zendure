// DayChart: the client behaviour every server-rendered day panel shares.
//
// Light DOM and no framework, like <energy-flows>: the SSE stream replaces
// the host's innerHTML every tick, but the host persists, so listeners and
// the hovered slot live on it and are re-applied to each fresh fragment.
// All formatting happens in Rust; subclasses only say which strings their
// readout shows.
//
// Published on `globalThis` because each app script is a module with its own
// scope and the page's load check imports them as standalone data: URLs, so an
// `import` between them cannot resolve. Module scripts run in document order
// and `build.rs` sorts them by name, so a subclass file must sort after
// `day-chart.js`.
const DAY_CHART = "day-chart";

class DayChart extends HTMLElement {
  connectedCallback() {
    // The element can be re-attached; listeners must be added only once.
    if (this.bound) return;
    this.bound = true;
    // The hovered/tapped hit's slot: its place in the day, which a refetch
    // filling a gap never shifts the way it shifts positions.
    this.hover = null;

    this.addEventListener("pointerover", (e) => {
      if (e.pointerType !== "mouse") return; // touch/pen reach us via click
      this.setHover(this.slotOf(e));
    });
    // Touch and pen have no hover; the tap is the only signal they give.
    this.addEventListener("click", (e) => {
      const slot = this.slotOf(e);
      if (slot !== null) this.setHover(slot);
    });
    // `pointerleave` does not bubble, so moving between bars keeps the readout.
    this.addEventListener("pointerleave", (e) => {
      if (e.pointerType === "mouse") this.setHover(null);
    });
    // afterSwap avoids a flash of the server's default readout; afterSettle
    // is a backstop in case the settle phase touches the markup again.
    this.addEventListener("htmx:afterSwap", () => this.apply());
    this.addEventListener("htmx:afterSettle", () => this.apply());
    // The stream only ever carries today. A panel on another day takes it
    // only once that day has become today, so one left on tomorrow goes
    // live at midnight instead of freezing; the server's day boundary, not
    // the browser's, decides.
    this.addEventListener("htmx:sseBeforeMessage", (e) => {
      if (this.dataset.live === "true") return;
      const day = /data-day="([^"]+)"/.exec(e.detail?.data ?? "")?.[1];
      if (day !== this.dataset.day) e.preventDefault();
    });
    this.apply();
  }

  slotOf(e) {
    return e.target.closest?.(`.${DAY_CHART}__hit`)?.dataset.slot ?? null;
  }

  // The hit rect for the remembered slot, or null once that slot is gone, so
  // the readout falls back to the server's default.
  hoveredHit() {
    if (this.hover === null) return null;
    return this.querySelector(`.${DAY_CHART}__hit[data-slot="${this.hover}"]`);
  }

  // The fragment says which day it shows and whether it is today; the host
  // keeps both across swaps.
  mirrorDay() {
    const marker = this.querySelector("[data-live]");
    if (!marker) return;
    if (this.dataset.day !== marker.dataset.day) {
      // Slots from another day point at unrelated hours.
      this.hover = null;
    }
    if (marker.dataset.day) this.dataset.day = marker.dataset.day;
    else delete this.dataset.day;
    this.dataset.live = marker.dataset.live;
  }

  setHover(slot) {
    this.hover = slot;
    this.apply();
  }

  // Idempotent: derives everything from host state, so it is safe to call
  // after any swap or interaction.
  apply() {
    this.mirrorDay();
    const readout = this.querySelector(`.${DAY_CHART}__readout`);
    if (!readout) return;
    const hit = this.hoveredHit();
    // The hit's `data-<key>`, or the readout's `data-default-<key>` with
    // nothing hovered.
    const read = (key) =>
      hit ? hit.dataset[key] : readout.dataset[`default${key[0].toUpperCase()}${key.slice(1)}`];
    this.setText(readout, `.${DAY_CHART}__read-label`, read("label"));
    this.showReadout(readout, read);
    this.moveHighlight(hit);
  }

  // Subclasses copy their figures into `readout`, reading each by its
  // data key through `read`.
  showReadout(_readout, _read) {
    throw new Error("DayChart subclasses must define showReadout");
  }

  setText(root, selector, text) {
    const el = root.querySelector(selector);
    if (el) el.textContent = text ?? "";
  }

  // Onto the hovered hit, or back to where the server left it.
  moveHighlight(hit) {
    for (const h of this.querySelectorAll(`.${DAY_CHART}__highlight`)) {
      h.setAttribute("x", hit ? hit.getAttribute("x") : h.dataset.defaultX);
      h.setAttribute("width", hit ? hit.getAttribute("width") : h.dataset.defaultWidth);
    }
  }
}

globalThis.DayChart = DayChart;
