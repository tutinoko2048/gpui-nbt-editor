#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod add_tag;
mod nbt;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, Sizable, StyledExt, Theme, ThemeMode, WindowExt, h_flex, v_flex,
    button::{Button, ButtonVariants},
    dialog::{DialogClose, DialogFooter},
    empty::{Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle},
    input::{Input, InputEvent, InputState},
    list::ListItem,
    notification::Notification,
    tag::Tag as Badge,
    tree::{TreeItem, TreeState, tree},
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use add_tag::{AddTagDialog, AddTarget};
use gpui_fps::fps_monitor;
use nbt::Seg;
use nbtx::Value;

actions!(nbt_editor, [Save, Open]);

/// Display data for one tree row, keyed by the tree item id.
struct Node {
    name: SharedString,
    kind: &'static str,
    preview: SharedString,
    full: SharedString,
    path: SharedString,
    parent: Option<SharedString>,
    segs: Vec<Seg>,
    is_container: bool,
}

struct Document {
    path: PathBuf,
    size: u64,
    file: nbt::NbtFile,
    tag_count: usize,
    dirty: bool,
    /// The tree's items; clones share expansion state with the tree.
    items: Vec<TreeItem>,
    nodes: Rc<HashMap<SharedString, Node>>,
}

struct NbtEditor {
    focus_handle: FocusHandle,
    tree: Entity<TreeState>,
    doc: Option<Document>,
    error: Option<SharedString>,
    loading: bool,
    saving: bool,
    selected_id: Option<SharedString>,
    value_input: Entity<InputState>,
    edit_error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl NbtEditor {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx));
        let value_input = cx.new(|cx| InputState::new(window, cx));

        let subscriptions = vec![
            cx.observe_in(&tree, window, |this, tree, window, cx| {
                let id = tree.read(cx).selected_item().map(|item| item.id.clone());
                if id != this.selected_id {
                    this.selected_id = id;
                    this.sync_inputs(window, cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&value_input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.apply_edit(window, cx);
                }
            }),
        ];

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);

        Self {
            focus_handle,
            tree,
            doc: None,
            error: None,
            loading: false,
            saving: false,
            selected_id: None,
            value_input,
            edit_error: None,
            _subscriptions: subscriptions,
        }
    }

    fn open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open NBT file".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update_in(cx, |this, window, cx| this.load(path, window, cx)).ok();
            }
        })
        .detach();
    }

    fn load(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        cx.notify();

        let task = cx.background_spawn({
            let path = path.clone();
            async move {
                let bytes = std::fs::read(&path)?;
                let file = nbt::parse(&bytes)?;
                std::io::Result::Ok((bytes.len() as u64, file))
            }
        });

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Ok((size, file)) => {
                        this.doc = Some(Document {
                            path,
                            size,
                            file,
                            tag_count: 0,
                            dirty: false,
                            items: Vec::new(),
                            nodes: Rc::default(),
                        });
                        this.rebuild(None, None, window, cx);
                    }
                    Err(err) => {
                        this.error = Some(format!("{}: {err}", path.display()).into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.doc = None;
        self.error = None;
        self.selected_id = None;
        self.tree.update(cx, |tree, cx| tree.set_items(Vec::<TreeItem>::new(), cx));
        cx.notify();
    }

    /// Rebuilds the tree from the document, keeping expanded nodes expanded
    /// (plus `expand`), then selects `select` or the root.
    fn rebuild(
        &mut self,
        select: Option<SharedString>,
        expand: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(doc) = self.doc.as_mut() else { return };
        let root_label = match doc.file.root_name.as_str() {
            "" => "root",
            name => name,
        };

        let mut expanded = HashSet::new();
        collect_expanded(&doc.items, &mut expanded);
        if doc.items.is_empty() {
            expanded.insert(SharedString::from(root_label.to_string()));
        }
        expanded.extend(expand);

        let mut nodes = HashMap::new();
        let root = build_item(root_label, &doc.file.root, "", Vec::new(), &expanded, &mut nodes);
        doc.items = vec![root.clone()];
        doc.nodes = Rc::new(nodes);
        doc.tag_count = nbt::count(&doc.file.root);

        let selected = self.tree.update(cx, |tree, cx| {
            tree.set_items(vec![root], cx);
            let ix = select.as_ref().and_then(|id| tree.index_of(id)).unwrap_or(0);
            tree.set_selected_index(Some(ix), cx);
            tree.scroll_to_item(ix, ScrollStrategy::Nearest);
            tree.selected_item().map(|item| item.id.clone())
        });
        self.selected_id = selected;
        self.sync_inputs(window, cx);
        cx.notify();
    }

    fn selected_node(&self) -> Option<&Node> {
        let doc = self.doc.as_ref()?;
        doc.nodes.get(self.selected_id.as_ref()?)
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus_handle, cx);
    }

    /// Loads the selected tag into the value editor.
    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_error = None;
        let text = self
            .selected_node()
            .zip(self.doc.as_ref())
            .and_then(|(node, doc)| nbt::get(&doc.file.root, &node.segs))
            .and_then(nbt::edit_text)
            .unwrap_or_default();
        self.value_input.update(cx, |s, cx| s.set_value(text, window, cx));
    }

    fn apply_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.value_input.read(cx).value();
        let Some(node) = self.selected_node() else { return };
        let (id, segs, kind) = (self.selected_id.clone(), node.segs.clone(), node.kind);
        let Some(doc) = self.doc.as_mut() else { return };
        let Some(slot) = nbt::get_mut(&mut doc.file.root, &segs) else { return };
        if nbt::edit_text(slot).is_none() {
            return;
        }
        match nbt::parse_value(kind, &text) {
            Ok(value) => {
                *slot = value;
                doc.dirty = true;
                self.rebuild(id, None, window, cx);
            }
            Err(err) => {
                self.edit_error = Some(err.into());
                cx.notify();
            }
        }
    }

    /// Where the + button adds: the selected container, or the selected tag's parent.
    fn add_target(&self) -> Option<AddTarget> {
        let doc = self.doc.as_ref()?;
        let mut node = self.selected_node()?;
        if !node.is_container {
            node = doc.nodes.get(node.parent.as_ref()?)?;
        }
        let (is_compound, fixed_kind) = match nbt::get(&doc.file.root, &node.segs)? {
            Value::Compound(_) => (true, None),
            Value::List(items) => (false, items.first().map(nbt::type_name)),
            _ => return None,
        };
        Some(AddTarget { parent_id: node.path.clone(), is_compound, fixed_kind })
    }

    fn open_add_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.add_target() else { return };
        let title = if target.is_compound { "Add tag" } else { "Add element" };
        let editor = cx.entity().downgrade();
        let form = cx.new(|cx| AddTagDialog::new(editor.clone(), target, window, cx));

        window.open_dialog(cx, move |dialog, _, _| {
            let submit = form.clone();
            let editor = editor.clone();
            dialog
                .title(title)
                .w(px(420.))
                .child(form.clone())
                .footer(
                    DialogFooter::new()
                        .gap_2()
                        .child(DialogClose::new().child(Button::new("cancel").outline().label("Cancel")))
                        .child(
                            Button::new("add")
                                .primary()
                                .icon(IconName::Plus)
                                .label("Add")
                                .on_click(move |_, window, cx| {
                                    submit.update(cx, |form, cx| form.submit(window, cx));
                                }),
                        ),
                )
                .on_close(move |_, window, cx| {
                    editor.update(cx, |editor, cx| editor.focus(window, cx)).ok();
                })
        });
    }

    /// Adds a child to the container `parent_id` and selects it.
    fn insert_tag(
        &mut self,
        parent_id: &SharedString,
        name: String,
        kind: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let doc = self.doc.as_mut().ok_or("No file is open")?;
        let segs = doc.nodes.get(parent_id).ok_or("Parent tag no longer exists")?.segs.clone();
        let target = nbt::get_mut(&mut doc.file.root, &segs).ok_or("Parent tag no longer exists")?;
        let value = nbt::parse_value(kind, text)?;

        let label = match target {
            Value::Compound(fields) => {
                if name.is_empty() {
                    return Err("Name is required".into());
                }
                if fields.contains_key(&name) {
                    return Err(format!("\"{name}\" already exists"));
                }
                fields.insert(name.clone(), value);
                name
            }
            Value::List(items) => {
                items.push(value);
                format!("[{}]", items.len() - 1)
            }
            _ => return Err("Tags can only be added to a Compound or List".into()),
        };

        doc.dirty = true;
        let new_id = SharedString::from(child_path(parent_id, &label));
        self.rebuild(Some(new_id), Some(parent_id.clone()), window, cx);
        Ok(())
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(doc) = self.doc.as_ref() else { return };
        if self.saving || !doc.dirty {
            return;
        }
        let bytes = match nbt::encode(&doc.file) {
            Ok(bytes) => bytes,
            Err(err) => {
                window.push_notification(Notification::error(format!("Save failed: {err}")), cx);
                return;
            }
        };
        let path = doc.path.clone();
        let len = bytes.len() as u64;
        let task = cx.background_spawn(async move { std::fs::write(path, bytes) });
        self.saving = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(()) => {
                        if let Some(doc) = this.doc.as_mut() {
                            doc.dirty = false;
                            doc.size = len;
                        }
                        window.push_notification(Notification::success("Saved"), cx);
                    }
                    Err(err) => {
                        window.push_notification(
                            Notification::error(format!("Save failed: {err}")),
                            cx,
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_theme(window: &mut Window, cx: &mut App) {
        let mode = if cx.theme().is_dark() { ThemeMode::Light } else { ThemeMode::Dark };
        Theme::change(mode, Some(window), cx);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .h_12()
            .px_4()
            .gap_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .size_7()
                            .rounded_md()
                            .bg(theme.primary)
                            .text_color(theme.primary_foreground)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::Braces).small()),
                    )
                    .child(div().font_semibold().child("NBT Editor")),
            )
            .when_some(self.doc.as_ref(), |this, doc| {
                let name = doc
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                this.child(div().h_5().w_px().bg(theme.border))
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(
                                Icon::new(IconName::FileBraces)
                                    .small()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(div().text_sm().font_medium().truncate().child(name)),
                    )
                    .when(doc.dirty, |this| this.child(Badge::warning().small().child("Modified")))
                    .child(Badge::secondary().small().child(doc.file.format.label()))
                    .child(Badge::secondary().small().child(doc.file.compression.label()))
                    .child(Badge::secondary().small().child(format_size(doc.size)))
                    .child(Badge::secondary().small().child(format!("{} tags", doc.tag_count)))
            })
            .child(div().flex_1())
            .child(
                Button::new("theme")
                    .ghost()
                    .small()
                    .icon(if theme.is_dark() { IconName::Sun } else { IconName::Moon })
                    .on_click(|_, window, cx| Self::toggle_theme(window, cx)),
            )
            .when_some(self.doc.as_ref(), |this, doc| {
                this.child(
                    Button::new("close")
                        .ghost()
                        .small()
                        .icon(IconName::X)
                        .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                )
                .child(
                    Button::new("save")
                        .outline()
                        .small()
                        .icon(IconName::Save)
                        .label("Save")
                        .loading(self.saving)
                        .disabled(!doc.dirty)
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                )
            })
            .child(
                Button::new("open")
                    .primary()
                    .small()
                    .icon(IconName::FolderOpen)
                    .label("Open")
                    .loading(self.loading)
                    .on_click(cx.listener(|this, _, window, cx| this.open_dialog(window, cx))),
            )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div().size_full().p_8().flex().child(
            Empty::new()
                .border_2()
                .header(
                    EmptyHeader::new()
                        .media(
                            EmptyMedia::new()
                                .with_variant(EmptyMediaVariant::Icon)
                                .child(Icon::new(IconName::FileBraces)),
                        )
                        .title(EmptyTitle::new().child("Open an NBT file"))
                        .description(EmptyDescription::new().child(
                            "Drop a .dat / .nbt / .schem / .litematic file here, or pick one from disk. GZip, Zlib and raw files are supported.",
                        )),
                )
                .child(
                    Button::new("open-empty")
                        .primary()
                        .icon(IconName::FolderOpen)
                        .label("Choose file…")
                        .loading(self.loading)
                        .on_click(cx.listener(|this, _, window, cx| this.open_dialog(window, cx))),
                )
                .when_some(self.error.clone(), |this, err| {
                    this.child(
                        div()
                            .max_w(px(560.))
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(theme.danger.opacity(0.1))
                            .text_color(theme.danger)
                            .text_sm()
                            .child(err),
                    )
                }),
        )
    }

    fn render_document(&self, doc: &Document, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let nodes = doc.nodes.clone();
        let tree_view = tree(&self.tree, move |ix, entry, _selected, _window, cx| {
            let item = entry.item();
            let node = nodes.get(&item.id);
            let theme = cx.theme();
            let chevron = if !entry.is_folder() {
                div().size_4().into_any_element()
            } else {
                Icon::new(if entry.is_expanded() {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall()
                .text_color(theme.muted_foreground)
                .into_any_element()
            };
            ListItem::new(ix).pl(px(8. + entry.depth() as f32 * 16.)).child(
                h_flex()
                    .gap_2()
                    .w_full()
                    .min_w_0()
                    .child(chevron)
                    .when_some(node, |this, node| this.child(type_badge(node.kind)))
                    .child(div().text_sm().flex_shrink_0().child(item.label.clone()))
                    .when_some(node, |this, node| {
                        this.child(
                            div()
                                .text_sm()
                                .min_w_0()
                                .truncate()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(if node.is_container {
                                    theme.muted_foreground
                                } else {
                                    type_color(node.kind)
                                })
                                .child(node.preview.clone()),
                        )
                    }),
            )
        });

        let detail = match self.selected_node() {
            Some(node) => self.render_detail(node, doc, cx).into_any_element(),
            None => div()
                .p_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Select a tag to inspect it.")
                .into_any_element(),
        };

        let can_add = self.add_target().is_some();
        let toolbar = h_flex()
            .h_10()
            .px_2()
            .gap_1()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("add-tag")
                    .ghost()
                    .small()
                    .icon(IconName::Plus)
                    .tooltip("Add tag")
                    .disabled(!can_add)
                    .on_click(cx.listener(|this, _, window, cx| this.open_add_dialog(window, cx))),
            );

        let theme = cx.theme();
        h_flex()
            .size_full()
            .min_h_0()
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(toolbar)
                    .child(div().flex_1().min_h_0().p_2().child(tree_view)),
            )
            .child(
                v_flex()
                    .w(px(360.))
                    .h_full()
                    .flex_shrink_0()
                    .border_l_1()
                    .border_color(theme.border)
                    .bg(theme.sidebar)
                    .child(detail),
            )
    }

    fn render_detail(&self, node: &Node, doc: &Document, cx: &mut Context<Self>) -> impl IntoElement {
        let value = nbt::get(&doc.file.root, &node.segs);
        let editable = value.and_then(nbt::edit_text).is_some();

        let apply = cx.listener(|this, _, window, cx| this.apply_edit(window, cx));

        let theme = cx.theme();
        let label = |text: &'static str| {
            div().text_xs().font_medium().text_color(theme.muted_foreground).child(text)
        };
        let mono = |text: SharedString| {
            div().text_sm().font_family(theme.mono_font_family.clone()).child(text)
        };
        let error_text = |err: Option<SharedString>| {
            err.map(|err| div().text_xs().text_color(theme.danger).child(err))
        };

        let value_section = if editable {
            v_flex()
                .gap_1()
                .child(label("Value"))
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.value_input).small()))
                        .child(Button::new("apply").primary().small().label("Apply").on_click(apply)),
                )
                .children(error_text(self.edit_error.clone()))
        } else {
            v_flex().gap_1().child(label("Value")).child(
                div()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .text_sm()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(if node.is_container {
                        theme.muted_foreground
                    } else {
                        type_color(node.kind)
                    })
                    .child(node.full.clone()),
            )
        };

        v_flex()
            .id("detail")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_4()
            .child(
                h_flex()
                    .gap_2()
                    .child(type_badge(node.kind))
                    .child(div().font_semibold().truncate().child(node.name.clone())),
            )
            .child(v_flex().gap_1().child(label("Type")).child(mono(node.kind.into())))
            .child(v_flex().gap_1().child(label("Path")).child(mono(node.path.clone())))
            .child(value_section)
    }
}

impl Render for NbtEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (bg, fg) = (cx.theme().background, cx.theme().foreground);
        let body = match &self.doc {
            Some(doc) => self.render_document(doc, cx).into_any_element(),
            None => self.render_empty(cx).into_any_element(),
        };

        v_flex()
            .id("root")
            .key_context("NbtEditor")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &Open, window, cx| this.open_dialog(window, cx)))
            .relative()
            .size_full()
            .bg(bg)
            .text_color(fg)
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(body))
            .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().accent))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                if let Some(path) = paths.paths().first() {
                    this.load(path.clone(), window, cx);
                }
            }))
            .child(fps_monitor(window, cx).anchor(Anchor::BottomRight))
    }
}

/// Tree item id for a child: `a.b`, `a[0]`.
fn child_path(parent_path: &str, label: &str) -> String {
    if parent_path.is_empty() {
        label.to_string()
    } else if label.starts_with('[') {
        format!("{parent_path}{label}")
    } else {
        format!("{parent_path}.{label}")
    }
}

fn collect_expanded(items: &[TreeItem], out: &mut HashSet<SharedString>) {
    for item in items {
        if item.is_expanded() {
            out.insert(item.id.clone());
        }
        collect_expanded(&item.children, out);
    }
}

fn build_item(
    label: &str,
    tag: &Value,
    parent_path: &str,
    segs: Vec<Seg>,
    expanded: &HashSet<SharedString>,
    nodes: &mut HashMap<SharedString, Node>,
) -> TreeItem {
    let path = child_path(parent_path, label);
    let id: SharedString = path.clone().into();
    let child_segs = |seg: Seg| {
        let mut segs = segs.clone();
        segs.push(seg);
        segs
    };

    let children: Vec<TreeItem> = match tag {
        Value::Compound(fields) => {
            let mut fields: Vec<_> = fields.iter().collect();
            fields.sort_by(|a, b| a.0.cmp(b.0));
            fields
                .into_iter()
                .map(|(name, child)| {
                    let segs = child_segs(Seg::Key(name.clone()));
                    build_item(name, child, &path, segs, expanded, nodes)
                })
                .collect()
        }
        Value::List(items) => items
            .iter()
            .enumerate()
            .map(|(i, child)| {
                let segs = child_segs(Seg::Index(i));
                build_item(&format!("[{i}]"), child, &path, segs, expanded, nodes)
            })
            .collect(),
        _ => Vec::new(),
    };

    nodes.insert(
        id.clone(),
        Node {
            name: label.to_string().into(),
            kind: nbt::type_name(tag),
            preview: nbt::preview(tag).into(),
            full: nbt::full_value(tag).into(),
            path: path.into(),
            parent: (!parent_path.is_empty()).then(|| parent_path.to_string().into()),
            segs,
            is_container: nbt::is_container(tag),
        },
    );
    let is_expanded = expanded.contains(&id);
    TreeItem::new(id, label.to_string()).children(children).expanded(is_expanded)
}

fn type_color(kind: &str) -> Hsla {
    let hue = match kind {
        "Byte" | "Short" | "Int" | "Long" => 210.,
        "Float" | "Double" => 265.,
        "String" => 142.,
        "List" => 32.,
        "Compound" => 45.,
        _ => 190.,
    };
    hsla(hue / 360., 0.65, 0.5, 1.)
}

pub(crate) fn type_badge(kind: &'static str) -> impl IntoElement {
    let short = match kind {
        "Byte" => "B",
        "Short" => "S",
        "Int" => "I",
        "Long" => "L",
        "Float" => "F",
        "Double" => "D",
        "String" => "T",
        "List" => "[ ]",
        "Compound" => "{ }",
        "Byte[]" => "B[]",
        "Int[]" => "I[]",
        "Long[]" => "L[]",
        _ => "?",
    };
    let color = type_color(kind);
    div()
        .flex_shrink_0()
        .min_w(px(26.))
        .h(px(18.))
        .px_1()
        .rounded_sm()
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .font_semibold()
        .bg(color.opacity(0.15))
        .text_color(color)
        .child(short)
}

fn format_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.),
        b => format!("{:.1} MB", b as f64 / (1024. * 1024.)),
    }
}

fn main() {
    application().with_assets(assets::Assets).run(|cx| {
        init(cx);
        // KeyBinding::new can't resolve "secondary-" (it has no OS), so pick it here.
        let mac = cfg!(target_os = "macos");
        cx.bind_keys([
            KeyBinding::new(if mac { "cmd-s" } else { "ctrl-s" }, Save, Some("NbtEditor")),
            KeyBinding::new(if mac { "cmd-o" } else { "ctrl-o" }, Open, Some("NbtEditor")),
        ]);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(720.)), cx)),
            ..Default::default()
        };
        open_window(options, cx, |window, cx| {
            window.set_window_title("NBT Editor");
            cx.new(|cx| NbtEditor::new(window, cx))
        })
        .expect("Failed to open window");
    });
}
