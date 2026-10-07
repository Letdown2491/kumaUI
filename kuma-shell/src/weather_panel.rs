use gpui::{Context, Entity, Render, Window, div, prelude::*, px, rgb, rgba, size};

use crate::panel_kit as kit;
use crate::settings::Settings;
use crate::weather::{Current, WeatherState};

/// The weather panel: current conditions up top, the five-day strip
/// under it. Clicking the bar widget opens it; the data is the slow
/// snapshot, nothing here fetches.
pub struct WeatherPanelView {
    geometry: crate::panel::PanelGeometry,
    weather: Entity<WeatherState>,
    settings: Entity<Settings>,
}

impl WeatherPanelView {
    pub fn new(
        weather: Entity<WeatherState>,
        settings: Entity<Settings>,
        _window: &mut Window,
        cx: &mut Context<Self>,
        geometry: crate::panel::PanelGeometry,
    ) -> Self {
        cx.observe(&weather, |_, _, cx| cx.notify()).detach();
        Self {
            geometry,
            weather,
            settings,
        }
    }

    fn current_block(&self, current: &Current, fahrenheit: bool, stale: bool) -> gpui::Div {
        let (condition, icon) = WeatherState::condition(current.code);
        let color = if stale { crate::theme::current().text_dim } else { crate::theme::current().text };
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(34.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(color))
                            .child(WeatherState::format_temp(fahrenheit, current.temp_c)),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(rgb(crate::theme::current().text_dim))
                            .child(format!(
                                "Feels like {} / {}",
                                WeatherState::format_temp(fahrenheit, current.feels_c),
                                condition
                            )),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .justify_end()
                    .child(gpui::svg().path(icon).size(px(40.)).text_color(rgb(color))),
            )
    }

    fn stat_row(&self, current: &Current, fahrenheit: bool) -> gpui::Div {
        div()
            .flex()
            .gap_2()
            .child(kit::card("weather-today").flex_1().child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .py_1p5()
                    .px_2()
                    .child(kit::card_note("Today"))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(crate::theme::current().text))
                            .child(format!(
                                "{} / {}",
                                WeatherState::format_temp(fahrenheit, current.hi_c),
                                WeatherState::format_temp(fahrenheit, current.lo_c)
                            )),
                    ),
            ))
            .child(kit::card("weather-wind").flex_1().child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .py_1p5()
                    .px_2()
                    .child(kit::card_note("Wind"))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(crate::theme::current().text))
                            .child(format!("{:.0} km/h", current.wind_kph)),
                    ),
            ))
    }

    fn forecast_strip(
        &self,
        forecast: &[crate::weather::ForecastDay],
        fahrenheit: bool,
        stale: bool,
    ) -> gpui::Div {
        let color = if stale { crate::theme::current().text_dim } else { crate::theme::current().text };
        div()
            .flex()
            .justify_between()
            .gap_1()
            .children(forecast.iter().enumerate().map(|(index, day)| {
                let (_, icon) = WeatherState::condition(day.code);
                let name = if index == 0 {
                    "Today".to_string()
                } else {
                    day.day.clone()
                };
                div()
                    .id(gpui::SharedString::from(format!("forecast-{index}")))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .flex_1()
                    .py_1p5()
                    .rounded_md()
                    .bg(rgb(crate::theme::current().inset))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(crate::theme::current().text_dim))
                            .child(name),
                    )
                    .child(gpui::svg().path(icon).size(px(18.)).text_color(rgb(color)))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(rgb(crate::theme::current().text))
                            .child(WeatherState::format_temp(fahrenheit, day.hi_c)),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(rgb(crate::theme::current().text_dim))
                            .child(WeatherState::format_temp(fahrenheit, day.lo_c)),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(crate::theme::current().text_dim))
                            .child(format!("{}%", day.rain)),
                    )
            }))
    }
}

impl Render for WeatherPanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let fahrenheit = self.settings.read(cx).weather.fahrenheit;
        let location = self
            .settings
            .read(cx)
            .weather
            .resolved
            .as_ref()
            .map(|resolved| resolved.label.clone());
        let weather = self.weather.read(cx);
        let stale = weather.stale();

        let content = div()
            .flex()
            .flex_col()
            .pt(px(10.))
            .px(px(16.))
            .pb(px(12.))
            .gap_2p5()
            .child(kit::pane_header(
                location.as_deref().unwrap_or("Weather").to_string().as_str(),
            ))
            .children(match (&weather.current, location) {
                (Some(current), Some(_)) => {
                    let current = current.clone();
                    let forecast = weather.forecast.clone();
                    vec![
                        self.current_block(&current, fahrenheit, stale)
                            .into_any_element(),
                        self.stat_row(&current, fahrenheit).into_any_element(),
                        div()
                            .h(px(1.))
                            .w_full()
                            .bg(rgba(crate::theme::current().divider_soft))
                            .into_any_element(),
                        self.forecast_strip(&forecast, fahrenheit, stale)
                            .into_any_element(),
                    ]
                }
                (Some(_), None) | (None, None) => vec![kit::empty_state(
                    "icons/cloud.svg",
                    "No location set",
                    "Set one on the Weather page in settings",
                )
                .into_any_element()],
                (None, Some(_)) => {
                    vec![kit::empty_state(
                        "icons/cloud.svg",
                        stale_label(stale),
                        "Waiting on the next poll",
                    )
                    .into_any_element()]
                }
            });

        // measured-panel flow: the content height refines the panel height
        let view = cx.weak_entity();
        let measured = crate::panel::MeasureHeight::new(content, move |height, window, cx| {
            let Some(view) = view.upgrade() else {
                return;
            };
            let mut resized = None;
            view.update(cx, |this, cx| {
                if (this.geometry.height - height).abs() > 0.5 {
                    this.geometry.height = height;
                    cx.notify();
                    resized = Some(height);
                }
            });
            if let Some(height) = resized {
                window.resize(size(window.viewport_size().width, px(height)));
            }
        });

        crate::panel::chrome(self.geometry, window, cx, measured)
    }
}

fn stale_label(stale: bool) -> &'static str {
    if stale {
        "Weather data is stale"
    } else {
        "Loading weather"
    }
}
