mod components;

use crate::components::ExtensionCard;
use client::telemetry::Telemetry;
use editor::{Editor, EditorElement, EditorStyle};
use extension::{ExtensionApiResponse, ExtensionManifest, ExtensionStatus, ExtensionStore};
use fuzzy::{match_strings, StringMatchCandidate};
use gpui::{
    actions, canvas, uniform_list, AnyElement, AppContext, EventEmitter, FocusableView, FontStyle,
    FontWeight, InteractiveElement, KeyContext, ParentElement, Render, Styled, Task, TextStyle,
    UniformListScrollHandle, View, ViewContext, VisualContext, WhiteSpace, WindowContext,
};
use settings::Settings;
use std::ops::DerefMut;
use std::time::Duration;
use std::{ops::Range, sync::Arc};
use theme::ThemeSettings;
use ui::{prelude::*, ToggleButton, Tooltip};
use util::ResultExt as _;
use workspace::{
    item::{Item, ItemEvent},
    Workspace, WorkspaceId,
};

actions!(zed, [Extensions, InstallDevExtension]);

pub fn init(cx: &mut AppContext) {
    cx.observe_new_views(move |workspace: &mut Workspace, _cx| {
        workspace
            .register_action(move |workspace, _: &Extensions, cx| {
                let extensions_page = ExtensionsPage::new(workspace, cx);
                workspace.add_item_to_active_pane(Box::new(extensions_page), cx)
            })
            .register_action(move |_, _: &InstallDevExtension, cx| {
                let store = ExtensionStore::global(cx);
                let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                });

                cx.deref_mut()
                    .spawn(|mut cx| async move {
                        let extension_path = prompt.await.log_err()??.pop()?;
                        store
                            .update(&mut cx, |store, cx| {
                                store
                                    .install_dev_extension(extension_path, cx)
                                    .detach_and_log_err(cx)
                            })
                            .ok()?;
                        Some(())
                    })
                    .detach();
            });
    })
    .detach();
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
enum ExtensionFilter {
    All,
    Installed,
    NotInstalled,
}

impl ExtensionFilter {
    pub fn include_dev_extensions(&self) -> bool {
        match self {
            Self::All | Self::Installed => true,
            Self::NotInstalled => false,
        }
    }
}

pub struct ExtensionsPage {
    list: UniformListScrollHandle,
    telemetry: Arc<Telemetry>,
    is_fetching_extensions: bool,
    filter: ExtensionFilter,
    remote_extension_entries: Vec<ExtensionApiResponse>,
    dev_extension_entries: Vec<Arc<ExtensionManifest>>,
    filtered_remote_extension_indices: Vec<usize>,
    query_editor: View<Editor>,
    query_contains_error: bool,
    _subscriptions: [gpui::Subscription; 2],
    extension_fetch_task: Option<Task<()>>,
}

impl ExtensionsPage {
    pub fn new(workspace: &Workspace, cx: &mut ViewContext<Workspace>) -> View<Self> {
        cx.new_view(|cx: &mut ViewContext<Self>| {
            let store = ExtensionStore::global(cx);
            let subscriptions = [
                cx.observe(&store, |_, _, cx| cx.notify()),
                cx.subscribe(&store, |this, _, event, cx| match event {
                    extension::Event::ExtensionsUpdated => this.fetch_extensions_debounced(cx),
                    _ => {}
                }),
            ];

            let query_editor = cx.new_view(|cx| {
                let mut input = Editor::single_line(cx);
                input.set_placeholder_text("Search extensions...", cx);
                input
            });
            cx.subscribe(&query_editor, Self::on_query_change).detach();

            let mut this = Self {
                list: UniformListScrollHandle::new(),
                telemetry: workspace.client().telemetry().clone(),
                is_fetching_extensions: false,
                filter: ExtensionFilter::All,
                dev_extension_entries: Vec::new(),
                filtered_remote_extension_indices: Vec::new(),
                remote_extension_entries: Vec::new(),
                query_contains_error: false,
                extension_fetch_task: None,
                _subscriptions: subscriptions,
                query_editor,
            };
            this.fetch_extensions(None, cx);
            this
        })
    }

    fn filter_extension_entries(&mut self, cx: &mut ViewContext<Self>) {
        let extension_store = ExtensionStore::global(cx).read(cx);

        self.filtered_remote_extension_indices.clear();
        self.filtered_remote_extension_indices.extend(
            self.remote_extension_entries
                .iter()
                .enumerate()
                .filter(|(_, extension)| match self.filter {
                    ExtensionFilter::All => true,
                    ExtensionFilter::Installed => {
                        let status = extension_store.extension_status(&extension.id);
                        matches!(status, ExtensionStatus::Installed(_))
                    }
                    ExtensionFilter::NotInstalled => {
                        let status = extension_store.extension_status(&extension.id);

                        matches!(status, ExtensionStatus::NotInstalled)
                    }
                })
                .map(|(ix, _)| ix),
        );
        cx.notify();
    }

    fn fetch_extensions(&mut self, search: Option<String>, cx: &mut ViewContext<Self>) {
        self.is_fetching_extensions = true;
        cx.notify();

        let extension_store = ExtensionStore::global(cx);

        let dev_extensions = extension_store.update(cx, |store, _| {
            store.dev_extensions().cloned().collect::<Vec<_>>()
        });

        let remote_extensions = extension_store.update(cx, |store, cx| {
            store.fetch_extensions(search.as_deref(), cx)
        });

        cx.spawn(move |this, mut cx| async move {
            let dev_extensions = if let Some(search) = search {
                let match_candidates = dev_extensions
                    .iter()
                    .enumerate()
                    .map(|(ix, manifest)| StringMatchCandidate {
                        id: ix,
                        string: manifest.name.clone(),
                        char_bag: manifest.name.as_str().into(),
                    })
                    .collect::<Vec<_>>();

                let matches = match_strings(
                    &match_candidates,
                    &search,
                    false,
                    match_candidates.len(),
                    &Default::default(),
                    cx.background_executor().clone(),
                )
                .await;
                matches
                    .into_iter()
                    .map(|mat| dev_extensions[mat.candidate_id].clone())
                    .collect()
            } else {
                dev_extensions
            };

            let fetch_result = remote_extensions.await;
            this.update(&mut cx, |this, cx| {
                cx.notify();
                this.dev_extension_entries = dev_extensions;
                this.is_fetching_extensions = false;
                this.remote_extension_entries = fetch_result?;
                this.filter_extension_entries(cx);
                anyhow::Ok(())
            })?
        })
        .detach_and_log_err(cx);
    }

    fn render_extensions(
        &mut self,
        range: Range<usize>,
        cx: &mut ViewContext<Self>,
    ) -> Vec<ExtensionCard> {
        let dev_extension_entries_len = if self.filter.include_dev_extensions() {
            self.dev_extension_entries.len()
        } else {
            0
        };
        range
            .map(|ix| {
                if ix < dev_extension_entries_len {
                    let extension = &self.dev_extension_entries[ix];
                    self.render_dev_extension(extension, cx)
                } else {
                    let extension_ix =
                        self.filtered_remote_extension_indices[ix - dev_extension_entries_len];
                    let extension = &self.remote_extension_entries[extension_ix];
                    self.render_remote_extension(extension, cx)
                }
            })
            .collect()
    }

    fn render_dev_extension(
        &self,
        extension: &ExtensionManifest,
        cx: &mut ViewContext<Self>,
    ) -> ExtensionCard {
        let status = ExtensionStore::global(cx)
            .read(cx)
            .extension_status(&extension.id);

        let repository_url = extension.repository.clone();

        ExtensionCard::new()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_end()
                            .child(Headline::new(extension.name.clone()).size(HeadlineSize::Medium))
                            .child(
                                Headline::new(format!("v{}", extension.version))
                                    .size(HeadlineSize::XSmall),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_between()
                            .child(
                                Button::new(
                                    SharedString::from(format!("rebuild-{}", extension.id)),
                                    "Rebuild",
                                )
                                .on_click({
                                    let extension_id = extension.id.clone();
                                    move |_, cx| {
                                        ExtensionStore::global(cx).update(cx, |store, cx| {
                                            store.rebuild_dev_extension(extension_id.clone(), cx)
                                        });
                                    }
                                })
                                .color(Color::Accent)
                                .disabled(matches!(status, ExtensionStatus::Upgrading)),
                            )
                            .child(
                                Button::new(SharedString::from(extension.id.clone()), "Uninstall")
                                    .on_click({
                                        let extension_id = extension.id.clone();
                                        move |_, cx| {
                                            ExtensionStore::global(cx).update(cx, |store, cx| {
                                                store.uninstall_extension(extension_id.clone(), cx)
                                            });
                                        }
                                    })
                                    .color(Color::Accent)
                                    .disabled(matches!(status, ExtensionStatus::Removing)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Label::new(format!(
                            "{}: {}",
                            if extension.authors.len() > 1 {
                                "Authors"
                            } else {
                                "Author"
                            },
                            extension.authors.join(", ")
                        ))
                        .size(LabelSize::Small),
                    )
                    .child(Label::new("<>").size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .justify_between()
                    .children(extension.description.as_ref().map(|description| {
                        Label::new(description.clone())
                            .size(LabelSize::Small)
                            .color(Color::Default)
                    }))
                    .children(repository_url.map(|repository_url| {
                        IconButton::new(
                            SharedString::from(format!("repository-{}", extension.id)),
                            IconName::Github,
                        )
                        .icon_color(Color::Accent)
                        .icon_size(IconSize::Small)
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener({
                            let repository_url = repository_url.clone();
                            move |_, _, cx| {
                                cx.open_url(&repository_url);
                            }
                        }))
                        .tooltip(move |cx| Tooltip::text(repository_url.clone(), cx))
                    })),
            )
    }

    fn render_remote_extension(
        &self,
        extension: &ExtensionApiResponse,
        cx: &mut ViewContext<Self>,
    ) -> ExtensionCard {
        let status = ExtensionStore::global(cx)
            .read(cx)
            .extension_status(&extension.id);

        let (install_or_uninstall_button, upgrade_button) =
            self.buttons_for_entry(extension, &status, cx);
        let repository_url = extension.repository.clone();

        ExtensionCard::new()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_end()
                            .child(Headline::new(extension.name.clone()).size(HeadlineSize::Medium))
                            .child(
                                Headline::new(format!("v{}", extension.version))
                                    .size(HeadlineSize::XSmall),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .justify_between()
                            .children(upgrade_button)
                            .child(install_or_uninstall_button),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Label::new(format!(
                            "{}: {}",
                            if extension.authors.len() > 1 {
                                "Authors"
                            } else {
                                "Author"
                            },
                            extension.authors.join(", ")
                        ))
                        .size(LabelSize::Small),
                    )
                    .child(
                        Label::new(format!("Downloads: {}", extension.download_count))
                            .size(LabelSize::Small),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .children(extension.description.as_ref().map(|description| {
                        h_flex().overflow_x_hidden().child(
                            Label::new(description.clone())
                                .size(LabelSize::Small)
                                .color(Color::Default),
                        )
                    }))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("repository-{}", extension.id)),
                            IconName::Github,
                        )
                        .icon_color(Color::Accent)
                        .icon_size(IconSize::Small)
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener({
                            let repository_url = repository_url.clone();
                            move |_, _, cx| {
                                cx.open_url(&repository_url);
                            }
                        }))
                        .tooltip(move |cx| Tooltip::text(repository_url.clone(), cx)),
                    ),
            )
    }

    fn buttons_for_entry(
        &self,
        extension: &ExtensionApiResponse,
        status: &ExtensionStatus,
        cx: &mut ViewContext<Self>,
    ) -> (Button, Option<Button>) {
        match status.clone() {
            ExtensionStatus::NotInstalled => (
                Button::new(SharedString::from(extension.id.clone()), "Install").on_click(
                    cx.listener({
                        let extension_id = extension.id.clone();
                        let version = extension.version.clone();
                        move |this, _, cx| {
                            this.telemetry
                                .report_app_event("extensions: install extension".to_string());
                            ExtensionStore::global(cx).update(cx, |store, cx| {
                                store.install_extension(extension_id.clone(), version.clone(), cx)
                            });
                        }
                    }),
                ),
                None,
            ),
            ExtensionStatus::Installing => (
                Button::new(SharedString::from(extension.id.clone()), "Install").disabled(true),
                None,
            ),
            ExtensionStatus::Upgrading => (
                Button::new(SharedString::from(extension.id.clone()), "Uninstall").disabled(true),
                Some(
                    Button::new(SharedString::from(extension.id.clone()), "Upgrade").disabled(true),
                ),
            ),
            ExtensionStatus::Installed(installed_version) => (
                Button::new(SharedString::from(extension.id.clone()), "Uninstall").on_click(
                    cx.listener({
                        let extension_id = extension.id.clone();
                        move |this, _, cx| {
                            this.telemetry
                                .report_app_event("extensions: uninstall extension".to_string());
                            ExtensionStore::global(cx).update(cx, |store, cx| {
                                store.uninstall_extension(extension_id.clone(), cx)
                            });
                        }
                    }),
                ),
                if installed_version == extension.version {
                    None
                } else {
                    Some(
                        Button::new(SharedString::from(extension.id.clone()), "Upgrade").on_click(
                            cx.listener({
                                let extension_id = extension.id.clone();
                                let version = extension.version.clone();
                                move |this, _, cx| {
                                    this.telemetry.report_app_event(
                                        "extensions: install extension".to_string(),
                                    );
                                    ExtensionStore::global(cx).update(cx, |store, cx| {
                                        store.upgrade_extension(
                                            extension_id.clone(),
                                            version.clone(),
                                            cx,
                                        )
                                    });
                                }
                            }),
                        ),
                    )
                },
            ),
            ExtensionStatus::Removing => (
                Button::new(SharedString::from(extension.id.clone()), "Uninstall").disabled(true),
                None,
            ),
        }
    }

    fn render_search(&self, cx: &mut ViewContext<Self>) -> Div {
        let mut key_context = KeyContext::default();
        key_context.add("BufferSearchBar");

        let editor_border = if self.query_contains_error {
            Color::Error.color(cx)
        } else {
            cx.theme().colors().border
        };

        h_flex()
            .w_full()
            .gap_2()
            .key_context(key_context)
            // .capture_action(cx.listener(Self::tab))
            // .on_action(cx.listener(Self::dismiss))
            .child(
                h_flex()
                    .flex_1()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .border_1()
                    .border_color(editor_border)
                    .min_w(rems(384. / 16.))
                    .rounded_lg()
                    .child(Icon::new(IconName::MagnifyingGlass))
                    .child(self.render_text_input(&self.query_editor, cx)),
            )
    }

    fn render_text_input(&self, editor: &View<Editor>, cx: &ViewContext<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: if editor.read(cx).read_only(cx) {
                cx.theme().colors().text_disabled
            } else {
                cx.theme().colors().text
            },
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features,
            font_size: rems(0.875).into(),
            font_weight: FontWeight::NORMAL,
            font_style: FontStyle::Normal,
            line_height: relative(1.3),
            background_color: None,
            underline: None,
            strikethrough: None,
            white_space: WhiteSpace::Normal,
        };

        EditorElement::new(
            &editor,
            EditorStyle {
                background: cx.theme().colors().editor_background,
                local_player: cx.theme().players().local(),
                text: text_style,
                ..Default::default()
            },
        )
    }

    fn on_query_change(
        &mut self,
        _: View<Editor>,
        event: &editor::EditorEvent,
        cx: &mut ViewContext<Self>,
    ) {
        if let editor::EditorEvent::Edited = event {
            self.query_contains_error = false;
            self.fetch_extensions_debounced(cx);
        }
    }

    fn fetch_extensions_debounced(&mut self, cx: &mut ViewContext<'_, ExtensionsPage>) {
        self.extension_fetch_task = Some(cx.spawn(|this, mut cx| async move {
            let search = this
                .update(&mut cx, |this, cx| this.search_query(cx))
                .ok()
                .flatten();

            // Only debounce the fetching of extensions if we have a search
            // query.
            //
            // If the search was just cleared then we can just reload the list
            // of extensions without a debounce, which allows us to avoid seeing
            // an intermittent flash of a "no extensions" state.
            if let Some(_) = search {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
            };

            this.update(&mut cx, |this, cx| {
                this.fetch_extensions(search, cx);
            })
            .ok();
        }));
    }

    pub fn search_query(&self, cx: &WindowContext) -> Option<String> {
        let search = self.query_editor.read(cx).text(cx);
        if search.trim().is_empty() {
            None
        } else {
            Some(search)
        }
    }

    fn render_empty_state(&self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        let has_search = self.search_query(cx).is_some();

        let message = if self.is_fetching_extensions {
            "Loading extensions..."
        } else {
            match self.filter {
                ExtensionFilter::All => {
                    if has_search {
                        "No extensions that match your search."
                    } else {
                        "No extensions."
                    }
                }
                ExtensionFilter::Installed => {
                    if has_search {
                        "No installed extensions that match your search."
                    } else {
                        "No installed extensions."
                    }
                }
                ExtensionFilter::NotInstalled => {
                    if has_search {
                        "No not installed extensions that match your search."
                    } else {
                        "No not installed extensions."
                    }
                }
            }
        };

        Label::new(message)
    }
}

impl Render for ExtensionsPage {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .gap_4()
                    .p_4()
                    .border_b()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .justify_between()
                            .child(Headline::new("Extensions").size(HeadlineSize::XLarge))
                            .child(
                                Button::new("add-dev-extension", "Add Dev Extension")
                                    .style(ButtonStyle::Filled)
                                    .size(ButtonSize::Large)
                                    .on_click(|_event, cx| {
                                        cx.dispatch_action(Box::new(InstallDevExtension))
                                    }),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .justify_between()
                            .child(h_flex().child(self.render_search(cx)))
                            .child(
                                h_flex()
                                    .child(
                                        ToggleButton::new("filter-all", "All")
                                            .style(ButtonStyle::Filled)
                                            .size(ButtonSize::Large)
                                            .selected(self.filter == ExtensionFilter::All)
                                            .on_click(cx.listener(|this, _event, cx| {
                                                this.filter = ExtensionFilter::All;
                                                this.filter_extension_entries(cx);
                                            }))
                                            .tooltip(move |cx| {
                                                Tooltip::text("Show all extensions", cx)
                                            })
                                            .first(),
                                    )
                                    .child(
                                        ToggleButton::new("filter-installed", "Installed")
                                            .style(ButtonStyle::Filled)
                                            .size(ButtonSize::Large)
                                            .selected(self.filter == ExtensionFilter::Installed)
                                            .on_click(cx.listener(|this, _event, cx| {
                                                this.filter = ExtensionFilter::Installed;
                                                this.filter_extension_entries(cx);
                                            }))
                                            .tooltip(move |cx| {
                                                Tooltip::text("Show installed extensions", cx)
                                            })
                                            .middle(),
                                    )
                                    .child(
                                        ToggleButton::new("filter-not-installed", "Not Installed")
                                            .style(ButtonStyle::Filled)
                                            .size(ButtonSize::Large)
                                            .selected(self.filter == ExtensionFilter::NotInstalled)
                                            .on_click(cx.listener(|this, _event, cx| {
                                                this.filter = ExtensionFilter::NotInstalled;
                                                this.filter_extension_entries(cx);
                                            }))
                                            .tooltip(move |cx| {
                                                Tooltip::text("Show not installed extensions", cx)
                                            })
                                            .last(),
                                    ),
                            ),
                    ),
            )
            .child(v_flex().px_4().size_full().overflow_y_hidden().map(|this| {
                let mut count = self.filtered_remote_extension_indices.len();
                if self.filter.include_dev_extensions() {
                    count += self.dev_extension_entries.len();
                }

                if count == 0 {
                    return this.py_4().child(self.render_empty_state(cx));
                }

                let view = cx.view().clone();
                let scroll_handle = self.list.clone();
                this.child(
                    canvas(
                        move |bounds, cx| {
                            let mut list = uniform_list::<_, ExtensionCard, _>(
                                view,
                                "entries",
                                count,
                                Self::render_extensions,
                            )
                            .size_full()
                            .pb_4()
                            .track_scroll(scroll_handle)
                            .into_any_element();
                            list.layout(bounds.origin, bounds.size.into(), cx);
                            list
                        },
                        |_bounds, mut list, cx| list.paint(cx),
                    )
                    .size_full(),
                )
            }))
    }
}

impl EventEmitter<ItemEvent> for ExtensionsPage {}

impl FocusableView for ExtensionsPage {
    fn focus_handle(&self, cx: &AppContext) -> gpui::FocusHandle {
        self.query_editor.read(cx).focus_handle(cx)
    }
}

impl Item for ExtensionsPage {
    type Event = ItemEvent;

    fn tab_content(&self, _: Option<usize>, selected: bool, _: &WindowContext) -> AnyElement {
        Label::new("Extensions")
            .color(if selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("extensions page")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn clone_on_split(
        &self,
        _workspace_id: WorkspaceId,
        _: &mut ViewContext<Self>,
    ) -> Option<View<Self>> {
        None
    }

    fn to_item_events(event: &Self::Event, mut f: impl FnMut(workspace::item::ItemEvent)) {
        f(*event)
    }
}
