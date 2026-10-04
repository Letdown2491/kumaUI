//! The weather widget's state and transport: one slow curl to
//! open-meteo for current conditions and the daily outlook, and one
//! Nominatim lookup when a location query is resolved. No API keys,
//! no new dependencies: the fetch is a curl subprocess (the nostr
//! panel's pattern) and the parse is serde.
//!
//! Coordinates come from the settings' cached resolve, so the poll
//! never geocodes. A failed fetch leaves the last-known snapshot in
//! place with the stale flag up: an old temperature beats a dash, and
//! the widget dims so it never reads as fresh.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use chrono::Datelike as _;
use gpui::{App, AppContext, Context, Entity};
use serde::Deserialize;

use crate::settings::{ResolvedLocation, Settings};

/// The weather's cadence: open-meteo's current conditions move on the
/// order of minutes, so 15 minutes costs nothing and stays honest.
const POLL: Duration = Duration::from_secs(15 * 60);
/// Older than this and the widget dims: two missed polls mean the
/// network or the service is gone, and the number is no longer fresh.
const STALE_AFTER: Duration = Duration::from_secs(31 * 60);

/// Current conditions plus today's range, one open-meteo response.
#[derive(Clone, Debug, PartialEq)]
pub struct Current {
    /// The raw Celsius values open-meteo returns; display converts.
    pub temp_c: f32,
    pub feels_c: f32,
    /// Today's range, read from the daily block.
    pub hi_c: f32,
    pub lo_c: f32,
    pub code: u8,
    /// The wind at 10m, km/h: the panel shows it, the bar doesn't.
    pub wind_kph: f32,
}

/// One forecast day: what the panel's strip shows.
#[derive(Clone, Debug, PartialEq)]
pub struct ForecastDay {
    /// Short weekday name ("Mon"), resolved from the date.
    pub day: String,
    pub code: u8,
    pub hi_c: f32,
    pub lo_c: f32,
    /// Rain chance, percent (open-meteo's precipitation_probability_max).
    pub rain: u8,
}

#[derive(Default)]
pub struct WeatherState {
    pub current: Option<Current>,
    pub forecast: Vec<ForecastDay>,
    /// When the snapshot was fetched; drives the stale flag.
    updated: Option<Instant>,
    /// The resolved location the last fetch used, so the settings
    /// observer can tell a resolve commit from any other write.
    last_location: Option<Option<ResolvedLocation>>,
}

impl WeatherState {
    /// The snapshot's age as a stale verdict: a fetch older than two
    /// poll gaps is no longer trustworthy.
    pub fn stale(&self) -> bool {
        self.updated
            .map(|at| at.elapsed() > STALE_AFTER)
            .unwrap_or(true)
    }

    /// The condition's short name and glyph for a WMO weather code.
    pub fn condition(code: u8) -> (&'static str, &'static str) {
        match code {
            0 => ("Clear", "icons/sun.svg"),
            1 | 2 => ("Partly cloudy", "icons/cloud-sun.svg"),
            3 => ("Overcast", "icons/cloud.svg"),
            45 | 48 => ("Fog", "icons/cloud-fog.svg"),
            51..=57 => ("Drizzle", "icons/cloud-drizzle.svg"),
            61..=67 | 80..=82 => ("Rain", "icons/cloud-rain.svg"),
            71..=77 | 85 | 86 => ("Snow", "icons/cloud-snow.svg"),
            95..=99 => ("Storm", "icons/cloud-lightning.svg"),
            _ => ("Unknown", "icons/cloud.svg"),
        }
    }

    /// A temperature in the settings' unit, no decimals: the bar and
    /// the panel share the one formatter.
    pub fn format_temp(fahrenheit: bool, celsius: f32) -> String {
        let value = if fahrenheit {
            celsius * 9.0 / 5.0 + 32.0
        } else {
            celsius
        };
        format!("{}°{}", value.round(), if fahrenheit { "F" } else { "C" })
    }

    /// One fetch, in flight or not: a background curl, the parse, and
    /// a notify. A failure keeps the old snapshot and its age, which
    /// is how stale happens.
    fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(location) = self
            .last_location
            .as_ref()
            .and_then(|resolved| resolved.as_ref())
            .cloned()
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { fetch_weather(&location) })
                .await;
            let _ = this.update(cx, |state, cx| {
                match result {
                    Ok((current, forecast)) => {
                        state.current = Some(current);
                        state.forecast = forecast;
                        state.updated = Some(Instant::now());
                    }
                    Err(err) => log::info!("weather fetch failed: {err:#}"),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

/// The open-meteo forecast response: only the fields we read. The URL
/// pins `timezone=auto` so daily names land in local time, and the
/// metric units open-meteo defaults to (display converts).
#[derive(Deserialize)]
struct OpenMeteo {
    current: Option<OpenMeteoCurrent>,
    daily: Option<OpenMeteoDaily>,
}

#[derive(Deserialize)]
struct OpenMeteoCurrent {
    temperature_2m: f32,
    apparent_temperature: f32,
    weather_code: u8,
    wind_speed_10m: f32,
}

#[derive(Deserialize)]
struct OpenMeteoDaily {
    time: Vec<String>,
    weather_code: Vec<u8>,
    temperature_2m_max: Vec<f32>,
    temperature_2m_min: Vec<f32>,
    precipitation_probability_max: Vec<u8>,
}

/// One Nominatim match: the display name and the coordinates, read as
/// strings because Nominatim hands them over as strings anyway.
#[derive(Deserialize)]
pub struct NominatimMatch {
    pub display_name: String,
    /// Nominatim's own short name: the city for a city query, the
    /// postcode for a postal query.
    pub name: String,
    pub lat: String,
    pub lon: String,
    #[serde(default)]
    pub importance: f32,
}

/// The best Nominatim match for a query: the resolve the settings
/// field caches. Postal codes and city names ride the same endpoint.
/// Five candidates come back and the most important one wins:
/// Nominatim's `limit=1` pick is not always that one (a bare postal
/// code can rank an obscure namesake first). Blocking on purpose:
/// the caller runs it inside `background_spawn`.
pub fn geolocate(query: &str) -> anyhow::Result<ResolvedLocation> {
    let output = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "15",
            "-A",
            "kuma-shell/1.0 (desktop shell weather widget)",
        ])
        .arg(format!(
            "https://nominatim.openstreetmap.org/search?q={}&format=jsonv2&limit=5",
            urlencode(query)
        ))
        .output()?;
    anyhow::ensure!(output.status.success(), "nominatim search failed");
    let matches: Vec<NominatimMatch> = serde_json::from_slice(&output.stdout)?;
    let best = matches
        .into_iter()
        .enumerate()
        .max_by(|(a_index, a), (b_index, b)| {
            a.importance
                .total_cmp(&b.importance)
                .then(b_index.cmp(a_index))
        })
        .map(|(_, match_)| match_)
        .context("no matches")?;
    // the label is the city alone: Nominatim's short `name` when it
    // is not just the query echoed back (a postal query's name is the
    // postcode; the city rides display_name's second segment), else
    // the second piece of the comma parade
    let label = if best.name.is_empty() || best.name == query.trim() {
        best.display_name
            .split(", ")
            .nth(1)
            .unwrap_or(&best.display_name)
            .to_string()
    } else {
        best.name
    };
    Ok(ResolvedLocation {
        label,
        lat: best.lat,
        lon: best.lon,
    })
}

fn urlencode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The forecast fetch: current conditions plus five forecast days in
/// one request, so the panel never opens a second one. Blocking on
/// purpose: the caller runs it inside `background_spawn`.
fn fetch_weather(location: &ResolvedLocation) -> anyhow::Result<(Current, Vec<ForecastDay>)> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}\
         &current=temperature_2m,apparent_temperature,weather_code,wind_speed_10m\
         &daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max\
         &forecast_days=5&timezone=auto",
        location.lat, location.lon
    );
    let output = std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "15"])
        .arg(url)
        .output()?;
    anyhow::ensure!(output.status.success(), "open-meteo request failed");
    let doc: OpenMeteo = serde_json::from_slice(&output.stdout)?;
    let current = doc.current.context("no current block")?;
    let mut current = Current {
        temp_c: current.temperature_2m,
        feels_c: current.apparent_temperature,
        code: current.weather_code,
        wind_kph: current.wind_speed_10m,
        // filled from the daily block below; the temperature itself
        // when that block is missing
        hi_c: current.temperature_2m,
        lo_c: current.temperature_2m,
    };
    let mut forecast = Vec::new();
    if let Some(daily) = doc.daily {
        for index in 0..daily.time.len() {
            let Some(code) = daily.weather_code.get(index) else {
                break;
            };
            let (Some(hi), Some(lo), Some(rain)) = (
                daily.temperature_2m_max.get(index),
                daily.temperature_2m_min.get(index),
                daily.precipitation_probability_max.get(index),
            ) else {
                break;
            };
            // "2026-10-03" lands in local time (timezone=auto); the
            // weekday comes from the date alone
            let day = chrono::NaiveDate::parse_from_str(&daily.time[index], "%Y-%m-%d")
                .map(|date| {
                    let weekday = date.weekday().to_string();
                    weekday[..3].to_string()
                })
                .unwrap_or_default();
            forecast.push(ForecastDay {
                day,
                code: *code,
                hi_c: *hi,
                lo_c: *lo,
                rain: *rain,
            });
        }
        // today's hi/lo ride the current snapshot's tooltip
        if let (Some(hi), Some(lo)) = (
            daily.temperature_2m_max.first(),
            daily.temperature_2m_min.first(),
        ) {
            current.hi_c = *hi;
            current.lo_c = *lo;
        }
    }
    Ok((current, forecast))
}

pub fn run(state: &Entity<WeatherState>, settings: &Entity<Settings>, cx: &mut App) {
    // seed the last-seen location, then refetch whenever the resolved
    // location changes (a settings commit): the resolve is the only
    // thing that moves the coordinates
    let seeded = settings.read(cx).weather.resolved.clone();
    state.update(cx, |state, _| {
        state.last_location = Some(seeded);
    });    {
        let state = state.downgrade();
        cx.observe(settings, move |settings, cx| {
            let _ = state.update(cx, |state, cx| {
                let resolved = settings.read(cx).weather.resolved.clone();
                if state.last_location.as_ref() != Some(&resolved) {
                    state.last_location = Some(resolved);
                    state.fetch(cx);
                }
            });
        })
        .detach();
    }

    let state = state.downgrade();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL).await;
            let _ = state.update(cx, |state, cx| state.fetch(cx));
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wmo_codes_map_to_names_and_glyphs() {
        assert_eq!(WeatherState::condition(0), ("Clear", "icons/sun.svg"));
        assert_eq!(
            WeatherState::condition(2),
            ("Partly cloudy", "icons/cloud-sun.svg")
        );
        assert_eq!(WeatherState::condition(3), ("Overcast", "icons/cloud.svg"));
        assert_eq!(WeatherState::condition(45), ("Fog", "icons/cloud-fog.svg"));
        assert_eq!(
            WeatherState::condition(53),
            ("Drizzle", "icons/cloud-drizzle.svg")
        );
        assert_eq!(
            WeatherState::condition(63),
            ("Rain", "icons/cloud-rain.svg")
        );
        assert_eq!(WeatherState::condition(80), ("Rain", "icons/cloud-rain.svg"));
        assert_eq!(WeatherState::condition(75), ("Snow", "icons/cloud-snow.svg"));
        assert_eq!(
            WeatherState::condition(95),
            ("Storm", "icons/cloud-lightning.svg")
        );
        assert_eq!(WeatherState::condition(42), ("Unknown", "icons/cloud.svg"));
    }

    #[test]
    fn temps_format_in_the_settings_unit() {
        assert_eq!(WeatherState::format_temp(false, 18.4), "18°C");
        assert_eq!(WeatherState::format_temp(true, 18.4), "65°F");
        assert_eq!(WeatherState::format_temp(true, 0.0), "32°F");
        assert_eq!(WeatherState::format_temp(false, -5.6), "-6°C");
    }

    #[test]
    fn query_urlencoding() {
        assert_eq!(urlencode("Hillsboro, OR"), "Hillsboro%2C+OR");
        assert_eq!(urlencode("97123"), "97123");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }
}
