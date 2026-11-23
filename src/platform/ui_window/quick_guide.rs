use std::io;

use windows_reactor::*;

use super::QUICK_GUIDE_TITLE;
use super::protocol::{Frames, GuidePreferences, parse_preferences};
use crate::settings::Modifier;

const CARD_STROKE: Brush = Brush::Theme(ThemeBrush::CardStroke);
const CARD_BACKGROUND: Brush = Brush::Theme(ThemeBrush::CardBackground);
const INLINE_SHORTCUTS_MIN_WIDTH: f64 = 500.0;

// Glyphs from Segoe Fluent Icons, supplied by WinUI's symbol font resource.
const MOVE: &str = "\u{e7c2}";
const RESIZE: &str = "\u{e740}";
const RESTORE: &str = "\u{e923}";
const TILE: &str = "\u{e90c}";
const MOUSE: &str = "\u{e962}";
const INFO: &str = "\u{e946}";

const WINDOWS_KEY_PATH: &str = concat!(
    "M0,0 H6.5 V6.5 H0 Z ",
    "M7.5,0 H14 V6.5 H7.5 Z ",
    "M0,7.5 H6.5 V14 H0 Z ",
    "M7.5,7.5 H14 V14 H7.5 Z",
);

pub(crate) struct QuickGuidePage {
    dismiss: ElementRef<Button>,
    preferences: GuidePreferences,
    compact: bool,
}

#[derive(Clone)]
pub(crate) enum Message {
    Close,
    SettingsChanged(GuidePreferences),
    SetCompact(bool),
}

impl Component for QuickGuidePage {
    type Input = GuidePreferences;
    type Message = Message;

    fn create(preferences: &GuidePreferences, context: &ComponentContext<Self>) -> Self {
        wait_for_settings(context);
        Self {
            dismiss: ElementRef::new(),
            preferences: *preferences,
            compact: false,
        }
    }

    fn update(&mut self, message: Message, context: &ComponentContext<Self>) {
        match message {
            Message::Close => {
                let _ = context.close_window();
            }
            Message::SettingsChanged(preferences) => {
                self.preferences = preferences;
                wait_for_settings(context);
            }
            Message::SetCompact(compact) => self.compact = compact,
        }
    }

    fn view(&self, _: &GuidePreferences, context: &mut ViewContext<Self>) -> View {
        context.window_title(QUICK_GUIDE_TITLE);
        context.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .client_size(568.0, 538.0)
                .constraints(WindowConstraints {
                    min_width: Some(330.0),
                    min_height: Some(250.0),
                    ..Default::default()
                }),
        );
        context.on_window_size(context.callback(|size: WindowSize| {
            Message::SetCompact(size.width < INLINE_SHORTCUTS_MIN_WIDTH)
        }));
        let dismiss = self.dismiss.clone();
        context.use_effect("focus dismiss button", (), move || {
            let _ = dismiss.request_focus();
            None
        });

        let modifier_name = match self.preferences.modifier {
            Modifier::Win => "the Windows key",
            Modifier::Alt => "Alt",
        };

        Grid::new()
            .key_accelerators(KeyAccelerators::new([KeyAccelerator::new(
                AcceleratorKey::Escape,
                AcceleratorModifiers::None,
                context.message(Message::Close),
            )]))
            .rows([GridLength::STAR, GridLength::Auto])
            .children((
                ScrollViewer::new()
                    .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
                    .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                    .content(
                        Border::new()
                            .padding(Thickness::new(24.0, 18.0, 24.0, 18.0))
                            .content(
                                StackPanel::new().max_width(720.0).spacing(14.0).children((
                                    StackPanel::new().spacing(4.0).children((
                                        text("Quick guide")
                                            .font_size(24.0)
                                            .font_weight(FontWeight::SEMI_BOLD)
                                            .automation_heading_level(
                                                AutomationHeadingLevel::Level1,
                                            ),
                                        text(format!(
                                            "Hold {modifier_name} while interacting with a window."
                                        ))
                                        .font_size(14.0)
                                        .opacity(0.76),
                                    )),
                                    self.gestures(),
                                    more_options(),
                                )),
                            ),
                    ),
                StackPanel::new().grid_row(1).children((
                    Border::new().height(1.0).background(CARD_STROKE),
                    Button::new()
                        .margin(Thickness::xy(24.0, 12.0))
                        .horizontal_alignment(HorizontalAlignment::Right)
                        .min_width(88.0)
                        .style(ButtonStyle::Accent)
                        .element_ref(&self.dismiss)
                        .on_click(context.message(Message::Close))
                        .content("Got it"),
                )),
            ))
            .into()
    }
}

fn wait_for_settings(context: &ComponentContext<QuickGuidePage>) {
    context.spawn_background(|_| {
        let update = Frames::new(io::stdin().lock()).next(parse_preferences);
        match update {
            Ok(Some(preferences)) => Message::SettingsChanged(preferences),
            _ => Message::Close,
        }
    });
}

impl QuickGuidePage {
    fn gestures(&self) -> View {
        let (tile_description, tile_shortcut) = if self.preferences.tiling {
            (
                "Left-drag to a screen edge or corner and release over the tiling preview.",
                Some("Drag to edge"),
            )
        } else {
            (
                "Turn on Tile on drop in Settings to use this gesture.",
                None,
            )
        };

        card(StackPanel::new().children((
            self.gesture(
                MOVE,
                "Move a window",
                "Drag to move the window.",
                Some("Left-drag"),
            ),
            row_separator(),
            self.gesture(
                RESIZE,
                "Resize a window",
                "Drag to resize the window.",
                Some("Right-drag"),
            ),
            row_separator(),
            self.gesture(
                RESTORE,
                "Maximize or restore",
                "Double-click to maximize or restore.",
                Some("Left double-click"),
            ),
            row_separator(),
            self.gesture(TILE, "Tile a window", tile_description, tile_shortcut),
            row_separator(),
            self.gesture(
                MOUSE,
                "Window actions",
                "Access actions like Always on top and Center on monitor.",
                Some("Middle-click"),
            ),
        )))
    }

    fn gesture(&self, glyph: &str, title: &str, description: &str, action: Option<&str>) -> View {
        let (columns, shortcut_row, shortcut_column, alignment): (&[_], _, _, _) = if self.compact {
            (
                &[GridLength::Auto, GridLength::STAR],
                1,
                1,
                HorizontalAlignment::Left,
            )
        } else {
            (
                &[GridLength::Auto, GridLength::STAR, GridLength::Auto],
                0,
                2,
                HorizontalAlignment::Right,
            )
        };
        let shortcut = match action {
            Some(action) => shortcut(self.preferences.modifier, action),
            None => secondary("Off").into(),
        };

        Grid::new()
            .margin(Thickness::xy(16.0, 8.0))
            .columns(columns.iter().copied())
            .rows([GridLength::Auto, GridLength::Auto])
            .column_spacing(12.0)
            .row_spacing(if self.compact { 6.0 } else { 0.0 })
            .children((
                icon(glyph).opacity(0.76),
                row_text(title, description),
                Border::new()
                    .grid_row(shortcut_row)
                    .grid_column(shortcut_column)
                    .horizontal_alignment(alignment)
                    .vertical_alignment(VerticalAlignment::Center)
                    .content(shortcut),
            ))
            .into()
    }
}

fn shortcut(modifier: Modifier, action: &str) -> View {
    let modifier: View = match modifier {
        Modifier::Win => PathIcon::new()
            .data(WINDOWS_KEY_PATH)
            .width(14.0)
            .height(14.0)
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .automation_name("Windows key")
            .into(),
        Modifier::Alt => keycap_text("Alt").into(),
    };
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(5.0)
        .children((
            keycap(modifier),
            secondary("+").vertical_alignment(VerticalAlignment::Center),
            keycap(keycap_text(action)),
        ))
        .into()
}

fn keycap(content: impl Into<View>) -> View {
    Border::new()
        .min_width(30.0)
        .min_height(26.0)
        .padding(Thickness::xy(6.0, 1.0))
        .background(CARD_BACKGROUND)
        .border_brush(CARD_STROKE)
        .border_thickness(1.0)
        .corner_radius(4.0)
        .content(content)
        .into()
}

fn keycap_text(label: &str) -> TextBlock {
    text(label)
        .font_size(12.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
}

fn more_options() -> View {
    card(
        Grid::new()
            .margin(Thickness::xy(16.0, 10.0))
            .columns([GridLength::Auto, GridLength::STAR])
            .column_spacing(12.0)
            .children((
                icon(INFO).opacity(0.76),
                row_text(
                    "More options",
                    "Find Settings and Pause in the notification area. Check the hidden icons if needed.",
                ),
            )),
    )
}

fn card(content: impl Into<View>) -> View {
    Border::new()
        .background(CARD_BACKGROUND)
        .border_brush(CARD_STROKE)
        .border_thickness(1.0)
        .corner_radius(8.0)
        .content(content)
        .into()
}

fn row_separator() -> Border {
    Border::new()
        .background(CARD_STROKE)
        .height(1.0)
        .margin(Thickness::xy(16.0, 0.0))
}

fn row_text(title: &str, description: &str) -> View {
    StackPanel::new()
        .grid_column(1)
        .spacing(2.0)
        .children((
            text(title)
                .font_size(14.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .automation_heading_level(AutomationHeadingLevel::Level2),
            secondary(description),
        ))
        .into()
}

fn icon(glyph: &str) -> FontIcon {
    FontIcon::new()
        .glyph(glyph)
        .width(20.0)
        .vertical_alignment(VerticalAlignment::Center)
}

fn secondary(value: &str) -> TextBlock {
    text(value).font_size(12.0).opacity(0.76)
}

fn text(value: impl AsRef<str>) -> TextBlock {
    TextBlock::new()
        .text(value)
        .text_wrapping(TextWrapping::Wrap)
}
