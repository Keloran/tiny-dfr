//! FunctionLayer layout, hit-testing, and pullout animation tests.

use std::time::Instant;

use input_linux::Key;
use tiny_dfr::*;

mod common;
use common::{notification_layer, test_button};

#[test]
fn pullout_panel_hits_child_buttons() {
    // Panel = ">" toggle + interactive children, laid out over the right
    // slice of the parent (coverage 0.66).
    let child = |t: &str| ButtonConfig {
        text: Some(t.to_string()),
        action: vec![Key::A],
        ..ButtonConfig::default()
    };
    let toggle = ButtonConfig {
        button: Some("pullout_toggle".to_string()),
        text: Some(">".to_string()),
        layer_toggle: Some("SystemInfo".to_string()),
        ..ButtonConfig::default()
    };
    let mut panel = FunctionLayer::with_config(
        "SystemInfo__pullout",
        vec![toggle, child("cpu"), child("mem"), child("bat"), child("time")],
    );
    panel.set_overlay("SystemInfo".to_string(), 0.66);

    let (width, height) = (1000u16, 60u16);
    let origin_x = panel.overlay_origin(width as f64);
    let y = height as f64 * 0.5;

    // The ">" toggle sits at the far left of the panel and must hit.
    let mid_toggle = origin_x + 1.0;
    assert_eq!(
        panel.hit(width, height, mid_toggle, y, None, None),
        Some(0),
        "\">\" toggle should be hittable"
    );

    // Each interactive child must be hittable at its centre.
    let spans = panel.button_spans(None);
    let visible = panel.buttons.len();
    let total_spacing = BUTTON_SPACING_PX as f64 * (visible - 1) as f64;
    let vbw = (width as f64 - origin_x - total_spacing) / panel.virtual_button_count;
    for (i, (start, end)) in spans.iter().enumerate() {
        let left = start * vbw + i as f64 * BUTTON_SPACING_PX as f64 + origin_x;
        let right = left + (end - start) * vbw;
        let cx = (left + right) / 2.0;
        assert_eq!(
            panel.hit(width, height, cx, y, None, None),
            Some(i),
            "child button {i} at x={cx} should hit"
        );
    }
}

#[test]
fn pullout_slide_reveals_over_its_duration() {
    let start = Instant::now();
    let expand = PulloutAnim {
        panel: 1,
        parent: 0,
        collapsing: false,
        start,
    };
    assert!(expand.reveal(start).abs() < 1e-9, "expand starts hidden");
    assert!(
        (expand.reveal(start + PULLOUT_ANIM) - 1.0).abs() < 1e-9,
        "expand ends fully revealed"
    );
    assert!(!expand.done(start));
    assert!(expand.done(start + PULLOUT_ANIM));

    let collapse = PulloutAnim {
        collapsing: true,
        start,
        ..expand
    };
    assert!(
        (collapse.reveal(start) - 1.0).abs() < 1e-9,
        "collapse starts fully revealed"
    );
    assert!(
        collapse.reveal(start + PULLOUT_ANIM).abs() < 1e-9,
        "collapse ends hidden"
    );
}

#[test]
fn begin_layer_switch_animates_only_pullout_transitions() {
    let btn = |t: &str| ButtonConfig {
        text: Some(t.to_string()),
        action: vec![Key::A],
        ..ButtonConfig::default()
    };
    let parent = FunctionLayer::with_config("SystemInfo", vec![btn("a")]);
    let mut panel =
        FunctionLayer::with_config("SystemInfo__pullout", vec![btn(">"), btn("cpu")]);
    panel.set_overlay("SystemInfo".to_string(), 0.66);
    let plain = FunctionLayer::with_config("Media", vec![btn("m")]);
    let layers = vec![parent, panel, plain];
    let now = Instant::now();

    // parent (0) -> panel (1): expand — panel active, slides in.
    let (active, anim) = begin_layer_switch(&layers, 0, 1, now);
    assert_eq!(active, 1);
    let a = anim.expect("expand should animate");
    assert!(!a.collapsing);
    assert_eq!((a.panel, a.parent), (1, 0));

    // panel (1) -> parent (0): collapse — panel stays active, slides out.
    let (active, anim) = begin_layer_switch(&layers, 1, 0, now);
    assert_eq!(active, 1);
    let a = anim.expect("collapse should animate");
    assert!(a.collapsing);
    assert_eq!((a.panel, a.parent), (1, 0));

    // parent (0) -> Media (2): a normal layer switch is instant.
    let (active, anim) = begin_layer_switch(&layers, 0, 2, now);
    assert_eq!(active, 2);
    assert!(anim.is_none());
}


#[test]
fn notification_nav_space_goes_to_content_until_multiple_notifications() {
    let layer = notification_layer();

    let empty = NotificationManager::with_count(0);
    assert_eq!(
        layer.button_spans(Some(&empty)),
        vec![(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 11.0)]
    );

    let one = NotificationManager::with_count(1);
    assert_eq!(
        layer.button_spans(Some(&one)),
        vec![(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 11.0)]
    );

    let two = NotificationManager::with_count(2);
    assert_eq!(
        layer.button_spans(Some(&two)),
        vec![(0.0, 1.0), (1.0, 9.0), (9.0, 10.0), (10.0, 11.0)]
    );
}

#[test]
fn notification_actions_shrink_content_by_two_each() {
    let layer = notification_layer();
    let mgr = NotificationManager::with_actions(&["Reply", "Archive"]);

    assert_eq!(
        layer.button_spans(Some(&mgr)),
        vec![
            (0.0, 0.0),
            (0.0, 6.0),
            (6.0, 8.0),
            (8.0, 10.0),
            (10.0, 10.0),
            (10.0, 11.0)
        ]
    );
}

#[test]
fn fractional_stretch_is_preserved_in_layer_spans() {
    let layer = FunctionLayer {
        name: "Fractional".to_string(),
        displays_time: false,
        displays_battery: false,
        displays_sysinfo: false,
        displays_slider: false,
        displays_weather: false,
        displays_notifications: false,
        buttons: vec![
            (0.0, test_button(ButtonImage::Text("a".to_string()))),
            (0.5, test_button(ButtonImage::Text("b".to_string()))),
            (2.0, test_button(ButtonImage::Text("c".to_string()))),
        ],
        virtual_button_count: 3.0,
        faster_refresh: false,
        overlay_parent: None,
        overlay_coverage: 0.0,
        overlay_reveal: 1.0,
        overlay_slide_start_x: None,
    };

    assert_eq!(
        layer.button_spans(None),
        vec![(0.0, 0.5), (0.5, 2.0), (2.0, 3.0)]
    );
}

#[test]
fn active_window_button_is_hittable_for_toggle() {
    let layer = FunctionLayer {
        name: "Titlebar".to_string(),
        displays_time: false,
        displays_battery: false,
        displays_sysinfo: true,
        displays_slider: false,
        displays_weather: false,
        displays_notifications: false,
        buttons: vec![(0.0, test_button(ButtonImage::ActiveWindow(None, false)))],
        virtual_button_count: 1.0,
        faster_refresh: false,
        overlay_parent: None,
        overlay_coverage: 0.0,
        overlay_reveal: 1.0,
        overlay_slide_start_x: None,
    };

    assert_eq!(layer.hit(100, 30, 50.0, 15.0, None, None), Some(0));
}
