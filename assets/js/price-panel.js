// <price-panel>: hover/tap readout for the server-rendered price panel.
//
// Light DOM and no framework, like <energy-flows>: the SSE stream replaces
// the host's innerHTML every tick, but the host persists, so listeners and
// the hovered index live on it and are re-applied to each fresh fragment.
// All formatting happens in Rust; this only copies strings and moves the
// highlight.
const B = "price-panel";

class PricePanel extends HTMLElement {
  connectedCallback() {
    // The element can be re-attached; listeners must be added only once.
    if (this.bound) return;
    this.bound = true;
    this.hover = null; // index of the hovered/tapped hit rect

    this.addEventListener("pointerover", (e) => {
      if (e.pointerType !== "mouse") return; // touch/pen reach us via click
      this.setHover(this.indexOf(e));
    });
    // Touch and pen have no hover; the tap is the only signal they give.
    this.addEventListener("click", (e) => {
      const i = this.indexOf(e);
      if (i !== null) this.setHover(i);
    });
    // `pointerleave` does not bubble, so moving between bars keeps the readout.
    this.addEventListener("pointerleave", (e) => {
      if (e.pointerType === "mouse") this.setHover(null);
    });
    // afterSwap avoids a flash of the server's default readout; afterSettle
    // is a backstop in case the settle phase touches the markup again.
    this.addEventListener("htmx:afterSwap", () => this.apply());
    this.addEventListener("htmx:afterSettle", () => this.apply());
    // The stream only ever carries today; it must not overwrite another day.
    this.addEventListener("htmx:sseBeforeMessage", (e) => {
      if (this.dataset.day && this.dataset.day !== "today") e.preventDefault();
    });
    this.apply();
  }

  hits() {
    return [...this.querySelectorAll(`.${B}__hit`)];
  }

  indexOf(e) {
    const hit = e.target.closest?.(`.${B}__hit`);
    return hit ? this.hits().indexOf(hit) : null;
  }

  // The fragment says which day it shows; the host keeps it across swaps.
  mirrorDay() {
    const marker = this.querySelector("[data-day]");
    if (!marker) return;
    if (this.dataset.day !== marker.dataset.day) {
      // Indexes from another day point at unrelated hours.
      this.hover = null;
    }
    this.dataset.day = marker.dataset.day;
  }

  setHover(i) {
    this.hover = i;
    this.apply();
  }

  // Idempotent: derives everything from host state, so it is safe to call
  // after any swap or interaction.
  apply() {
    this.mirrorDay();
    const root = this.querySelector(`.${B}__readout`);
    if (!root) return;
    // A remembered index that no longer exists falls back to the default.
    const hit = this.hover !== null ? this.hits()[this.hover] : undefined;
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
      h.setAttribute("x", hit ? hit.getAttribute("x") : "0");
      h.setAttribute("width", hit ? hit.getAttribute("width") : "0");
    }
  }
}

customElements.define("price-panel", PricePanel);
