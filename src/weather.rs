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
    pub desc: String,
    pub tmax: f64,
    pub tmin: f64,
}

#[derive(Clone, Default)]
pub struct WeatherData {
    pub available: bool,
    pub city: String,
    pub current_temp: Option<f64>,
    pub current_desc: String,
    pub unit: String,
    pub days: Vec<DayForecast>,
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
    let current_desc = desc_value(current).unwrap_or_default();

    let city = v
        .get("nearest_area")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|x| x.get("areaName"))
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|x| x.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut days = Vec::new();
    if let Some(weather) = v.get("weather").and_then(|w| w.as_array()) {
        for day in weather {
            let date = day.get("date").and_then(|d| d.as_str()).unwrap_or("");
            let tmax = parse_temp(day.get(max_key));
            let tmin = parse_temp(day.get(min_key));
            // Representative condition: the midday hourly slot if present.
            let desc = day
                .get("hourly")
                .and_then(|h| h.as_array())
                .and_then(|hours| {
                    hours
                        .iter()
                        .find(|h| h.get("time").and_then(|t| t.as_str()) == Some("1200"))
                        .or_else(|| hours.get(hours.len() / 2))
                })
                .and_then(desc_value)
                .unwrap_or_default();
            days.push(DayForecast {
                weekday: weekday_abbrev(date),
                desc,
                tmax,
                tmin,
            });
        }
    }

    Some(WeatherData {
        available: true,
        city,
        current_temp,
        current_desc,
        unit: if fahrenheit { "°F" } else { "°C" }.to_string(),
        days,
    })
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
