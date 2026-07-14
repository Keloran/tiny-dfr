//! Shared helpers for the integration tests.

use tiny_dfr::{Button, ButtonImage, FunctionLayer, NotificationButton};

pub fn test_button(image: ButtonImage) -> Button {
    Button {
        image,
        changed: false,
        active: false,
        action: vec![],
        command: None,
        icon_width: 0.0,
        icon_height: 0.0,
        stacked: false,
        font_size: None,
        max_title_length: None,
        toggle_target: None,
        hold_started: None,
    }
}

pub fn notification_layer() -> FunctionLayer {
    FunctionLayer {
        name: "Notifications".to_string(),
        displays_time: false,
        displays_battery: false,
        displays_sysinfo: false,
        displays_slider: false,
        displays_weather: false,
        displays_notifications: true,
        buttons: vec![
            (
                0.0,
                test_button(ButtonImage::Notification(NotificationButton::Previous, None, None)),
            ),
            (
                1.0,
                test_button(ButtonImage::Notification(NotificationButton::Text, None, None)),
            ),
            (
                9.0,
                test_button(ButtonImage::Notification(NotificationButton::Next, None, None)),
            ),
            (
                10.0,
                test_button(ButtonImage::Notification(NotificationButton::Dnd, None, None)),
            ),
        ],
        virtual_button_count: 11.0,
        faster_refresh: false,
        overlay_parent: None,
        overlay_coverage: 0.0,
        overlay_reveal: 1.0,
        overlay_slide_start_x: None,
    }
}
