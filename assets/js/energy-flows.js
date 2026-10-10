// <energy-flows>: interactivity for the server-rendered energy flows panel.
//
// Light DOM and no framework on purpose: the server owns the markup and the
// SSE stream replaces the host's innerHTML on every tick. The host element
// itself persists, so all state lives on it (as classes and fields) and is
// re-applied to the fresh markup after each swap.
const SERIES = ["solar", "home", "grid", "battery"];
const B = "energy-flows";

class EnergyFlows extends HTMLElement {
  connectedCallback() {
    // The element can be re-attached; listeners must be added only once.
    if (this.bound) return;
    this.bound = true;
    this.hover = null; // index of the hovered column in the visible plot
    this.pinned = null; // index pinned by a touch/pen tap

    this.addEventListener("click", (e) => this.onClick(e));
    this.addEventListener("pointerover", (e) => this.onOver(e));
    this.addEventListener("pointerdown", (e) => this.onDown(e));
    // `pointerleave` does not bubble, which is what we want on the host:
    // moving between columns must not flicker the readout back to default.
    this.addEventListener("pointerleave", (e) => {
      if (e.pointerType === "mouse") this.setHover(null);
    });
    // An outside tap unpins; the document sees taps the host never does.
    document.addEventListener("pointerdown", (e) => {
      if (this.pinned !== null && !e.target.closest?.(`.${B}__hit`)) this.setPinned(null);
    });
    // afterSwap avoids a flash of the server's default readout; afterSettle
    // is a backstop in case the settle phase touches the markup again.
    this.addEventListener("htmx:afterSwap", () => this.apply());
    this.addEventListener("htmx:afterSettle", () => this.apply());
    // The stream only ever carries today; it must not overwrite a past day.
    this.addEventListener("htmx:sseBeforeMessage", (e) => {
      if (this.dataset.live === "false") e.preventDefault();
    });
    // Step links carry only the day: the interval is host state, which the
    // server could not know when it rendered them.
    this.addEventListener("htmx:configRequest", (e) => {
      if (e.detail.path?.startsWith("/fragments/energy-flows")) {
        e.detail.parameters.interval = this.interval;
      }
    });
    this.apply();
  }

  // The fragment says which day it shows; the host keeps it across swaps.
  mirrorDay() {
    const nav = this.querySelector(".day-nav");
    if (!nav) return;
    if (this.dataset.day !== nav.dataset.day) {
      // Column indexes from another day point at unrelated intervals.
      this.hover = this.pinned = null;
    }
    this.dataset.day = nav.dataset.day;
    this.dataset.live = nav.dataset.live;
  }

  get interval() {
    return this.classList.contains(`${B}--15m`) ? "15m" : "1h";
  }

  plot() {
    return this.querySelector(`.${B}__plot--${this.interval}`);
  }

  hits() {
    return [...(this.plot()?.querySelectorAll(`.${B}__hit`) ?? [])];
  }

  onClick(e) {
    const segment = e.target.closest(`.${B}__segment`);
    if (segment) {
      this.classList.toggle(`${B}--15m`, segment.dataset.interval === "15m");
      // Column indexes mean something different in the other plot.
      this.hover = this.pinned = null;
      return this.apply();
    }
    const item = e.target.closest(`.${B}__legend-item`);
    if (item) {
      this.classList.toggle(`${B}--hide-${item.dataset.series}`);
      this.apply();
    }
  }

  onOver(e) {
    if (e.pointerType !== "mouse") return; // touch/pen pin on pointerdown
    const hit = e.target.closest(`.${B}__hit`);
    this.setHover(hit ? this.hits().indexOf(hit) : null);
  }

  onDown(e) {
    if (e.pointerType === "mouse") return;
    const hit = e.target.closest(`.${B}__hit`);
    if (hit) this.setPinned(this.hits().indexOf(hit));
  }

  setHover(i) {
    this.hover = i;
    this.apply();
  }

  setPinned(i) {
    this.pinned = i;
    this.apply();
  }

  // Idempotent: derives everything from host state, so it is safe to call
  // after any swap or interaction.
  apply() {
    this.mirrorDay();
    const interval = this.interval;
    for (const s of this.querySelectorAll(`.${B}__segment`)) {
      s.setAttribute("aria-pressed", String(s.dataset.interval === interval));
    }
    for (const item of this.querySelectorAll(`.${B}__legend-item`)) {
      const hidden = this.classList.contains(`${B}--hide-${item.dataset.series}`);
      item.setAttribute("aria-pressed", String(!hidden));
    }

    // The server's readout is kept before anything overwrites it: it is the
    // only fallback when no interval has completed (just after midnight, or
    // a past day with no history).
    const readouts = this.querySelectorAll(`.${B}__readout-time, .${B}__readout-value`);
    for (const out of readouts) out.dataset.default ??= out.textContent;

    const hits = this.hits();
    const index = this.hover ?? this.pinned;
    // Falls back to the latest completed interval when nothing is hovered,
    // or when the remembered index no longer exists after a swap.
    const active = index !== null && hits[index] !== undefined;
    const shown = active ? hits[index] : hits.find((h) => h.classList.contains(`${B}__hit--latest`));

    const time = this.querySelector(`.${B}__readout-time`);
    if (time) time.textContent = shown ? `${shown.dataset.label} · kW` : time.dataset.default;
    for (const s of SERIES) {
      const out = this.querySelector(`.${B}__readout-value[data-series="${s}"]`);
      if (out) out.textContent = shown ? shown.dataset[s] : out.dataset.default;
    }

    // Only a deliberate hover or pin is highlighted, not the default column;
    // every plot is reset so the hidden one holds no stale column either.
    for (const highlight of this.querySelectorAll(`.${B}__highlight`)) {
      const on = active && highlight.closest(`.${B}__plot`) === this.plot();
      highlight.setAttribute("x", on ? shown.getAttribute("x") : "0");
      highlight.setAttribute("width", on ? shown.getAttribute("width") : "0");
    }
  }
}

customElements.define("energy-flows", EnergyFlows);
