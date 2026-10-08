// <line-chart>: a fixed readout, a crosshair and a dot that follow the pointer.
//
// The server renders every string (times, formatted values) into data-*
// arrays, one entry per bucket; this only copies them. Modal content is
// fetched once and never swapped, so direct listeners are safe here.
const B = "line-chart";
const VIEW_WIDTH = 1000;
const VIEW_HEIGHT = 180;

class LineChart extends HTMLElement {
  connectedCallback() {
    this.times = JSON.parse(this.dataset.times ?? "[]");
    this.values = JSON.parse(this.dataset.values ?? "[]");
    this.ys = JSON.parse(this.dataset.y ?? "[]");
    this.pinned = false;

    // Charts in one modal share a crosshair, so the event is caught on the
    // modal body rather than on each chart.
    this.scope = this.closest(".modal__body") ?? this;
    this.onSync = (e) => this.show(e.detail.index);
    this.onOutside = (e) => {
      if (this.pinned && !this.contains(e.target)) {
        this.pinned = false;
        this.announce(null);
      }
    };
    this.scope.addEventListener(`${B}:hover`, this.onSync);
    document.addEventListener("pointerdown", this.onOutside);

    const svg = this.querySelector(`.${B}__svg`);
    svg.addEventListener("pointermove", (e) => {
      if (e.pointerType === "touch" && !this.pinned) return;
      this.announce(this.indexAt(e, svg));
    });
    svg.addEventListener("pointerdown", (e) => {
      if (e.pointerType === "mouse") return;
      this.pinned = true;
      this.announce(this.indexAt(e, svg));
    });
    // A pinned touch stays put; only a mouse leaving resets to "now".
    svg.addEventListener("pointerleave", (e) => {
      if (e.pointerType === "mouse") this.announce(null);
    });

    this.addEventListener("keydown", (e) => this.onKey(e));
  }

  disconnectedCallback() {
    this.scope?.removeEventListener(`${B}:hover`, this.onSync);
    document.removeEventListener("pointerdown", this.onOutside);
  }

  indexAt(e, svg) {
    const rect = svg.getBoundingClientRect();
    const last = this.values.length - 1;
    if (last < 1 || rect.width === 0) return 0;
    const ratio = (e.clientX - rect.left) / rect.width;
    return Math.min(last, Math.max(0, Math.round(ratio * last)));
  }

  onKey(e) {
    const last = this.values.length - 1;
    const from = this.index ?? last;
    const steps = { ArrowLeft: from - 1, ArrowRight: from + 1, Home: 0, End: last };
    if (!(e.key in steps)) return;
    e.preventDefault();
    this.announce(Math.min(last, Math.max(0, steps[e.key])));
  }

  announce(index) {
    this.dispatchEvent(new CustomEvent(`${B}:hover`, { bubbles: true, detail: { index } }));
  }

  // `index` null is the resting state: the latest bucket, labelled "now".
  show(index) {
    this.index = index;
    const resting = index === null || this.values[index] === undefined;
    const i = resting ? this.values.length - 1 : index;
    const time = this.querySelector(`.${B}__readout-time`);
    const value = this.querySelector(`.${B}__readout-value`);
    time.textContent = resting ? "now" : this.times[i];
    value.textContent = this.values[i];

    const y = this.ys[i];
    this.classList.toggle(`${B}--hover`, !resting);
    this.classList.toggle(`${B}--gap`, y === null || y === undefined);

    const last = this.values.length - 1;
    const x = last > 0 ? (i / last) * VIEW_WIDTH : 0;
    const crosshair = this.querySelector(`.${B}__crosshair`);
    crosshair.setAttribute("x1", x);
    crosshair.setAttribute("x2", x);
    const dot = this.querySelector(`.${B}__dot`);
    dot.style.left = `${(x / VIEW_WIDTH) * 100}%`;
    dot.style.top = `${((y ?? 0) / VIEW_HEIGHT) * 100}%`;
  }
}

customElements.define(B, LineChart);
