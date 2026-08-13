//! The PPU debugger section: layer isolation, palettes, tiles, OAM, and decoded registers.
//!
//! Draws inside the same window [`super::debugger_view`] does, as a section below the CPU views
//! that a checkbox expands — one debugger, not two windows fighting for space. Like every other
//! panel here it reaches nothing: it renders a [`frontend_core::PpuSnapshot`] the emulation
//! thread captured and returns [`UiAction`]s, exactly as the CPU views do. See
//! `CONTRIBUTING.md` rule 3.
//!
//! # Why the section has to be opened to cost anything
//!
//! [`request`] is only ever called while [`Chrome::debugger_ppu_open`] is true — see
//! `App::request_ppu_debug_if_due` — so a closed section asks the emulation thread for nothing,
//! decodes nothing, and sends nothing over the channel. Layer isolation is the one exception: its
//! toggle buttons work whether or not the section below them happens to be expanded, because
//! *seeing* the effect of a toggle costs a snapshot, but *setting* one does not.

use super::{Chrome, ChromeState, UiAction};
use frontend_core::{DebugLayer, LayerOverrides, PpuDebugRequest, SessionCommand, TileBitDepth};

fn mono(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).monospace()
}

fn color32(c: core_common::Rgba8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

/// A small filled square, for a palette or tile pixel. Painted directly rather than as a texture
/// — nothing under `chrome/` reaches the GPU, and immediate-mode rects are what every other
/// panel here uses for a block of solid colour (the bezel in `app.rs`, for one).
fn swatch(ui: &mut egui::Ui, size: f32, color: egui::Color32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, 0.0, color);
    }
    response
}

/// Draw the PPU section. Called from [`super::debugger_view::window`].
///
/// A checkbox rather than a collapsing header: it binds directly to `Chrome`'s own field with no
/// separate egui-internal state to keep in sync, which is what lets
/// `App::request_ppu_debug_if_due` gate on that field alone and be sure it matches what is drawn.
pub fn section(
    chrome: &mut Chrome,
    ui: &mut egui::Ui,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    ui.separator();
    ui.checkbox(
        &mut chrome.debugger_ppu_open,
        "PPU — layers, palettes, tiles, OAM, registers",
    );
    if !chrome.debugger_ppu_open {
        return;
    }

    let Some(reason) = state.ppu_debug_unavailable else {
        let Some(snapshot) = state.ppu_debug else {
            ui.label(
                egui::RichText::new("waiting for the first PPU snapshot…")
                    .small()
                    .weak(),
            );
            return;
        };
        layers(chrome, ui, actions);
        ui.separator();
        palettes(ui, snapshot);
        ui.separator();
        tiles(chrome, ui, snapshot);
        ui.separator();
        oam(chrome, ui, snapshot);
        ui.separator();
        registers(ui, snapshot);
        return;
    };
    ui.label(egui::RichText::new(reason).color(egui::Color32::from_rgb(0xFF, 0xB0, 0x74)));
}

fn layers(chrome: &mut Chrome, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
    ui.label(
        egui::RichText::new("Layers — force-hide or solo, to isolate what one layer draws")
            .small()
            .weak(),
    );

    let mut overrides = chrome.debugger_layer_overrides;
    let mut changed = false;

    ui.horizontal_wrapped(|ui| {
        // Unrolled rather than looped over an array of `&mut` field references: `overrides.solo`
        // and one `bg_hidden` element at a time cannot both be named in one array literal
        // without aliasing it, and four straight-line calls are no less clear than a loop here.
        let mut solo = overrides.solo;
        changed |= layer_toggle(
            ui,
            "BG0",
            &mut overrides.bg_hidden[0],
            &mut solo,
            DebugLayer::Bg0,
        );
        changed |= layer_toggle(
            ui,
            "BG1",
            &mut overrides.bg_hidden[1],
            &mut solo,
            DebugLayer::Bg1,
        );
        changed |= layer_toggle(
            ui,
            "BG2",
            &mut overrides.bg_hidden[2],
            &mut solo,
            DebugLayer::Bg2,
        );
        changed |= layer_toggle(
            ui,
            "BG3",
            &mut overrides.bg_hidden[3],
            &mut solo,
            DebugLayer::Bg3,
        );
        changed |= layer_toggle(
            ui,
            "OBJ",
            &mut overrides.obj_hidden,
            &mut solo,
            DebugLayer::Obj,
        );
        overrides.solo = solo;
        ui.separator();
        for index in 0..overrides.win_hidden.len() {
            let label = if index == 0 { "WIN0" } else { "WIN1" };
            if ui
                .selectable_label(overrides.win_hidden[index], label)
                .on_hover_text(
                    "Force this window off, so every layer draws as though it did not exist",
                )
                .clicked()
            {
                overrides.win_hidden[index] = !overrides.win_hidden[index];
                changed = true;
            }
        }
        if overrides != LayerOverrides::default() && ui.small_button("reset").clicked() {
            overrides = LayerOverrides::default();
            changed = true;
        }
    });

    if changed {
        chrome.debugger_layer_overrides = overrides;
        actions.push(UiAction::Session(SessionCommand::SetLayerOverrides(
            overrides,
        )));
    }
}

/// One layer's hide/solo pair. Returns whether either was clicked.
fn layer_toggle(
    ui: &mut egui::Ui,
    label: &str,
    hidden: &mut bool,
    solo: &mut Option<DebugLayer>,
    layer: DebugLayer,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        if ui
            .selectable_label(*hidden, label)
            .on_hover_text("Force this layer off")
            .clicked()
        {
            *hidden = !*hidden;
            changed = true;
        }
        if ui
            .selectable_label(*solo == Some(layer), "solo")
            .on_hover_text("Show only this layer")
            .clicked()
        {
            *solo = if *solo == Some(layer) {
                None
            } else {
                Some(layer)
            };
            changed = true;
        }
    });
    changed
}

fn palettes(ui: &mut egui::Ui, snapshot: &frontend_core::PpuSnapshot) {
    ui.label(
        egui::RichText::new("Palette — hover a swatch for its raw value")
            .small()
            .weak(),
    );
    ui.horizontal(|ui| {
        palette_grid(ui, "bg-palette", &snapshot.bg_palette, "BG");
        ui.add_space(12.0);
        palette_grid(ui, "obj-palette", &snapshot.sprite_palette, "OBJ");
    });
}

fn palette_grid(
    ui: &mut egui::Ui,
    id: &str,
    swatches: &[frontend_core::PaletteSwatch],
    label: &str,
) {
    ui.vertical(|ui| {
        ui.label(mono(label));
        egui::Grid::new(id)
            .spacing(egui::vec2(0.0, 0.0))
            .show(ui, |ui| {
                for (index, entry) in swatches.iter().enumerate() {
                    swatch(ui, 10.0, color32(entry.color)).on_hover_text(format!(
                        "index {index}: BGR555 {:#06X} -> rgb({}, {}, {})",
                        entry.raw, entry.color.r, entry.color.g, entry.color.b
                    ));
                    if index % 16 == 15 {
                        ui.end_row();
                    }
                }
            });
    });
}

fn tiles(chrome: &mut Chrome, ui: &mut egui::Ui, snapshot: &frontend_core::PpuSnapshot) {
    ui.label(egui::RichText::new("Tiles / VRAM").small().weak());
    ui.horizontal(|ui| {
        ui.label("character block");
        // 16 KiB apart, per `background::CHAR_BLOCK` — six blocks span the whole 96 KiB VRAM
        // makes eight windows onto it here, matching the debugger memory viewer's own jump-list
        // style of moving by named regions rather than a raw byte offset.
        let mut block = chrome.debugger_tile_char_base / 0x4000;
        if ui
            .add(egui::DragValue::new(&mut block).range(0..=5))
            .changed()
        {
            chrome.debugger_tile_char_base = block * 0x4000;
        }
        ui.separator();
        for (label, depth) in [("4bpp", TileBitDepth::Four), ("8bpp", TileBitDepth::Eight)] {
            if ui
                .selectable_label(chrome.debugger_tile_depth == depth, label)
                .clicked()
            {
                chrome.debugger_tile_depth = depth;
            }
        }
        if chrome.debugger_tile_depth == TileBitDepth::Four {
            ui.separator();
            ui.label("bank");
            ui.add(egui::DragValue::new(&mut chrome.debugger_tile_palette_bank).range(0..=15));
        }
    });

    let per_row = 16;
    let pixel = 2.0;
    egui::ScrollArea::vertical()
        .id_salt("tile-viewer")
        .max_height(220.0)
        .show(ui, |ui| {
            egui::Grid::new("tile-grid")
                .spacing(egui::vec2(2.0, 2.0))
                .show(ui, |ui| {
                    for (index, tile) in snapshot.tiles.iter().enumerate() {
                        draw_tile(ui, tile, pixel);
                        if index % per_row == per_row - 1 {
                            ui.end_row();
                        }
                    }
                });
        });
}

fn draw_tile(ui: &mut egui::Ui, tile: &frontend_core::TileBitmap, pixel: f32) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(pixel * 8.0, pixel * 8.0), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        for (index, colour) in tile.pixels.iter().enumerate() {
            let (row, col) = (index / 8, index % 8);
            let min = rect.min + egui::vec2(col as f32 * pixel, row as f32 * pixel);
            painter.rect_filled(
                egui::Rect::from_min_size(min, egui::vec2(pixel, pixel)),
                0.0,
                color32(*colour),
            );
        }
    }
    response.on_hover_text(
        "Raw tile data, decoded through the selected palette bank — index 0 \
                             is shown as its actual colour here, not treated as transparent",
    );
}

fn oam(chrome: &mut Chrome, ui: &mut egui::Ui, snapshot: &frontend_core::PpuSnapshot) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("OAM").small().weak());
        ui.checkbox(&mut chrome.debugger_oam_show_all, "show all 128");
    });

    egui::ScrollArea::vertical()
        .id_salt("oam-viewer")
        .max_height(200.0)
        .show(ui, |ui| {
            egui::Grid::new("oam-grid").striped(true).show(ui, |ui| {
                ui.label(mono("#"));
                ui.label(mono("x,y"));
                ui.label(mono("size"));
                ui.label(mono("pri"));
                ui.label(mono("pal"));
                ui.label(mono("tile"));
                ui.label(mono("affine"));
                ui.label(mono("mode"));
                ui.label(mono("gfx"));
                ui.end_row();

                for row in snapshot
                    .oam
                    .iter()
                    .filter(|row| chrome.debugger_oam_show_all || row.on_current_scanline)
                {
                    let text = if row.on_current_scanline {
                        |s: String| mono(s).color(egui::Color32::from_rgb(0x7A, 0xD7, 0xFF))
                    } else {
                        |s: String| mono(s)
                    };
                    ui.label(text(row.index.to_string()));
                    ui.label(text(format!("{},{}", row.x, row.y)));
                    ui.label(text(format!("{}x{}", row.width, row.height)));
                    ui.label(text(row.priority.to_string()));
                    ui.label(text(row.palette.to_string()));
                    ui.label(text(row.tile.to_string()));
                    ui.label(text(
                        row.affine_index.map_or("-".to_string(), |i| i.to_string()),
                    ));
                    ui.label(text(row.mode.to_string()));
                    ui.label(text(row.graphics_mode.to_string()));
                    ui.end_row();
                }
            });
        });
}

fn registers(ui: &mut egui::Ui, snapshot: &frontend_core::PpuSnapshot) {
    ui.label(egui::RichText::new("Registers").small().weak());
    let r = &snapshot.registers;
    ui.label(mono(format!(
        "DISPCNT={:04X} mode={} forced_blank={} obj_1d={}   DISPSTAT={:04X} VCOUNT={}",
        r.dispcnt, r.mode, r.forced_blank, r.obj_1d_mapping, r.dispstat, r.vcount
    )));
    for (index, bg) in r.backgrounds.iter().enumerate() {
        ui.label(mono(format!(
            "BG{index} {} BG{index}CNT={:04X} priority={} char={:#X} screen={:#X} {}bpp \
             size={}x{} scroll=({},{}){}",
            if bg.enabled { "on " } else { "off" },
            bg.control,
            bg.priority,
            bg.char_base,
            bg.screen_base,
            bg.bpp,
            bg.size_tiles.0,
            bg.size_tiles.1,
            bg.scroll_x,
            bg.scroll_y,
            if bg.mosaic { " mosaic" } else { "" },
        )));
    }
    for (index, win) in r.windows.iter().enumerate() {
        ui.label(mono(format!(
            "WIN{index} {} x {}..{} y {}..{} layers_in={:06b}",
            if win.enabled { "on " } else { "off" },
            win.left,
            win.right,
            win.top,
            win.bottom,
            win.layers_in,
        )));
    }
    ui.label(mono(format!(
        "outside all windows layers={:06b}   object window layers={:06b}",
        r.winout, r.obj_window_layers
    )));
    let effect = match (r.bldcnt >> 6) & 3 {
        0 => "none",
        1 => "alpha blend",
        2 => "brighten toward white",
        _ => "darken toward black",
    };
    ui.label(mono(format!(
        "BLDCNT={:04X} effect={effect} first={:06b} second={:06b} BLDALPHA={:04X} BLDY={}",
        r.bldcnt,
        r.bldcnt & 0x3F,
        (r.bldcnt >> 8) & 0x3F,
        r.bldalpha,
        r.bldy,
    )));
}

/// The request the section wants served, derived from its own selection state — same reasoning
/// as [`super::debugger_view::request`].
pub fn request(chrome: &Chrome) -> PpuDebugRequest {
    PpuDebugRequest {
        tile_char_base: chrome.debugger_tile_char_base,
        // Enough to fill one character block's worth of the grid above without decoding VRAM
        // that view is not showing.
        tile_count: 256,
        tile_depth: chrome.debugger_tile_depth,
        tile_palette_bank: chrome.debugger_tile_palette_bank,
    }
    .clamped()
}
