// <forecast-panel>: hover/tap readout for the server-rendered solar forecast.
//
// The hover, pin and SSE behaviour is shared with the other day panels in
// DayChart (day-chart.js); this only knows which strings the forecast readout
// shows and where its highlight sits.
const B = "forecast-panel";

class ForecastPanel extends DayChart {
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
    const forecast = hit ? hit.dataset.forecast : d.defaultForecast;
    const forecastUnit = hit ? hit.dataset.forecastUnit : d.defaultForecastUnit;
    const actual = hit ? hit.dataset.actual : d.defaultActual;
    const actualUnit = hit ? hit.dataset.actualUnit : d.defaultActualUnit;

    const set = (sel, text) => {
      const el = root.querySelector(sel);
      if (el) el.textContent = text ?? "";
    };
    set(`.${B}__read-label`, label);
    set(`.${B}__read--forecast .${B}__read-value`, forecast);
    set(`.${B}__read--forecast .${B}__read-unit`, forecastUnit);
    set(`.${B}__read--actual .${B}__read-value`, actual);
    // A slot with no actual reads "—"; a unit beside it would mislabel it.
    set(`.${B}__read--actual .${B}__read-unit`, actual === "—" ? "" : actualUnit);

    for (const h of this.querySelectorAll(`.${B}__highlight`)) {
      h.setAttribute("x", hit ? hit.getAttribute("x") : h.dataset.defaultX);
      h.setAttribute("width", hit ? hit.getAttribute("width") : h.dataset.defaultWidth);
    }
  }
}

customElements.define("forecast-panel", ForecastPanel);
