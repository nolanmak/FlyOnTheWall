//! The menu-bar item.
//!
//! `tray-icon` owns the `NSStatusItem`; we hold a strong reference to it for
//! the process's whole life. **Dropping the last strong reference silently
//! removes the menu-bar icon** — no error, no log line — and the app looks
//! like it failed to launch (docs/REQUIREMENTS.md 5.5).
//!
//! The `NSMenu` is built exactly once. Every render is `set_text` /
//! `set_enabled` on rows that already exist, so the menu never rebuilds under
//! an open cursor and there is no code path that omits a row.

use muda::{Menu, MenuItem, PredefinedMenuItem};
use objc2::rc::Retained;
use objc2_app_kit::NSStatusItem;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::error::ShellError;
use crate::icon::{ICON_PIXELS, is_template, tray_icon_rgba};
use crate::view::{MenuAction, MenuModel, TrayState, TrayView};

/// The status item and its menu.
pub(crate) struct Tray {
    icon: TrayIcon,

    /// The `NSStatusItem` behind `tray-icon`.
    ///
    /// **Held deliberately.** This field is never read; its only job is to
    /// keep the status item alive. Dropping it removes the icon from the menu
    /// bar with no diagnostic at all.
    _status_item: Retained<NSStatusItem>,

    /// Held for the same reason: `TrayIcon::set_menu` borrows it, and a
    /// dropped `Menu` takes the rows with it.
    _menu: Menu,

    status_row: MenuItem,
    rows: Vec<(MenuAction, MenuItem)>,

    last_state: Option<TrayState>,
    /// The menu-bar title as last applied. `None` is both "never applied" and
    /// "no title", which coincide: `TrayIconBuilder` sets no title, so the
    /// initial state really is no title.
    last_title: Option<String>,
    last_tooltip: Option<String>,
    last_menu: Option<MenuModel>,
}

impl Tray {
    /// Create the status item.
    ///
    /// # Errors
    ///
    /// If the icon cannot be rasterised, if `tray-icon` refuses (which it does
    /// off the main thread), or if it hands back no `NSStatusItem`.
    pub(crate) fn new(model: &MenuModel) -> Result<Self, ShellError> {
        let menu = Menu::new();
        let status_row = MenuItem::with_id("fotw.status", &model.status, false, None);
        menu.append(&status_row).map_err(menu_err)?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(menu_err)?;

        let mut rows = Vec::with_capacity(MenuAction::ALL.len());
        for action in MenuAction::ALL {
            let button = model.button(action);
            let item = MenuItem::with_id(action.id(), &button.label, button.enabled, None);
            menu.append(&item).map_err(menu_err)?;
            // Grouping, so Quit is not one slip away from Stop Recording.
            if matches!(
                action,
                MenuAction::ToggleRecording | MenuAction::DisclosureKit | MenuAction::About
            ) {
                menu.append(&PredefinedMenuItem::separator())
                    .map_err(menu_err)?;
            }
            rows.push((action, item));
        }

        let icon = TrayIconBuilder::new()
            .with_id("fotw.tray")
            .with_menu(Box::new(menu.clone()))
            .with_icon(make_icon(TrayState::Idle)?)
            .with_icon_as_template(is_template(TrayState::Idle))
            .with_tooltip("FlyOnTheWall")
            // Left click opens the menu too: on a menu-bar-only app there is
            // no other surface for it to reach.
            .with_menu_on_left_click(true)
            .build()
            .map_err(|e| ShellError::StatusItem(e.to_string()))?;

        let status_item = icon.ns_status_item().ok_or_else(|| {
            ShellError::StatusItem(
                "tray-icon created no NSStatusItem; the menu bar item would vanish on the next drop"
                    .to_owned(),
            )
        })?;

        Ok(Self {
            icon,
            _status_item: status_item,
            _menu: menu,
            status_row,
            rows,
            last_state: None,
            last_title: None,
            last_tooltip: None,
            last_menu: None,
        })
    }

    /// Apply a view. Every write is guarded by a comparison against the last
    /// one applied, because this runs at the pump rate.
    pub(crate) fn render(&mut self, view: &TrayView, model: &MenuModel) {
        if self.last_state != Some(view.state) {
            if let Ok(icon) = make_icon(view.state) {
                let _ = self
                    .icon
                    .set_icon_with_as_template(Some(icon), is_template(view.state));
            }
            self.last_state = Some(view.state);
        }

        if self.last_title != view.title {
            self.icon.set_title(view.title.clone());
            self.last_title.clone_from(&view.title);
        }

        if self.last_tooltip.as_ref() != Some(&view.tooltip) {
            let _ = self.icon.set_tooltip(Some(&view.tooltip));
            self.last_tooltip = Some(view.tooltip.clone());
        }

        if self.last_menu.as_ref() != Some(model) {
            self.status_row.set_text(&model.status);
            for (action, item) in &self.rows {
                let button = model.button(*action);
                item.set_text(&button.label);
                item.set_enabled(button.enabled);
            }
            self.last_menu = Some(model.clone());
        }
    }
}

fn make_icon(state: TrayState) -> Result<Icon, ShellError> {
    Icon::from_rgba(tray_icon_rgba(state), ICON_PIXELS, ICON_PIXELS)
        .map_err(|e| ShellError::StatusItem(format!("menu-bar icon: {e}")))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "passed as a `map_err` argument, which hands over the error by value"
)]
fn menu_err(e: muda::Error) -> ShellError {
    ShellError::StatusItem(format!("menu: {e}"))
}
