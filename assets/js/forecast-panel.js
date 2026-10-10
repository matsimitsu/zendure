// <forecast-panel>: the forecast and actual readout over the shared DayChart
// behaviour.
class ForecastPanel extends DayChart {
  showReadout(readout, read) {
    for (const series of ["forecast", "actual"]) {
      const figure = `.forecast-panel__read--${series}`;
      this.setText(readout, `${figure} .day-chart__read-value`, read(series));
      this.setText(readout, `${figure} .day-chart__read-unit`, read(`${series}Unit`));
    }
  }
}

customElements.define("forecast-panel", ForecastPanel);
