use serde_json::Value;
use std::{
    process::Command,
    time::{Duration, Instant},
};

const REFRESH: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Notification {
    id: u64,
    app: String,
    summary: String,
    body: String,
    actions: Vec<NotificationAction>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotificationAction {
    key: String,
    pub label: String,
}

pub struct NotificationManager {
    notifications: Vec<Notification>,
    index: usize,
    dnd: bool,
    last_refresh: Instant,
}

impl NotificationManager {
    #[cfg(test)]
    pub(crate) fn with_count(count: usize) -> NotificationManager {
        NotificationManager {
            notifications: (0..count)
                .map(|id| Notification {
                    id: id as u64,
                    app: "app".to_string(),
                    summary: format!("summary {id}"),
                    body: String::new(),
                    actions: Vec::new(),
                })
                .collect(),
            index: 0,
            dnd: false,
            last_refresh: Instant::now(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_actions(labels: &[&str]) -> NotificationManager {
        NotificationManager {
            notifications: vec![Notification {
                id: 1,
                app: "app".to_string(),
                summary: "summary".to_string(),
                body: String::new(),
                actions: labels
                    .iter()
                    .map(|label| NotificationAction {
                        key: label.to_lowercase(),
                        label: label.to_string(),
                    })
                    .collect(),
            }],
            index: 0,
            dnd: false,
            last_refresh: Instant::now(),
        }
    }

    pub fn new() -> NotificationManager {
        let mut mgr = NotificationManager {
            notifications: Vec::new(),
            index: 0,
            dnd: false,
            last_refresh: Instant::now() - REFRESH,
        };
        mgr.refresh();
        mgr
    }

    pub fn refresh(&mut self) -> bool {
        if self.last_refresh.elapsed() < REFRESH {
            return false;
        }
        self.last_refresh = Instant::now();

        let old = (self.notifications.clone(), self.index, self.dnd);
        self.notifications = load_notifications();
        if self.index >= self.notifications.len() {
            self.index = self.notifications.len().saturating_sub(1);
        }
        self.dnd = load_dnd();

        old != (self.notifications.clone(), self.index, self.dnd)
    }

    pub fn count_text(&self) -> String {
        self.notifications.len().to_string()
    }

    pub fn current_text(&self) -> String {
        let Some(n) = self.notifications.get(self.index) else {
            return "No notifications".to_string();
        };

        let mut parts = Vec::new();
        if !n.app.is_empty() {
            parts.push(n.app.as_str());
        }
        if !n.summary.is_empty() {
            parts.push(n.summary.as_str());
        }
        if !n.body.is_empty() {
            parts.push(n.body.as_str());
        }
        parts.join(": ")
    }

    pub fn dnd_text(&self) -> String {
        if self.dnd {
            "DnD on".to_string()
        } else {
            "DnD off".to_string()
        }
    }

    pub fn dnd_enabled(&self) -> bool {
        self.dnd
    }

    pub fn previous(&mut self) {
        self.index = self.index.saturating_sub(1);
    }

    pub fn next(&mut self) {
        if self.index + 1 < self.notifications.len() {
            self.index += 1;
        }
    }

    pub fn can_navigate(&self) -> bool {
        self.notifications.len() > 1
    }

    pub fn invoke_current(&self) {
        let Some(n) = self.notifications.get(self.index) else {
            return;
        };
        if !n.actions.is_empty() {
            let _ = Command::new("makoctl")
                .arg("invoke")
                .arg("-n")
                .arg(n.id.to_string())
                .spawn();
        }
    }

    pub fn current_actions(&self) -> &[NotificationAction] {
        self.notifications
            .get(self.index)
            .map(|n| n.actions.as_slice())
            .unwrap_or(&[])
    }

    pub fn invoke_action(&self, action_index: usize) {
        let Some(n) = self.notifications.get(self.index) else {
            return;
        };
        let Some(action) = n.actions.get(action_index) else {
            return;
        };
        let _ = Command::new("makoctl")
            .arg("invoke")
            .arg("-n")
            .arg(n.id.to_string())
            .arg(action.key.as_str())
            .spawn();
    }

    pub fn dismiss_current(&mut self) {
        let Some(n) = self.notifications.get(self.index) else {
            return;
        };
        let _ = Command::new("makoctl")
            .arg("dismiss")
            .arg("-n")
            .arg(n.id.to_string())
            .status();
        self.last_refresh = Instant::now() - REFRESH;
        self.refresh();
    }

    pub fn toggle_dnd(&mut self) {
        let _ = Command::new("makoctl")
            .arg("mode")
            .arg("-t")
            .arg("do-not-disturb")
            .status();
        self.last_refresh = Instant::now() - REFRESH;
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_requires_multiple_notifications() {
        assert!(!NotificationManager::with_count(0).can_navigate());
        assert!(!NotificationManager::with_count(1).can_navigate());
        assert!(NotificationManager::with_count(2).can_navigate());
    }

    #[test]
    fn next_and_previous_clamp_to_available_notifications() {
        let mut mgr = NotificationManager::with_count(2);
        assert_eq!(mgr.current_text(), "app: summary 0");
        mgr.next();
        assert_eq!(mgr.current_text(), "app: summary 1");
        mgr.next();
        assert_eq!(mgr.current_text(), "app: summary 1");
        mgr.previous();
        assert_eq!(mgr.current_text(), "app: summary 0");
        mgr.previous();
        assert_eq!(mgr.current_text(), "app: summary 0");
    }
}

fn load_notifications() -> Vec<Notification> {
    let Ok(output) = Command::new("makoctl").arg("list").arg("-j").output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| {
            let actions = value
                .get("actions")
                .and_then(Value::as_object)
                .map(|actions| {
                    let mut actions = actions
                        .iter()
                        .filter_map(|(key, label)| {
                            Some(NotificationAction {
                                key: key.clone(),
                                label: label.as_str()?.to_string(),
                            })
                        })
                        .collect::<Vec<_>>();
                    actions.sort_by(|a, b| a.label.cmp(&b.label));
                    actions
                })
                .unwrap_or_default();
            Some(Notification {
                id: value.get("id")?.as_u64()?,
                app: string_field(&value, "app_name"),
                summary: string_field(&value, "summary"),
                body: string_field(&value, "body"),
                actions,
            })
        })
        .collect()
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn load_dnd() -> bool {
    Command::new("makoctl")
        .arg("mode")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|modes| modes.lines().any(|mode| mode.trim() == "do-not-disturb"))
        .unwrap_or(false)
}
