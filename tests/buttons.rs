//! Button model / color / formatting tests.

use tiny_dfr::*;

#[test]
fn time_option_aliases_normalize() {
    assert_eq!(normalize_time_option("24h".to_string()), "24hr");
    assert_eq!(normalize_time_option("12h".to_string()), "12hr");
    assert_eq!(normalize_time_option("%H:%M".to_string()), "%H:%M");
}

#[test]
fn notification_aliases_map_to_buttons() {
    assert!(matches!(
        notification_button_alias("notifications"),
        Some(NotificationButton::Count)
    ));
    assert!(matches!(
        notification_button_alias("PreviousLayer"),
        Some(NotificationButton::Back)
    ));
    assert!(matches!(
        notification_button_alias("DnDNotification"),
        Some(NotificationButton::Dnd)
    ));
    assert!(notification_button_alias("nope").is_none());
}

#[test]
fn usage_color_thresholds_match_configured_ranges() {
    assert_eq!(usage_color(10.0), (1.0, 1.0, 1.0));
    assert_eq!(usage_color(11.0), (1.0, 0.85, 0.0));
    assert_eq!(usage_color(25.0), (1.0, 0.85, 0.0));
    assert_eq!(usage_color(26.0), (1.0, 0.45, 0.0));
    assert_eq!(usage_color(55.0), (1.0, 0.45, 0.0));
    assert_eq!(usage_color(56.0), (0.55, 0.25, 1.0));
    assert_eq!(usage_color(75.0), (0.55, 0.25, 1.0));
    assert_eq!(usage_color(76.0), (1.0, 0.0, 0.0));
}

#[test]
fn battery_color_thresholds_match_configured_ranges() {
    assert_eq!(battery_color(10), (1.0, 0.0, 0.0));
    assert_eq!(battery_color(11), (0.55, 0.25, 1.0));
    assert_eq!(battery_color(25), (0.55, 0.25, 1.0));
    assert_eq!(battery_color(26), (1.0, 0.45, 0.0));
    assert_eq!(battery_color(50), (1.0, 0.45, 0.0));
    assert_eq!(battery_color(51), (0.0, 0.45, 1.0));
    assert_eq!(battery_color(75), (0.0, 0.45, 1.0));
    assert_eq!(battery_color(76), (0.0, 0.8, 0.0));
}

#[test]
fn battery_warning_requires_colorize_and_low_capacity() {
    assert!(battery_warning(10, BatteryState::NotCharging, true));
    assert!(!battery_warning(11, BatteryState::NotCharging, true));
    assert!(!battery_warning(10, BatteryState::NotCharging, false));
    assert!(!battery_warning(10, BatteryState::Charging, true));
}

#[test]
fn titlebar_and_workspace_icon_colors_require_colorize() {
    assert_eq!(now_playing_icon_color(true), Some((0.0, 0.8, 0.0)));
    assert_eq!(now_playing_icon_color(false), None);
    assert_eq!(workspace_icon_color(true), Some((0.55, 0.25, 1.0)));
    assert_eq!(workspace_icon_color(false), None);
}

#[test]
fn weather_colorizes_only_sun_moon_and_rain() {
    assert_eq!(weather_icon_color("weather_sunny"), Some((1.0, 0.85, 0.0)));
    assert_eq!(weather_icon_color("weather_moon"), Some((1.0, 0.85, 0.0)));
    assert_eq!(weather_icon_color("weather_rainy"), None);
    assert_eq!(weather_icon_color("weather_cloudy"), None);
}

#[test]
fn brightness_slider_icons_blend_from_white_to_orange() {
    assert_eq!(
        slider_icon_color(SliderKind::DisplayBrightness, 0),
        Some((1.0, 1.0, 1.0))
    );
    let orange = slider_icon_color(SliderKind::KeyboardBacklight, 100).unwrap();
    assert!((orange.0 - 1.0).abs() < f64::EPSILON);
    assert!((orange.1 - 0.45).abs() < 0.0001);
    assert!((orange.2 - 0.0).abs() < f64::EPSILON);
    assert_eq!(slider_icon_color(SliderKind::Volume, 100), None);
}

