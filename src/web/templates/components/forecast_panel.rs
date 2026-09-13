use maud::{Markup, html};

/// Static placeholder: no forecasting component exists yet, so the panel
/// labels itself as sample data on the page.
pub fn render() -> Markup {
    const HOUR_LABELS: [&str; 24] = [
        "00", "", "", "03", "", "", "06", "", "", "09", "", "", "12", "", "", "15", "", "", "18",
        "", "", "21", "", "",
    ];

    const BATTERY_MODES: [&str; 24] = [
        "idle",
        "idle",
        "idle",
        "idle",
        "idle",
        "idle",
        "idle",
        "charge",
        "charge",
        "charge",
        "charge",
        "charge",
        "charge",
        "charge",
        "charge",
        "charge",
        "discharge",
        "discharge",
        "discharge",
        "discharge",
        "idle",
        "idle",
        "idle",
        "idle",
    ];

    const EV_MODES: [&str; 24] = [
        "charge", "charge", "charge", "charge", "charge", "charge", "charge", "idle", "idle",
        "idle", "idle", "idle", "charge", "charge", "idle", "idle", "idle", "idle", "idle", "idle",
        "idle", "idle", "idle", "idle",
    ];

    const SOLAR_LINE_PATH: &str = "M0.0,108.0 L43.5,108.0 L87.0,108.0 L130.4,108.0 L173.9,108.0 L217.4,108.0 L260.9,104.2 L304.3,92.9 L347.8,71.1 L391.3,49.3 L434.8,28.5 L478.3,14.3 L521.7,7.2 L565.2,2.0 L608.7,8.6 L652.2,21.9 L695.7,41.8 L739.1,65.4 L782.6,88.1 L826.1,103.7 L869.6,108.0 L913.0,108.0 L956.5,108.0 L1000.0,108.0";

    const SOLAR_AREA_PATH: &str = "M0.0,108.0 L43.5,108.0 L87.0,108.0 L130.4,108.0 L173.9,108.0 L217.4,108.0 L260.9,104.2 L304.3,92.9 L347.8,71.1 L391.3,49.3 L434.8,28.5 L478.3,14.3 L521.7,7.2 L565.2,2.0 L608.7,8.6 L652.2,21.9 L695.7,41.8 L739.1,65.4 L782.6,88.1 L826.1,103.7 L869.6,108.0 L913.0,108.0 L956.5,108.0 L1000.0,108.0 L1000,110 L0,110 Z";

    html! {
        div class="forecast-panel" {
            div class="forecast-panel__header" {
                h2 class="forecast-panel__title" { "24-hour forecast (sample data)" }
                p class="forecast-panel__subtitle" { "Predicted solar yield and planned charge schedule" }
                p class="forecast-panel__notice" { "No forecasting exists yet — the curve and the schedule below are fixed illustrations, not predictions." }
            }

            div class="forecast-panel__chart-label" { "Predicted solar (kW)" }

            svg class="forecast-panel__chart" viewBox="0 0 1000 110" {
                defs {
                    linearGradient id="solarGrad" x1="0" y1="0" x2="0" y2="1" {
                        stop offset="0%" stop-color="var(--color-accent)" stop-opacity="0.3" {}
                        stop offset="100%" stop-color="var(--color-accent)" stop-opacity="0" {}
                    }
                }
                path d=(SOLAR_LINE_PATH) fill="none" stroke="var(--color-accent)" stroke-width="2" stroke-linejoin="round" {}
                path d=(SOLAR_AREA_PATH) fill="url(#solarGrad)" {}
            }

            div class="forecast-panel__axis" {
                div class="forecast-panel__axis-gutter" {}
                @for label in HOUR_LABELS.iter() {
                    div class="forecast-panel__axis-label" { (label) }
                }
            }

            div class="forecast-panel__row" {
                div class="forecast-panel__row-label" { "Battery" }
                @for (hour, &mode) in BATTERY_MODES.iter().enumerate() {
                    (render_slot(hour, mode))
                }
            }

            div class="forecast-panel__row" {
                div class="forecast-panel__row-label" { "EV" }
                @for (hour, &mode) in EV_MODES.iter().enumerate() {
                    (render_slot(hour, mode))
                }
            }

            div class="forecast-panel__legend" {
                @for &(mode, label) in [("charge", "Charging"), ("discharge", "Discharging"), ("idle", "Idle")].iter() {
                    div class="forecast-panel__legend-item" {
                        div class={(format!("forecast-panel__legend-swatch forecast-panel__legend-swatch--{}", mode))} {}
                        span class="forecast-panel__legend-label" { (label) }
                    }
                }
            }
        }
    }
}

fn render_slot(hour: usize, mode: &str) -> Markup {
    let class = format!("forecast-panel__slot forecast-panel__slot--{}", mode);
    let title = format!("{:02}:00 — {}", hour, mode);
    html! {
        div class=(class) title=(title) {}
    }
}
