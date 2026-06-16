use serde_json::Value;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// How often to refresh weather data, and how soon to retry after a failure.
const WEATHER_REFRESH: Duration = Duration::from_secs(900); // 15 minutes
const RETRY_REFRESH: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct DayForecast {
    pub weekday: String,
    pub icon: String,
    pub desc: String,
    pub tmax: f64,
    pub tmin: f64,
}

#[derive(Clone, Default)]
pub struct WeatherData {
    pub available: bool,
    pub current_temp: Option<f64>,
    pub current_icon: String,
    pub unit: String,
    pub days: Vec<DayForecast>,
}

/// Map a wttr.in (WWO) weather code to one of our icon file names. Clear skies
/// resolve to sunny here; the caller swaps in the moon icon at night.
fn weather_icon_name(code: i64) -> &'static str {
    match code {
        113 => "weather_sunny",
        // rain, drizzle, showers, thunder
        176 | 200 | 263 | 266 | 281 | 284 | 293 | 296 | 299 | 302 | 305 | 308 | 311 | 314
        | 353 | 356 | 359 | 386 | 389 | 392 | 395 => "weather_rainy",
        // snow, sleet, freezing
        179 | 182 | 185 | 227 | 230 | 317 | 320 | 323 | 326 | 329 | 332 | 335 | 338 | 350
        | 362 | 365 | 368 | 371 | 374 | 377 => "weather_snowy",
        // partly cloudy, cloudy, overcast, fog, mist, anything else
        _ => "weather_cloudy",
    }
}

pub struct WeatherManager {
    data: Arc<Mutex<WeatherData>>,
}

impl WeatherManager {
    pub fn new(location: Option<String>, fahrenheit: bool) -> Self {
        let unit = if fahrenheit { "°F" } else { "°C" }.to_string();
        let data = Arc::new(Mutex::new(WeatherData {
            unit: unit.clone(),
            ..Default::default()
        }));
        let data_clone = Arc::clone(&data);

        thread::spawn(move || {
            let mut next_fetch = Instant::now();
            loop {
                if Instant::now() >= next_fetch {
                    let ok = match fetch_weather(&location, fahrenheit) {
                        Some(wd) => {
                            *data_clone.lock().unwrap() = wd;
                            true
                        }
                        None => false,
                    };
                    next_fetch = Instant::now()
                        + if ok { WEATHER_REFRESH } else { RETRY_REFRESH };
                }
                thread::sleep(Duration::from_secs(5));
            }
        });

        WeatherManager { data }
    }

    pub fn data(&self) -> WeatherData {
        self.data.lock().unwrap().clone()
    }
}

fn curl(url: &str) -> Option<String> {
    let out = Command::new("curl")
        .arg("-fsS")
        .arg("--max-time")
        .arg("8")
        .arg(url)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Fetch current + forecast from wttr.in (same source waybar uses). wttr.in
/// auto-detects location by IP when none is given; the JSON `j1` view provides
/// a three-day forecast.
fn fetch_weather(location: &Option<String>, fahrenheit: bool) -> Option<WeatherData> {
    let url = match location {
        Some(loc) => format!("https://wttr.in/{}?format=j1", loc.replace(' ', "+")),
        None => "https://wttr.in/?format=j1".to_string(),
    };
    let body = curl(&url)?;
    let v: Value = serde_json::from_str(&body).ok()?;

    let (temp_key, max_key, min_key) = if fahrenheit {
        ("temp_F", "maxtempF", "mintempF")
    } else {
        ("temp_C", "maxtempC", "mintempC")
    };

    let current = v.get("current_condition")?.as_array()?.first()?;
    let current_temp = current
        .get(temp_key)
        .and_then(|t| t.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let current_code = current
        .get("weatherCode")
        .and_then(|c| c.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(-1);

    // Clear skies show the moon at night.
    let mut current_icon = weather_icon_name(current_code).to_string();
    if current_code == 113 && is_night(&v) {
        current_icon = "weather_moon".to_string();
    }

    let mut days = Vec::new();
    if let Some(weather) = v.get("weather").and_then(|w| w.as_array()) {
        for day in weather {
            let date = day.get("date").and_then(|d| d.as_str()).unwrap_or("");
            let tmax = parse_temp(day.get(max_key));
            let tmin = parse_temp(day.get(min_key));
            // Representative condition: the midday hourly slot if present.
            let noon = day
                .get("hourly")
                .and_then(|h| h.as_array())
                .and_then(|hours| {
                    hours
                        .iter()
                        .find(|h| h.get("time").and_then(|t| t.as_str()) == Some("1200"))
                        .or_else(|| hours.get(hours.len() / 2))
                });
            let code = noon
                .and_then(|h| h.get("weatherCode"))
                .and_then(|c| c.as_str())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(-1);
            let desc = noon.and_then(desc_value).unwrap_or_default();
            days.push(DayForecast {
                weekday: weekday_abbrev(date),
                icon: weather_icon_name(code).to_string(),
                desc,
                tmax,
                tmin,
            });
        }
    }

    Some(WeatherData {
        available: true,
        current_temp,
        current_icon,
        unit: if fahrenheit { "°F" } else { "°C" }.to_string(),
        days,
    })
}

/// Determine whether it is currently night at the location, using today's
/// sunrise/sunset from the wttr.in astronomy block (same approach as the
/// omarchy weather icon).
fn is_night(v: &Value) -> bool {
    let astronomy = v
        .get("weather")
        .and_then(|w| w.as_array())
        .and_then(|w| w.first())
        .and_then(|d| d.get("astronomy"))
        .and_then(|a| a.as_array())
        .and_then(|a| a.first());
    let Some(astronomy) = astronomy else {
        return false;
    };
    let parse = |key: &str| {
        astronomy
            .get(key)
            .and_then(|t| t.as_str())
            .and_then(|s| chrono::NaiveTime::parse_from_str(s.trim(), "%I:%M %p").ok())
    };
    match (parse("sunrise"), parse("sunset")) {
        (Some(sunrise), Some(sunset)) => {
            let now = chrono::Local::now().time();
            now < sunrise || now >= sunset
        }
        _ => false,
    }
}

fn desc_value(node: &Value) -> Option<String> {
    node.get("weatherDesc")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|x| x.get("value"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_temp(node: Option<&Value>) -> f64 {
    node.and_then(|t| t.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn weekday_abbrev(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.format("%a").to_string())
        .unwrap_or_default()
}
