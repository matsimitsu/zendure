// <price-panel>: the price readout over the shared DayChart behaviour.
class PricePanel extends DayChart {
  showReadout(readout, read) {
    this.setText(readout, ".day-chart__read-value", read("value"));

    const tier = read("tier");
    const pill = readout.querySelector(".price-tier");
    if (pill) {
      for (const c of [...pill.classList]) {
        if (c.startsWith("price-tier--")) pill.classList.remove(c);
      }
      pill.hidden = !tier;
      pill.textContent = tier ? read("tierLabel") : "";
      if (tier) pill.classList.add(`price-tier--${tier}`);
    }
  }
}

customElements.define("price-panel", PricePanel);
