//! The window: the same screens and flow as `pi-famulus top`, drawn with
//! native views. The [`engine`] owns the session; this file only lays out
//! its [`Frame`]s and turns keys, clicks and typing into the TUI's inputs.
//!
//! Screens, as in the TUI: a header (overview and work kinds), the main box
//! (tabs, scope and sort, the table, key hints), an optional preview, and
//! full-window overlays for the help and for a detail.

use std::sync::Arc;

use nsh_core::prelude::*;
use pi_famulus::top::model::TabHit;
use pi_famulus::top::{ui, Action, Input};

use crate::engine::{self, Column, Detail, Engine, Frame, Key, Row, Run, Runs, Scrolled, Viewport};
use crate::keys;

// Type and spacing. Text regions are measured in character cells, like the
// terminal, so the TUI's layout functions can size what they produce.
const FONT: f32 = 12.0;
const CHAR_W: f32 = 7.23;
const LINE_H: f32 = 17.0;
const ROW_H: f32 = 22.0;
const PAD: f32 = 10.0;
/// Scroll views that only report the wheel sit at this offset.
const WHEEL_REST: f32 = 400.0;

// The TUI's palette (`top::ui`), on a terminal-like dark surface.
const BG: u32 = 0x15171d;
const PANEL: u32 = 0x1b1e26;
const BORDER: u32 = 0x2c303c;
const TEXT: u32 = 0xdee1e8;
const MUTED: u32 = 0x969cac;
const DIM: u32 = 0x646a7c;
const ACCENT: u32 = 0x82aaff;
const HOT: u32 = 0xee7070;
const SEL_BG: u32 = 0x262a38;
const WARN: u32 = 0xe6b45a;
const ERR: u32 = 0xf06e78;
const BOX_OVERVIEW: u32 = 0x567c62;
const BOX_KINDS: u32 = 0x807a50;
const BOX_MAIN: u32 = 0x625e9c;
const BOX_PREVIEW: u32 = 0x885a60;

fn c(hex: u32) -> AdaptiveColor {
    AdaptiveColor::rgb(hex, hex)
}

fn mono(t: impl IntoReactive<SharedString>) -> Text {
    text(t).font_family("monospace").text_size(px(FONT)).line_height(px(LINE_H))
}

/// One line of styled runs, never wrapping: one native text with a span
/// per run, so spacing and alignment stay as the TUI laid them out.
fn line(runs: &Runs) -> View {
    let mut t = mono(String::new()).number_of_lines(1u32).text_color(c(TEXT));
    for r in runs {
        let mut span = text(r.text.clone()).text_color(c(r.fg.unwrap_or(TEXT)));
        if r.bold {
            span = span.font_weight(FontWeight::BOLD);
        }
        if r.italic {
            span = span.italic();
        }
        if r.underline {
            span = span.underline();
        }
        if let Some(bg) = r.bg {
            span = span.bg(c(bg));
        }
        t = t.child(span);
    }
    view().h(px(LINE_H)).overflow_hidden().child(t)
}

fn plain(text: &str, color: u32) -> Runs {
    vec![Run { text: text.into(), fg: Some(color), ..Default::default() }]
}

/// A box with its title cut into the top edge, as the TUI draws them.
fn panel(border: u32, title: impl IntoElement, body: impl IntoElement) -> View {
    view()
        .bg(c(PANEL))
        .border(px(1.0))
        .border_color(c(border))
        .rounded(px(6.0))
        .overflow_hidden()
        .child(view().flex_row().items_center().gap_2().px(px(PAD)).h(px(24.0)).child(title))
        .child(body)
}

fn keycap(k: &Key) -> View {
    view()
        .flex_row()
        .flex_shrink_0()
        .child(mono(k.key.clone()).text_color(c(TEXT)).font_weight(FontWeight::BOLD))
        .child(mono(format!(" {}", k.label)).text_color(c(MUTED)))
}

fn key_hints(keys: &[Key]) -> View {
    let mut row = view().flex_row().gap_4().overflow_hidden().flex_1();
    for k in keys {
        row = row.child(keycap(k));
    }
    row
}

#[component]
pub fn App() -> impl IntoElement {
    let engine = engine::start();
    let frame = signal(Arc::new(Frame::default()));
    let viewport = signal(Viewport::default());

    {
        let mut frames = engine.frames.clone();
        spawn(async move {
            while frames.changed().await.is_ok() {
                let f = frames.borrow_and_update().clone();
                if f.quit {
                    nsh_core::element::desktop::close_window();
                    std::process::exit(0);
                }
                frame.set(f);
            }
        });
    }
    keys::bind(&engine);
    {
        let engine = engine.clone();
        effect(move || engine.viewport(viewport.get()));
    }

    let detail_open = computed(move || frame.get().detail.is_some());
    let help_open = computed(move || frame.get().help);
    let preview_open = computed(move || frame.get().preview.is_some());

    view()
        .flex_1()
        .bg(c(BG))
        .text_color(c(TEXT))
        .relative()
        .on_layout(move |b: &Bounds<Points>| {
            let cols = ((b.size.width.0 - 2.0 * PAD) / CHAR_W).floor().max(20.0) as u16;
            viewport.update(|v| {
                v.cols = cols;
                v.overview_cols = ((b.size.width.0 * 0.56 - 3.0 * PAD) / CHAR_W).floor().max(20.0) as usize;
                v.kinds_cols = ((b.size.width.0 * 0.44 - 3.0 * PAD) / CHAR_W).floor().max(20.0) as usize;
                v.preview_cols = cols.saturating_sub(4) as usize;
                v.detail_cols = cols.saturating_sub(4) as usize;
                v.detail_rows = ((b.size.height.0 - 150.0) / LINE_H).floor().max(6.0) as usize;
            });
        })
        .child(header(frame, viewport))
        .child(main_box(&engine, frame, viewport))
        .child(when(preview_open, move || preview(frame, viewport)))
        .child(when(help_open, {
            let engine = engine.clone();
            move || help(&engine)
        }))
        .child(when(detail_open, {
            let engine = engine.clone();
            move || detail(&engine, frame)
        }))
}

// ── header ──────────────────────────────────────────────────────────────────

fn header(frame: Signal<Arc<Frame>>, viewport: Signal<Viewport>) -> View {
    let overview = computed(move || frame.get().overview.clone());
    let kinds = computed(move || frame.get().kinds.clone());
    let pill = computed(move || frame.get().pill.clone());
    let footer = computed(move || {
        let f = frame.get();
        let mut runs = f.pid_up.as_ref().map(|p| plain(p, DIM)).unwrap_or_default();
        if !f.messaging.is_empty() {
            runs.push(Run { text: "   ".into(), ..Default::default() });
            runs.extend(f.messaging.iter().cloned());
        }
        runs
    });
    let title = move || {
        view()
            .flex_row()
            .flex_1()
            .child(mono("pi-famulus".to_string()).font_weight(FontWeight::BOLD).text_color(c(TEXT)))
            .child(mono(move || format!(" {}", frame.get().version)).text_color(c(DIM)))
            .child(view().flex_1())
            .child(dynamic(move || line(&pill.get())))
    };
    view()
        .flex_row()
        .gap(px(PAD))
        .px(px(PAD))
        .pt(px(PAD))
        .child(
            panel(
                BOX_OVERVIEW,
                title(),
                view()
                    .px(px(PAD))
                    .pb(px(6.0))
                    .child(each(move || overview.get(), |r: &Runs| format!("{r:?}"), |r, _| dynamic(move || line(&r.get()))))
                    .child(dynamic(move || line(&footer.get()))),
            )
            .flex_grow(56.0).w(px(0.0)),
        )
        .child(
            panel(
                BOX_KINDS,
                mono("work kinds".to_string()).font_weight(FontWeight::BOLD).text_color(c(TEXT)),
                view()
                    .px(px(PAD))
                    .pb(px(6.0))
                    .flex_1()
                    .on_layout(move |b: &Bounds<Points>| {
                        let rows = (b.size.height.0 / LINE_H).floor().max(2.0) as usize;
                        if viewport.get_untracked().kinds_rows != rows {
                            viewport.update(|v| v.kinds_rows = rows);
                        }
                    })
                    .child(each(move || kinds.get(), |r: &Runs| format!("{r:?}"), |r, _| dynamic(move || line(&r.get())))),
            )
            .flex_grow(44.0).w(px(0.0)),
        )
}

// ── main box ────────────────────────────────────────────────────────────────

fn tab_bar(engine: &Engine, frame: Signal<Arc<Frame>>) -> View {
    let tabs = computed(move || frame.get().tabs.clone());
    let e = engine.clone();
    let mut bar = view().flex_row().items_center().gap_4().px(px(PAD)).h(px(30.0)).border_b(px(1.0)).border_color(c(BORDER));
    bar = bar.child(each(
        move || tabs.get(),
        |t: &engine::TabItem| t.label.clone(),
        move |t, index| {
            let e = e.clone();
            view()
                .flex_row()
                .cursor(Cursor::Pointer)
                .on_press(move || {
                    e.send(Input::Action(Action::TabBar(t.get_untracked().hit)));
                })
                .child(dynamic(move || {
                    let t = t.get();
                    let (name, count) = if t.active { (ACCENT, ACCENT) } else { (MUTED, DIM) };
                    let mut runs = vec![
                        Run { text: ["¹", "²", "³", "⁴"][index.get().min(3)].into(), fg: Some(HOT), ..Default::default() },
                        Run { text: t.label.clone(), fg: Some(name), bold: t.active, underline: t.active, ..Default::default() },
                    ];
                    if let Some(n) = t.count {
                        runs.push(Run { text: format!(" {n}"), fg: Some(count), ..Default::default() });
                    }
                    line(&runs)
                }))
        },
    ));
    let scope_e = engine.clone();
    let sort_e = engine.clone();
    bar.child(view().flex_1())
        .child(
            view()
                .flex_row()
                .cursor(Cursor::Pointer)
                .on_press(move || scope_e.send(Input::Action(Action::TabBar(TabHit::Scope))))
                .child(mono("scope ".to_string()).text_color(c(DIM)))
                .child(mono(move || frame.get().scope.clone()).text_color(c(TEXT))),
        )
        .child(
            view()
                .flex_row()
                .cursor(Cursor::Pointer)
                .on_press(move || sort_e.send(Input::Action(Action::TabBar(TabHit::Sort))))
                .child(mono("sort ".to_string()).text_color(c(DIM)))
                .child(mono(move || frame.get().sort.clone()).text_color(c(TEXT))),
        )
}

/// A scroll view that only reports the wheel, as the terminal's wheel moves
/// the selection rather than the page: its movement becomes `Wheel` ticks
/// (one per `step` points), and it recenters before reaching an edge.
fn wheel_area(engine: &Engine, step: f32, content: impl IntoElement) -> Scroll {
    let r = element_ref();
    let e = engine.clone();
    let acc = signal(0.0f32);
    // The last offset seen; `None` until the first event after a layout.
    let last = signal(None::<f32>);
    scroll()
        .flex_1()
        .scrollbar(false)
        .element_ref(&r)
        .on_layout(move |_: &Bounds<Points>| {
            last.set(None);
            r.scroll_to(Point { x: px(0.0), y: px(WHEEL_REST) }, false);
        })
        .on_scroll(move |ev: &ScrollEvent| {
            let y = ev.offset.y.0;
            let Some(prev) = last.get_untracked() else {
                // Settle at rest first (a jump made before the content was
                // measured is clamped); no ticks until it holds.
                if (y - WHEEL_REST).abs() > 1.0 {
                    r.scroll_to(Point { x: px(0.0), y: px(WHEEL_REST) }, false);
                } else {
                    last.set(Some(y));
                }
                return;
            };
            last.set(Some(y));
            let total = acc.get_untracked() + (y - prev);
            let ticks = (total / step).trunc();
            acc.set(total - ticks * step);
            if ticks != 0.0 {
                e.send(Input::Action(Action::Wheel(ticks as i32)));
            }
            let max = ev.content_size.height.0 - ev.size.height.0;
            if y < WHEEL_REST / 4.0 || y > max - WHEEL_REST / 4.0 {
                last.set(None);
                r.scroll_to(Point { x: px(0.0), y: px(WHEEL_REST) }, false);
            }
        })
        .child(view().h(px(WHEEL_REST)).flex_shrink_0())
        .child(content)
        // Taller than any window: the rest offset is always reachable, however
        // short the content.
        .child(view().h(px(WHEEL_REST + 4000.0)).flex_shrink_0())
}

fn cell_view(col: &Column, runs: &Runs, first: bool, selected: bool) -> View {
    let mut cell = view().flex_row().overflow_hidden();
    cell = match col.width {
        Some(w) => cell.w(px(f32::from(w) * CHAR_W)).flex_shrink_0(),
        None => cell.flex_1().min_w(px(0.0)),
    };
    if first {
        cell = cell.child(mono(if selected { "▌" } else { " " }.to_string()).text_color(c(ACCENT)));
    }
    cell.child(line(runs))
}

fn table(engine: &Engine, frame: Signal<Arc<Frame>>, viewport: Signal<Viewport>) -> View {
    let columns = computed(move || frame.get().columns.clone());
    let rows = computed(move || frame.get().rows.clone());
    let head_e = engine.clone();
    let row_e = engine.clone();
    let header = view()
        .flex_row()
        .gap_2()
        .h(px(ROW_H))
        .items_center()
        .px(px(PAD))
        .cursor(Cursor::Pointer)
        .on_press(move || head_e.send(Input::Action(Action::HeaderClick)))
        .child(dynamic(move || {
            let cols = columns.get();
            cols.iter()
                .enumerate()
                .map(|(i, col)| {
                    let title = if i == 0 { format!(" {}", col.title) } else { col.title.clone() };
                    cell_view(col, &vec![Run { text: title, fg: Some(MUTED), bold: true, ..Default::default() }], false, false)
                })
                .collect::<Vec<_>>()
        }));
    let body = view()
        .child(each(
            move || rows.get(),
            |r: &Row| r.id.clone(),
            move |r: ReadSignal<Row>, _| {
                let e = row_e.clone();
                view()
                    .flex_row()
                    .gap_2()
                    .h(px(ROW_H))
                    .items_center()
                    .px(px(PAD))
                    .bg(move || c(if r.get().selected { SEL_BG } else { PANEL }))
                    .on_press(move |p: &PressEvent| {
                        let id = r.get_untracked().id;
                        let action = if p.clicks >= 2 { Action::RowOpen(id) } else { Action::RowClick(id) };
                        e.send(Input::Action(action));
                    })
                    .child(dynamic(move || {
                        let row = r.get();
                        let cols = columns.get();
                        cols.iter()
                            .zip(row.cells.iter())
                            .enumerate()
                            .map(|(i, (col, runs))| cell_view(col, runs, i == 0, row.selected))
                            .collect::<Vec<_>>()
                    }))
            },
        ));
    view()
        .flex_1()
        .child(header)
        .child(
            view()
                .flex_1()
                .relative()
                .on_layout(move |b: &Bounds<Points>| {
                    let rows = (b.size.height.0 / ROW_H).floor().max(1.0) as usize;
                    if viewport.get_untracked().table_rows != rows {
                        viewport.update(|v| v.table_rows = rows);
                    }
                })
                .child(wheel_area(engine, ROW_H, body).absolute().size_full()),
        )
}

fn footer(engine: &Engine, frame: Signal<Arc<Frame>>) -> View {
    let state = computed(move || {
        let f = frame.get();
        (f.confirm_stop.clone(), f.filter_editing, f.filter.clone(), f.message.clone(), f.keys.clone(), f.position)
    });
    let e = engine.clone();
    view()
        .flex_row()
        .items_center()
        .gap_4()
        .px(px(PAD))
        .h(px(28.0))
        .border_t(px(1.0))
        .border_color(c(BORDER))
        .child(dynamic(move || {
            let (confirm, editing, filter, message, keys, _) = state.get();
            let mut parts: Vec<AnyElement> = Vec::new();
            if let Some(id) = confirm {
                parts.push(mono(format!("■ stop {id}?")).text_color(c(ERR)).font_weight(FontWeight::BOLD).into_element());
            } else if editing {
                let e = e.clone();
                parts.push(
                    view()
                        .flex_row()
                        .items_center()
                        .child(mono("/ ".to_string()).text_color(c(ACCENT)).font_weight(FontWeight::BOLD))
                        .child(
                            nsh_core::element::input()
                                .w(px(260.0))
                                .font_family("monospace")
                                .text_size(px(FONT))
                                .text_color(c(TEXT))
                                .value(filter)
                                .auto_focus(true)
                                .on_submit({
                                    let e = e.clone();
                                    move |_: &str| e.send(keys::press(keys::KeyCode::Enter))
                                })
                                .on_change(move |t: &str| e.send(Input::Action(Action::Text(t.to_string())))),
                        )
                        .into_element(),
                );
            } else {
                if !filter.is_empty() {
                    parts.push(line(&vec![Run { text: "/ ".into(), fg: Some(ACCENT), ..Default::default() }, Run { text: filter, fg: Some(TEXT), ..Default::default() }]).into_element());
                }
                if let Some(m) = message {
                    parts.push(mono(m).text_color(c(WARN)).into_element());
                }
            }
            parts.push(key_hints(&keys).into_element());
            parts
        }))
        .child(mono(move || {
            let (at, total) = state.get().5;
            format!("{at}/{total}")
        }).text_color(c(MUTED)))
}

fn main_box(engine: &Engine, frame: Signal<Arc<Frame>>, viewport: Signal<Viewport>) -> View {
    view()
        .flex_1()
        .m(px(PAD))
        .bg(c(PANEL))
        .border(px(1.0))
        .border_color(c(BOX_MAIN))
        .rounded(px(6.0))
        .overflow_hidden()
        .child(tab_bar(engine, frame))
        .child(table(engine, frame, viewport))
        .child(footer(engine, frame))
}

fn preview(frame: Signal<Arc<Frame>>, viewport: Signal<Viewport>) -> View {
    let parts = computed(move || frame.get().preview.clone().unwrap_or_default());
    panel(
        BOX_PREVIEW,
        dynamic(move || line(&parts.get().0)),
        view()
            .h(px(LINE_H * 9.0))
            .px(px(PAD))
            .pb(px(6.0))
            .overflow_hidden()
            .on_layout(move |b: &Bounds<Points>| {
                let rows = (b.size.height.0 / LINE_H).floor().max(1.0) as usize;
                if viewport.get_untracked().preview_rows != rows {
                    viewport.update(|v| v.preview_rows = rows);
                }
            })
            .child(dynamic(move || parts.get().1.iter().map(line).collect::<Vec<_>>())),
    )
    .mx(px(PAD))
    .mb(px(PAD))
}

// ── overlays ────────────────────────────────────────────────────────────────

fn backdrop(engine: &Engine) -> View {
    let e = engine.clone();
    view()
        .absolute()
        .size_full()
        .bg(AdaptiveColor::rgb(0x000000, 0x000000).opacity(0.45))
        .on_press(move || e.send(Input::Action(Action::Dismiss)))
}

fn help(engine: &Engine) -> View {
    let split = ui::HELP.iter().position(|(k, d)| k.is_empty() && *d == "detail").unwrap_or(ui::HELP.len());
    let column = |rows: &[(&str, &str)]| {
        let mut col = view().flex_1().gap(px(2.0));
        for (k, d) in rows {
            col = col.child(if k.is_empty() {
                line(&vec![Run { text: (*d).into(), fg: Some(MUTED), bold: true, ..Default::default() }])
            } else {
                line(&vec![
                    Run { text: format!("  {k:<19}"), fg: Some(TEXT), bold: true, ..Default::default() },
                    Run { text: (*d).into(), fg: Some(MUTED), ..Default::default() },
                ])
            });
        }
        col
    };
    view()
        .absolute()
        .size_full()
        .items_center()
        .justify_center()
        .child(backdrop(engine))
        .child(
            panel(
                BOX_MAIN,
                mono("◆ keys".to_string()).font_weight(FontWeight::BOLD).text_color(c(TEXT)),
                view()
                    .flex_row()
                    .gap_6()
                    .p(px(PAD * 1.5))
                    .child(column(&ui::HELP[..split]))
                    .child(column(&ui::HELP[split..]))
                    .child(view()),
            )
            .w(px(760.0))
            .child(view().px(px(PAD)).pb(px(8.0)).child(keycap(&Key { key: "esc ? q".into(), label: "close".into() }))),
        )
}

fn scrollbar(engine: &Engine, bar: Memo<Scrolled>) -> View {
    let e = engine.clone();
    let height = signal(1.0f32);
    view()
        .w(px(8.0))
        .relative()
        .on_layout(move |b: &Bounds<Points>| height.set(b.size.height.0.max(1.0)))
        .on_press(move |p: &PressEvent| {
            e.send(Input::Action(Action::ScrollTo(p.position.y.0 / height.get_untracked())));
        })
        .child(dynamic(move || {
            let b = bar.get();
            if b.len <= b.height {
                return None;
            }
            let h = height.get();
            let thumb = (h * b.height as f32 / b.len as f32).max(12.0);
            let max = (b.len - b.height) as f32;
            let top = (h - thumb) * (b.scroll as f32 / max);
            Some(view().absolute().top(px(top)).w(px(6.0)).h(px(thumb)).rounded(px(3.0)).bg(c(DIM)))
        }))
}

fn detail(engine: &Engine, frame: Signal<Arc<Frame>>) -> View {
    let d = computed(move || frame.get().detail.clone());
    let e = engine.clone();
    view()
        .absolute()
        .size_full()
        .p(px(PAD))
        .bg(c(BG))
        .child(dynamic(move || {
            let det = d.get()?;
            Some(match det {
                Detail::Task { .. } => task_detail(&e, d),
                Detail::Chat { .. } => chat_detail(&e, d),
            })
        }))
}

fn detail_frame(title: impl IntoElement, position: impl IntoElement, body: impl IntoElement, bottom: impl IntoElement) -> View {
    view()
        .flex_1()
        .bg(c(PANEL))
        .border(px(1.0))
        .border_color(c(BOX_MAIN))
        .rounded(px(6.0))
        .overflow_hidden()
        .child(
            view()
                .flex_row()
                .items_center()
                .h(px(28.0))
                .px(px(PAD))
                .border_b(px(1.0))
                .border_color(c(BORDER))
                .child(title)
                .child(view().flex_1())
                .child(position),
        )
        .child(body)
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_4()
                .min_h(px(28.0))
                .px(px(PAD))
                .border_t(px(1.0))
                .border_color(c(BORDER))
                .child(bottom),
        )
}

fn title_runs(title: &str) -> Runs {
    vec![
        Run { text: "◆ ".into(), fg: Some(ACCENT), bold: true, ..Default::default() },
        Run { text: title.into(), fg: Some(TEXT), bold: true, ..Default::default() },
    ]
}

fn task_detail(engine: &Engine, d: Memo<Option<Detail>>) -> View {
    let get = move || match d.get() {
        Some(Detail::Task { title, card, tabs, count, rows, bar, editing, query, prompt, position, keys }) => {
            Some((title, card, tabs, count, rows, bar, editing, query, prompt, position, keys))
        }
        _ => None,
    };
    let parts = computed(get);
    let bar = computed(move || parts.get().map(|p| p.5).unwrap_or(Scrolled { len: 0, height: 0, scroll: 0 }));
    let tab_e = engine.clone();
    let input_e = engine.clone();
    let body = view()
        .flex_1()
        .px(px(PAD))
        .child(dynamic(move || parts.get().map(|p| p.1.iter().map(line).collect::<Vec<_>>()).unwrap_or_default()))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_3()
                .h(px(24.0))
                .border_b(px(1.0))
                .border_color(c(BORDER))
                .child(dynamic(move || {
                    let Some(p) = parts.get() else { return Vec::new() };
                    p.2.iter()
                        .enumerate()
                        .map(|(i, (name, active))| {
                            let e = tab_e.clone();
                            view()
                                .flex_row()
                                .cursor(Cursor::Pointer)
                                .on_press(move || e.send(Input::Action(Action::DetailTab(i))))
                                .child(mono(["¹", "²", "³"][i.min(2)].to_string()).text_color(c(HOT)))
                                .child(
                                    mono(name.clone())
                                        .text_color(c(if *active { ACCENT } else { MUTED }))
                                        .font_weight(if *active { FontWeight::BOLD } else { FontWeight::NORMAL }),
                                )
                                .into_element()
                        })
                        .collect::<Vec<_>>()
                }))
                .child(view().flex_1())
                .child(dynamic(move || parts.get().map(|p| line(&p.3)))),
        )
        .child(
            view()
                .flex_1()
                .flex_row()
                .child(
                    view()
                        .flex_1()
                        .relative()
                        .child(wheel_area(engine, LINE_H, view().child(dynamic(move || {
                            parts.get().map(|p| p.4.iter().map(line).collect::<Vec<_>>()).unwrap_or_default()
                        })))
                        .absolute()
                        .size_full()),
                )
                .child(scrollbar(engine, bar)),
        );
    let bottom = dynamic(move || {
        let Some(p) = parts.get() else { return Vec::new() };
        let mut items: Vec<AnyElement> = Vec::new();
        if p.6 {
            let e = input_e.clone();
            let submit = input_e.clone();
            items.push(
                view()
                    .flex_row()
                    .items_center()
                    .child(mono("/ ".to_string()).text_color(c(ACCENT)).font_weight(FontWeight::BOLD))
                    .child(
                        nsh_core::element::input()
                            .w(px(260.0))
                            .font_family("monospace")
                            .text_size(px(FONT))
                            .text_color(c(TEXT))
                            .value(p.7.clone())
                            .auto_focus(true)
                            .on_change(move |t: &str| e.send(Input::Action(Action::Text(t.to_string()))))
                            .on_submit(move |_: &str| submit.send(keys::press(keys::KeyCode::Enter))),
                    )
                    .into_element(),
            );
        } else if let Some(prompt) = &p.8 {
            items.push(line(prompt).into_element());
        }
        items.push(key_hints(&p.10).into_element());
        items
    });
    detail_frame(
        dynamic(move || parts.get().map(|p| line(&title_runs(&p.0)))),
        mono(move || parts.get().and_then(|p| p.9).unwrap_or_default()).text_color(c(MUTED)),
        body,
        bottom,
    )
}

fn chat_detail(engine: &Engine, d: Memo<Option<Detail>>) -> View {
    let parts = computed(move || match d.get() {
        Some(Detail::Chat { title, rows, folds, bar, status, live, position, keys }) => {
            Some((title, rows, folds, bar, status, live, position, keys))
        }
        _ => None,
    });
    let bar = computed(move || parts.get().map(|p| p.3).unwrap_or(Scrolled { len: 0, height: 0, scroll: 0 }));
    let fold_e = engine.clone();
    let input_e = engine.clone();
    let body = view()
        .flex_1()
        .flex_row()
        .px(px(PAD))
        .child(
            view().flex_1().relative().child(
                wheel_area(
                    engine,
                    LINE_H,
                    view().child(dynamic(move || {
                        let Some(p) = parts.get() else { return Vec::new() };
                        p.1.iter()
                            .zip(p.2.iter())
                            .map(|(runs, fold)| {
                                let row = line(runs);
                                match fold {
                                    Some(id) => {
                                        let e = fold_e.clone();
                                        let id = *id;
                                        row.cursor(Cursor::Pointer).on_press(move || e.send(Input::Action(Action::Fold(id)))).into_element()
                                    }
                                    None => row.into_element(),
                                }
                            })
                            .collect::<Vec<_>>()
                    })),
                )
                .absolute()
                .size_full(),
            ),
        )
        .child(scrollbar(engine, bar));
    let bottom = view()
        .flex_1()
        .gap(px(4.0))
        .py(px(4.0))
        .child(dynamic(move || parts.get().map(|p| line(&p.4))))
        .child(dynamic(move || {
            let p = parts.get()?;
            let (input_text, facts) = p.5.clone()?;
            let e = input_e.clone();
            let submit = input_e.clone();
            Some(
                view()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(mono("›".to_string()).text_color(c(ACCENT)))
                    .child(
                        nsh_core::element::input()
                            .flex_1()
                            .font_family("monospace")
                            .text_size(px(FONT))
                            .text_color(c(TEXT))
                            .value(input_text)
                            .placeholder("message the agent — enter sends, alt-enter queues, esc esc aborts".to_string())
                            .auto_focus(true)
                            .on_change(move |t: &str| e.send(Input::Action(Action::Text(t.to_string()))))
                            .on_submit(move |_: &str| submit.send(keys::press(keys::KeyCode::Enter))),
                    )
                    .child(mono(facts).text_color(c(DIM))),
            )
        }))
        .child(dynamic(move || parts.get().map(|p| key_hints(&p.7))));
    detail_frame(
        dynamic(move || parts.get().map(|p| line(&title_runs(&p.0)))),
        mono(move || parts.get().and_then(|p| p.6).unwrap_or_default()).text_color(c(MUTED)),
        body,
        bottom,
    )
}

