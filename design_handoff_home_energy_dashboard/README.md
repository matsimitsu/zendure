# Handoff: Home Energy Dashboard

## Overview
A display-only dashboard for a home battery management system: live solar/home/grid/battery/EV state, a decision log explaining what the controller did and why, and a 24-hour forecast of the planned charge/discharge schedule for the battery and (future) EV. Dark, technical "control-room" aesthetic.

## About the design files
The bundled files (`Home Energy.dc.html`, `StatCard.dc.html`, `MiniStat.dc.html`) are **HTML design references**, built as streaming design components in the design tool — not production code. Recreate them as Maud templates + Grass stylesheets in the target Rust/Axum stack, following the architecture rules below. Don't port the HTML/inline-style markup directly.

## Fidelity
**High-fidelity.** Colors, typography, spacing and layout below are final; copy/data are illustrative sample values pending real telemetry.

## Architecture rules
These are non-negotiable for this build:

1. **One Maud component = one CSS file.** Each component gets its own `.rs` (a Maud template function) and a co-located `.scss`/`.css` file of the same name, compiled with Grass and served via `rust-embed`. Class names follow **BEM** (`block__element--modifier`).
2. **Tokens live in one place.** All raw values (color, type scale, spacing, radius) are declared **once**, as CSS custom properties, in a single index/tokens stylesheet. Component stylesheets only ever *consume* `var(--token-name)` — they never declare a literal color, px value, or new custom property of their own.
3. **No layout literals in components.** A component's own stylesheet must not hardcode `width`, `height`, `padding`, or similar box dimensions — those come from tokens (`var(--space-*)`, `var(--size-*)`) or from the parent, never as inline numbers in the component file.
4. **Parents own spacing between components, not components themselves.** A component never sets its own external `margin`. Layout containers (the parent) space children with `gap` (flex/grid), so components stay drop-in and reorderable. A component's stylesheet may size *its own internal* children this way too — spacing between its children is that component's "parent" job.

## Design tokens

**Color** (dark neutral scale + single amber accent + two semantic hues, all same chroma/lightness family):
```
--color-bg-canvas:      oklch(0.16 0.015 250);
--color-bg-header:      oklch(0.13 0.015 250);
--color-surface:        oklch(0.23 0.015 250);
--color-border:         oklch(1 0 0 / 0.08);
--color-border-soft:    oklch(1 0 0 / 0.07);

--color-text-primary:   oklch(0.96 0.01 250);
--color-text-secondary: oklch(0.9  0.01 250);
--color-text-muted:     oklch(0.65 0.02 250);
--color-text-faint:     oklch(0.55 0.02 250);

--color-accent:         oklch(0.75 0.15 75);   /* amber — solar, charging, primary */
--color-accent-bg:      oklch(0.30 0.06  75);
--color-accent-text:    oklch(0.90 0.10  75);

--color-negative:       oklch(0.70 0.16 25);   /* red — discharge */
--color-negative-bg:    oklch(0.30 0.07 25);
--color-negative-text:  oklch(0.85 0.10 25);

--color-info:           oklch(0.72 0.12 200);  /* teal-blue — grid */
--color-home:           oklch(0.70 0.13 250);  /* blue — home usage */
--color-ev:             oklch(0.72 0.13 300);  /* purple — EV */

--color-idle:           oklch(0.35 0.01 250);
--color-idle-bg:        oklch(1 0 0 / 0.08);
```

**Typography**
```
--font-ui:   "Inter", system-ui, sans-serif;
--font-mono: "JetBrains Mono", monospace;

--text-xs:   11px;  --text-sm: 12.5px; --text-base: 13.5px;
--text-md:   14.5px; --text-lg: 22px;  --text-2xl: 28px; --text-3xl: 30px;

--weight-medium: 500; --weight-semibold: 600; --weight-bold: 700;
```

**Spacing / radius** (4px base grid)
```
--space-1: 4px;  --space-2: 8px;  --space-3: 12px; --space-4: 16px;
--space-5: 20px; --space-6: 24px; --space-7: 28px; --space-8: 32px;

--radius-sm: 4px; --radius-md: 6px; --radius-lg: 8px; --radius-xl: 10px;
```

## Screens / Views

Single scrolling page, max content width ~1180px, centered, 24px side padding.

### 1. Top bar
Fixed-height header: logo mark + "Home energy system" wordmark (left), operational status dot + label (right). `background: --color-bg-header`, bottom `--color-border` hairline.

### 2. Page header
Eyebrow ("Overview"), title ("Live system state"), one-line description with last-update timestamp.

### 3. Stat card row — `.stat-card` (×4: Solar, Home usage, Grid, EV)
4-column grid on desktop, 2-column under ~860px, `gap: --space-4`.
- `.stat-card` — surface panel, `--radius-xl`, `--color-border` ring
- `.stat-card__icon` — 30×30 tinted tile, one of the semantic colors above
- `.stat-card__label` — `--text-sm`, `--color-text-muted`
- `.stat-card__value` — `--font-mono`, `--text-3xl`, `--weight-semibold`
- `.stat-card__unit` — `--text-base`, `--color-text-faint`
- `.stat-card__sparkline` — inline SVG line chart, 96×28, stroke = card's semantic color
- `.stat-card__detail` — `--text-sm`, `--color-text-muted`, one line of context (e.g. "Peak today: 2,240 W at 13:15")

Sample values: Solar 2,130 W · Home 850 W · Grid −80 W (negative = exporting) · EV 42% (flagged "sample data" — not wired to a real vehicle yet).

### 4. Battery panel — `.battery-panel`
Full-width surface panel below the stat row.
- `.battery-panel__head` — icon tile + label ("Home battery") + big mono SOC value (e.g. "64%"), with a mode badge (`.battery-panel__badge`, amber for charging, red for discharging, neutral for idle) right-aligned
- `.battery-panel__bar` — 8px rounded track, fill = SOC%, amber when charging
- `.battery-panel__stats` — 4-column row of label/value pairs (Rate, Usable energy, Capacity, Round-trip efficiency), each a small reusable `.mini-stat` component

### 5. Decision log — `.decision-log`
Secondary/subtle by design — smaller type, no big numbers.
- Header: title "Decision log" + subtitle "Why the controller did what it did"
- `.decision-log__row` (×N, top-bordered): time (mono, muted) · mode badge (`--charge`/`--discharge`/`--idle` modifiers) · reason sentence · power value (mono, right-aligned)
- Reason text should map to the controller's real `ControlDecision.reason` strings in production.

### 6. 24-hour forecast — `.forecast-panel`
- Title "24-hour forecast" / subtitle "Predicted solar yield and planned charge schedule"
- `.forecast-panel__chart` — area+line SVG chart of predicted solar (kW) across 24 hourly points, amber, gradient fill fading to transparent
- `.forecast-panel__axis` — hour labels under the chart, mono, every 3rd hour shown, grid: fixed label gutter + 24 equal columns
- `.forecast-panel__row` (×2: "Battery", "EV") — a label + 24 colored slot cells (`.forecast-panel__slot`) in the same 24-column grid; modifiers `--charge` (amber), `--discharge` (red), `--idle` (neutral gray); each cell's `title`/tooltip states the hour and planned action
- `.forecast-panel__legend` — 3 swatch+label pairs (Charging / Discharging / Idle)

### 7. Callout — `.callout--info`
Teaser banner: "Goal-based scheduling is coming" + one line of body copy. Tinted amber panel, low-opacity accent border. This is copy only for now — no functional goal-setting UI in this build.

## Interactions & behavior
Display-only, as specified — no click handlers, forms, or live data wiring in this pass. All values are static sample data. When wiring to the real controller, poll/subscribe the same way the existing MQTT/REST integration already does (see the attached Rust controller) and replace the sample arrays 1:1 by field.

## Data model notes (for real wiring)
- Battery SOC/rate/mode ← `ZendureProperties.electric_level`, `pack_input_power`/`output_pack_power`, `ControlMode`
- Decision log rows ← `ControlDecision { mode, power_watts, reason, grid_power }`, timestamped
- Grid/solar/home stat cards ← `ShellyReading` (grid) and `ZendureProperties.solar_input_power` (solar); home usage is derived (solar + grid − battery, or a dedicated CT clamp if added)
- EV card is a placeholder — no EV integration exists yet in the controller; keep it visually flagged as sample data until real telemetry lands
- 24h forecast requires a new planning component (not in the current controller) driven by a weather/solar forecast input — out of scope for this pass beyond the static mock

## Assets
No icons or images — unicode glyphs (☀ ⌂ ⇄ ⛽ ▮ ⓘ) are placeholders standing in for a real icon set. Swap for a proper icon font/SVG set before shipping.

## Suggested file layout
```
templates/
  tokens.scss              <- all custom properties above, imported once
  index.scss                <- @forward tokens + each component partial
  components/
    top_bar.rs / .scss
    page_header.rs / .scss
    stat_card.rs / .scss
    mini_stat.rs / .scss
    battery_panel.rs / .scss
    decision_log.rs / .scss
    forecast_panel.rs / .scss
    callout.rs / .scss
```
Grass compiles `index.scss` (or each component partial) at build time; `rust-embed` serves the compiled CSS/static assets from the binary.

## Files
- `Home Energy.dc.html` — full page reference
- `StatCard.dc.html` — stat card component reference
- `MiniStat.dc.html` — label/value pair reference used inside the battery panel
