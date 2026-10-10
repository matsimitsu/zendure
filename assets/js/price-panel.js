// <price-panel>: hover/tap readout for the server-rendered price panel.
//
// Light DOM and no framework, like <energy-flows>: the SSE stream replaces
// the host's innerHTML every tick, but the host persists, so listeners and
// the hovered slot live on it and are re-applied to each fresh fragment.
// All formatting happens in Rust; this only copies strings and moves the
// highlight.
const B = "price-panel";

class PricePanel extends HTMLElement {
  connectedCallback() {
    // The element can be re-attached; listeners must be added only once.
    if (this.bound) return;
    this.bound = true;
    // The hovered/tapped hit's slot: its hour's place in the day, which a
    // refetch filling a gap never shifts the way it shifts positions.
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
    return e.target.closest?.(`.${B}__hit`)?.dataset.slot ?? null;
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
    const root = this.querySelector(`.${B}__readout`);
    if (!root) return;
    // A remembered slot that is no longer priced falls back to the default.
    const hit =
      this.hover !== null
        ? this.querySelector(`.${B}__hit[data-slot="${this.hover}"]`)
        : null;
    const d = root.dataset;
    const label = hit ? hit.dataset.label : d.defaultLabel;
    const value = hit ? hit.dataset.value : d.defaultValue;
    const tier = hit ? hit.dataset.tier : d.defaultTier;
    const tierLabel = hit ? hit.dataset.tierLabel : d.defaultTierLabel;

    const set = (sel, text) => {
      const el = root.querySelector(sel);
      if (el) el.textContent = text;
    };
    set(`.${B}__read-label`, label);
    set(`.${B}__read-value`, value);

    const pill = root.querySelector(".price-tier");
    if (pill) {
      for (const c of [...pill.classList]) {
        if (c.startsWith("price-tier--")) pill.classList.remove(c);
      }
      pill.hidden = !tier;
      pill.textContent = tier ? tierLabel : "";
      if (tier) pill.classList.add(`price-tier--${tier}`);
    }

    for (const h of this.querySelectorAll(`.${B}__highlight`)) {
      h.setAttribute("x", hit ? hit.getAttribute("x") : h.dataset.defaultX);
      h.setAttribute("width", hit ? hit.getAttribute("width") : h.dataset.defaultWidth);
    }
  }
}

customElements.define("price-panel", PricePanel);
