// <price-panel>: hover/tap readout for the server-rendered price panel.
//
// The hover, pin and SSE behaviour is shared with the other day panels in
// DayChart (day-chart.js); this only knows which strings the price readout
// shows and where its highlight sits.
const B = "price-panel";

class PricePanel extends DayChart {
  get hitSelector() {
    return `.${B}__hit`;
  }

  apply() {
    this.mirrorDay();
    const root = this.querySelector(`.${B}__readout`);
    if (!root) return;
    const hit = this.hoveredHit();
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
