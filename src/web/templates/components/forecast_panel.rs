use maud::{Markup, html};

use crate::web::view::ForecastPanelView;

/// The predicted-vs-actual solar chart: bars for the forecast, a line for
/// today's actual measured production over whichever hours have elapsed.
pub fn render(view: &ForecastPanelView) -> Markup {
    let bar_width = 1000.0 / view.bar_heights.len() as f64;

    html! {
        div class="forecast-panel" {
            div class="forecast-panel__header" {
                h2 class="forecast-panel__title" { "24-hour solar forecast" }
                @if view.has_data {
                    p class="forecast-panel__subtitle" { (view.as_of) }
                }
            }

            @if view.has_data {
                div class="forecast-panel__chart-label" { "Forecast (bars) vs. actual (line), in watts" }

                svg class="forecast-panel__chart" viewBox="0 0 1000 110" {
                    @for (hour, &height) in view.bar_heights.iter().enumerate() {
                        @let x = hour as f64 * bar_width;
                        @let y = 108.0 - height;
                        rect class="forecast-panel__bar" x=(format!("{x:.1}")) y=(format!("{y:.1}")) width=(format!("{:.1}", bar_width - 1.0)) height=(format!("{height:.1}")) {}
                    }
                    path class="forecast-panel__line" d=(view.line_path) fill="none" {}
                }

                div class="forecast-panel__axis" {
                    div class="forecast-panel__axis-gutter" {}
                    @for label in &view.hour_labels {
                        div class="forecast-panel__axis-label" { (label) }
                    }
                }
            } @else {
                p class="forecast-panel__empty" { (view.as_of) }
            }
        }
    }
}
