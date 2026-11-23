use std::rc::Rc;

use windows_reactor::*;

use super::{SETTINGS_TITLE, SettingsSession};
use crate::settings::{Modifier, Settings};

const PROJECT_URL: &str = env!("CARGO_PKG_REPOSITORY");
const CARD_STROKE: Brush = Brush::Theme(ThemeBrush::CardStroke);
const MODIFIERS: [(Modifier, &str); 2] =
    [(Modifier::Win, "Windows key"), (Modifier::Alt, "Alt key")];

pub(crate) struct SettingsPage {
    session: SettingsSession,
    draft: Settings,
    start_at_sign_in: bool,
    editor: Option<String>,
    editor_error: Option<String>,
    error: Option<(&'static str, String)>,
    modifier_field: ElementRef<ComboBox>,
    name_field: ElementRef<TextBox>,
    add_button: ElementRef<Button>,
    focus_add_button: usize,
}

#[derive(Clone)]
pub(crate) enum Message {
    ModifierChanged(Option<usize>),
    SnapChanged(bool),
    TileChanged(bool),
    MoveMaximizedChanged(bool),
    ResizeMaximizedChanged(bool),
    StartupChanged(bool),
    ShowApplicationEditor,
    EditApplicationName(String),
    AddApplication,
    CloseApplicationEditor,
    RemoveApplication(usize),
    OpenSettingsFile,
    DismissError,
    Apply,
    Save,
    Cancel,
}

impl Component for SettingsPage {
    type Input = SettingsSession;
    type Message = Message;

    fn create(session: &SettingsSession, context: &ComponentContext<Self>) -> Self {
        if session.has_parent() {
            context.spawn_background(|_| {
                SettingsSession::wait_for_parent_exit();
                Message::Cancel
            });
        }
        Self {
            session: session.clone(),
            draft: session.draft(),
            start_at_sign_in: session.start_at_sign_in().unwrap_or(false),
            editor: None,
            editor_error: None,
            error: None,
            modifier_field: ElementRef::new(),
            name_field: ElementRef::new(),
            add_button: ElementRef::new(),
            focus_add_button: 0,
        }
    }

    fn update(&mut self, message: Message, context: &ComponentContext<Self>) {
        match message {
            Message::ModifierChanged(index) => {
                if let Some(&(modifier, _)) = index.and_then(|index| MODIFIERS.get(index)) {
                    self.draft.modifier = modifier;
                }
            }
            Message::SnapChanged(value) => self.draft.snapping = value,
            Message::TileChanged(value) => self.draft.tiling = value,
            Message::MoveMaximizedChanged(value) => {
                self.draft.window_permissions.move_maximized = value;
            }
            Message::ResizeMaximizedChanged(value) => {
                self.draft.window_permissions.resize_maximized = value;
            }
            Message::StartupChanged(value) => self.start_at_sign_in = value,
            Message::ShowApplicationEditor if self.editor.is_none() => {
                self.editor = Some(String::new());
            }
            Message::ShowApplicationEditor => {
                let _ = self.name_field.request_focus();
            }
            Message::EditApplicationName(name) => {
                if let Some(current) = &mut self.editor {
                    *current = name;
                    self.editor_error = None;
                }
            }
            Message::AddApplication => {
                if let Some(name) = &self.editor {
                    let name = name.trim();
                    match Settings::validate_excluded_app(name) {
                        Ok(()) => {
                            self.draft.excluded_apps.push(name.to_owned());
                            self.close_editor();
                        }
                        Err(error) => self.editor_error = Some(error),
                    }
                }
            }
            Message::CloseApplicationEditor => self.close_editor(),
            Message::RemoveApplication(index) => {
                if index < self.draft.excluded_apps.len() {
                    self.draft.excluded_apps.remove(index);
                    self.focus_add_button += 1;
                }
            }
            Message::OpenSettingsFile => {
                self.error = self
                    .session
                    .open_file()
                    .err()
                    .map(|error| ("Could not open the settings file", error.to_string()));
            }
            Message::DismissError => self.error = None,
            Message::Apply | Message::Save if self.can_commit() => {
                match self.session.save(&self.draft, self.start_at_sign_in) {
                    Ok(()) => {
                        self.error = None;
                        if matches!(message, Message::Save) {
                            let _ = context.close_window();
                        }
                    }
                    Err(error) => self.error = Some(("Settings update failed", error.to_string())),
                }
            }
            Message::Apply | Message::Save => (),
            Message::Cancel => {
                let _ = context.close_window();
            }
        }
    }

    fn view(&self, _: &SettingsSession, context: &mut ViewContext<Self>) -> View {
        context.window_title(SETTINGS_TITLE);
        context.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .client_size(648.0, 700.0)
                .constraints(WindowConstraints {
                    min_width: Some(470.0),
                    min_height: Some(390.0),
                    ..Default::default()
                }),
        );
        let modifier = self.modifier_field.clone();
        context.use_effect("focus activation modifier", (), move || {
            let _ = modifier.request_focus();
            None
        });

        if self.editor.is_some() {
            let field = self.name_field.clone();
            context.use_effect("focus application name", (), move || {
                let _ = field.request_focus();
                None
            });
        }
        if self.focus_add_button > 0 {
            let button = self.add_button.clone();
            context.use_effect(
                "return focus to add application",
                self.focus_add_button,
                move || {
                    let _ = button.request_focus();
                    None
                },
            );
        }

        let cards = StackPanel::new().max_width(760.0).spacing(16.0).children((
            self.window_management(context),
            self.maximized_windows(context),
            self.applications(context),
            self.general(context),
        ));

        Grid::new()
            .key_accelerators(KeyAccelerators::new([KeyAccelerator::new(
                AcceleratorKey::Escape,
                AcceleratorModifiers::None,
                context.message(Message::Cancel),
            )]))
            .rows([GridLength::STAR, GridLength::Auto])
            .children((
                ScrollViewer::new()
                    .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
                    .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                    .content(
                        // Keep the scroll extent as wide as the viewport so the
                        // width cap affects only the cards inside it.
                        Border::new()
                            .padding(Thickness::new(24.0, 18.0, 24.0, 20.0))
                            .content(cards),
                    ),
                self.footer(context),
            ))
            .into()
    }
}

impl SettingsPage {
    fn can_commit(&self) -> bool {
        self.session.has_changes(&self.draft, self.start_at_sign_in) && !self.has_pending_name()
    }

    fn has_pending_name(&self) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|name| !name.trim().is_empty())
    }

    fn close_editor(&mut self) {
        self.editor = None;
        self.editor_error = None;
        self.focus_add_button += 1;
    }

    fn window_management(&self, context: &ViewContext<Self>) -> View {
        let selected = MODIFIERS
            .iter()
            .position(|&(modifier, _)| modifier == self.draft.modifier);
        let modifier = ComboBox::new()
            .element_ref(&self.modifier_field)
            .items_source(MODIFIERS.map(|(_, label)| label))
            .selected_index(selected)
            .on_selection_changed(context.callback(Message::ModifierChanged))
            .min_width(130.0)
            .automation_name("Activation modifier");
        section(
            "Window management",
            Symbol::ViewAll,
            StackPanel::new().children((
                setting_row(
                    "Activation modifier",
                    "Hold this key to move or resize windows.",
                    modifier,
                ),
                divider(),
                setting_row(
                    "Snap window edges",
                    "Automatically snap windows to nearby edges.",
                    toggle(
                        "Snap window edges",
                        self.draft.snapping,
                        context.callback(Message::SnapChanged),
                    ),
                ),
                divider(),
                setting_row(
                    "Tile on drop",
                    "Tile windows when dropped at a screen edge.",
                    toggle(
                        "Tile on drop",
                        self.draft.tiling,
                        context.callback(Message::TileChanged),
                    ),
                ),
            )),
        )
    }

    fn maximized_windows(&self, context: &ViewContext<Self>) -> View {
        section(
            "Maximized windows",
            Symbol::FullScreen,
            StackPanel::new().children((
                setting_row(
                    "Move maximized windows",
                    "Allow dragging a maximized window into a new position.",
                    toggle(
                        "Move maximized windows",
                        self.draft.window_permissions.move_maximized,
                        context.callback(Message::MoveMaximizedChanged),
                    ),
                ),
                divider(),
                setting_row(
                    "Resize maximized windows",
                    "Allow resizing a maximized window with a drag.",
                    toggle(
                        "Resize maximized windows",
                        self.draft.window_permissions.resize_maximized,
                        context.callback(Message::ResizeMaximizedChanged),
                    ),
                ),
            )),
        )
    }

    fn applications(&self, context: &ViewContext<Self>) -> View {
        let entries = if self.draft.excluded_apps.is_empty() {
            StackPanel::new()
                .margin(Thickness::new(16.0, 0.0, 16.0, 12.0))
                .orientation(Orientation::Horizontal)
                .spacing(8.0)
                .children((
                    SymbolIcon::new().symbol(Symbol::List).opacity(0.72),
                    secondary("No excluded applications")
                        .vertical_alignment(VerticalAlignment::Center),
                ))
        } else {
            StackPanel::new().keyed_children(self.draft.excluded_apps.iter().enumerate().map(
                |(index, name)| {
                    KeyedView::new(
                        index,
                        StackPanel::new().children((
                            divider(),
                            Grid::new()
                                .margin(Thickness::xy(16.0, 5.0))
                                .column_spacing(12.0)
                                .columns([GridLength::STAR, GridLength::Auto])
                                .children((
                                    TextBlock::new()
                                        .text(name)
                                        .text_wrapping(TextWrapping::Wrap)
                                        .vertical_alignment(VerticalAlignment::Center),
                                    Button::new()
                                        .grid_column(1)
                                        .style(ButtonStyle::Subtle)
                                        .automation_name(format!("Remove {name}"))
                                        .on_click(
                                            context.message(Message::RemoveApplication(index)),
                                        )
                                        .content("Remove"),
                                )),
                        )),
                    )
                },
            ))
        };

        let editor = self.editor.as_ref().map(|name| {
            StackPanel::new()
                .margin(Thickness::new(16.0, 8.0, 16.0, 16.0))
                .spacing(8.0)
                .children(
                    [Grid::new()
                        .columns([GridLength::STAR, GridLength::Auto, GridLength::Auto])
                        .column_spacing(8.0)
                        .children((
                            TextBox::new()
                                .text(name)
                                .placeholder_text("e.g. notepad.exe")
                                .automation_name("Executable name")
                                .element_ref(&self.name_field)
                                .on_text_changed(context.callback(|name: Rc<str>| {
                                    Message::EditApplicationName(name.to_string())
                                })),
                            Button::new()
                                .grid_column(1)
                                .is_enabled(!name.trim().is_empty())
                                .on_click(context.message(Message::AddApplication))
                                .content("Add"),
                            Button::new()
                                .grid_column(2)
                                .style(ButtonStyle::Subtle)
                                .on_click(context.message(Message::CloseApplicationEditor))
                                .content("Discard entry"),
                        ))
                        .into()]
                    .into_iter()
                    .chain(self.editor_error.as_ref().map(|error| {
                        secondary(error)
                            .foreground(Brush::Theme(ThemeBrush::SystemCritical))
                            .into()
                    }))
                    .collect::<Vec<_>>(),
                )
        });
        let add_application = Button::new()
            .automation_name("Add application")
            .element_ref(&self.add_button)
            .on_click(context.message(Message::ShowApplicationEditor))
            .content(icon_label(Symbol::Add, "Add application"));

        section(
            "Applications",
            Symbol::AllApps,
            StackPanel::new().children(
                [
                    setting_row(
                        "Excluded applications",
                        "Window management ignores these applications.",
                        add_application,
                    ),
                    entries.into(),
                ]
                .into_iter()
                .chain(editor.map(Into::into))
                .collect::<Vec<_>>(),
            ),
        )
    }

    fn general(&self, context: &ViewContext<Self>) -> View {
        let startup_available = self.session.start_at_sign_in().is_some();
        let startup_description = if startup_available {
            "Launch Window Gestures when you sign in to Windows."
        } else {
            "The Windows Startup folder is unavailable."
        };
        let open = Button::new()
            .automation_name("Open settings file")
            .on_click(context.message(Message::OpenSettingsFile))
            .content(icon_label(Symbol::Document, "Open file"));
        section(
            "General",
            Symbol::Setting,
            StackPanel::new().children((
                setting_row(
                    "Start at sign-in",
                    startup_description,
                    toggle(
                        "Start at sign-in",
                        self.start_at_sign_in,
                        context.callback(Message::StartupChanged),
                    )
                    .is_enabled(startup_available),
                ),
                divider(),
                setting_row(
                    "Open settings file",
                    "View or edit the configuration file directly.",
                    open,
                ),
            )),
        )
    }

    fn footer(&self, context: &ViewContext<Self>) -> View {
        let can_commit = self.can_commit();
        let error = self.error.as_ref().map(|(title, message)| {
            InfoBar::new()
                .margin(Thickness::new(24.0, 0.0, 24.0, 12.0))
                .severity(InfoBarSeverity::Error)
                .title(*title)
                .message(message)
                .is_closable(true)
                .is_open(true)
                .on_closed(context.message(Message::DismissError))
        });
        let project_link = if PROJECT_URL.is_empty() {
            None
        } else {
            HyperlinkButton::new()
                .vertical_alignment(VerticalAlignment::Center)
                .navigate_uri(PROJECT_URL)
                .ok()
                .map(|link| link.content("GitHub"))
        };
        let version = Border::new()
            .vertical_alignment(VerticalAlignment::Center)
            .padding(Thickness::xy(8.0, 3.0))
            .background(ThemeBrush::CardBackground)
            .border_brush(CARD_STROKE)
            .border_thickness(1.0)
            .corner_radius(6.0)
            .content(
                TextBlock::new()
                    .text(concat!("v", env!("CARGO_PKG_VERSION")))
                    .automation_name(concat!("Version ", env!("CARGO_PKG_VERSION")))
                    .font_size(12.0)
                    .font_weight(FontWeight::SEMI_BOLD),
            );
        StackPanel::new()
            .grid_row(1)
            .children(
                error
                    .map(Into::into)
                    .into_iter()
                    .chain([
                        Border::new().height(1.0).background(CARD_STROKE).into(),
                        Grid::new()
                            .columns([GridLength::STAR, GridLength::Auto])
                            .margin(Thickness::xy(24.0, 12.0))
                            .children((
                                StackPanel::new()
                                    .orientation(Orientation::Horizontal)
                                    .vertical_alignment(VerticalAlignment::Center)
                                    .spacing(4.0)
                                    .children(
                                        [version.into()]
                                            .into_iter()
                                            .chain(project_link.map(Into::into))
                                            .collect::<Vec<_>>(),
                                    ),
                                StackPanel::new()
                                    .grid_column(1)
                                    .orientation(Orientation::Horizontal)
                                    .spacing(8.0)
                                    .children((
                                        Button::new()
                                            .min_width(88.0)
                                            .style(ButtonStyle::Accent)
                                            .is_enabled(can_commit)
                                            .on_click(context.message(Message::Save))
                                            .content("Save"),
                                        Button::new()
                                            .min_width(88.0)
                                            .on_click(context.message(Message::Cancel))
                                            .content("Cancel"),
                                        Button::new()
                                            .min_width(88.0)
                                            .is_enabled(can_commit)
                                            .on_click(context.message(Message::Apply))
                                            .content("Apply"),
                                    )),
                            ))
                            .into(),
                    ])
                    .collect::<Vec<_>>(),
            )
            .into()
    }
}

fn section(title: &str, icon: Symbol, settings: impl Into<View>) -> View {
    let heading = StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(8.0)
        .children((
            SymbolIcon::new().symbol(icon).opacity(0.78),
            TextBlock::new()
                .text(title)
                .font_size(17.0)
                .font_weight(FontWeight::SEMI_BOLD)
                .automation_heading_level(AutomationHeadingLevel::Level1),
        ));
    let card = Border::new()
        .background(Brush::Theme(ThemeBrush::CardBackground))
        .border_brush(CARD_STROKE)
        .border_thickness(1.0)
        .corner_radius(8.0)
        .content(settings);
    StackPanel::new()
        .spacing(8.0)
        .children((heading, card))
        .into()
}

fn setting_row(title: &str, description: &str, control: impl Into<View>) -> View {
    Grid::new()
        .margin(Thickness::xy(16.0, 0.0))
        .min_height(58.0)
        .column_spacing(16.0)
        .columns([GridLength::STAR, GridLength::Auto])
        .children((
            StackPanel::new()
                .vertical_alignment(VerticalAlignment::Center)
                .margin(Thickness::xy(0.0, 10.0))
                .spacing(2.0)
                .children((
                    TextBlock::new()
                        .text(title)
                        .font_size(14.0)
                        .font_weight(FontWeight::SEMI_BOLD)
                        .text_wrapping(TextWrapping::Wrap),
                    secondary(description),
                )),
            Border::new()
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center)
                .content(control),
        ))
        .into()
}

fn divider() -> Border {
    Border::new()
        .background(CARD_STROKE)
        .height(1.0)
        .margin(Thickness::xy(16.0, 0.0))
}

fn secondary(text: impl AsRef<str>) -> TextBlock {
    TextBlock::new()
        .text(text)
        .font_size(12.0)
        .opacity(0.76)
        .text_wrapping(TextWrapping::Wrap)
}

fn toggle(title: &str, on: bool, changed: Callback<bool>) -> ToggleSwitch {
    ToggleSwitch::new()
        .automation_name(title)
        .min_width(78.0)
        .is_on(on)
        .on_toggled(changed)
}

fn icon_label(icon: Symbol, label: &str) -> View {
    StackPanel::new()
        .orientation(Orientation::Horizontal)
        .spacing(6.0)
        .children((
            SymbolIcon::new().symbol(icon).opacity(0.78),
            TextBlock::new()
                .text(label)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .into()
}
