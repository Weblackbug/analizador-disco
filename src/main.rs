#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Tamaño mínimo que se guarda durante el escaneo (1 MB).
/// El medidor de la interfaz filtra sobre esta lista sin re-escanear.
const MIN_TRACK: u64 = 1024 * 1024;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Analizador de Disco — Archivos Grandes")
            .with_maximized(true)
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Analizador de Disco",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_zoom_factor(1.1);
            Ok(Box::new(App::new(cc)))
        }),
    )
}

// ---------------------------------------------------------------------------

struct FileEntry {
    path: PathBuf,
    size: u64,
    selected: bool,
}

enum ScanMsg {
    Entry(PathBuf, u64),
    Done,
}

struct ScanState {
    scanned: Arc<AtomicU64>,
    total_bytes: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<ScanMsg>,
    root: String,
}

struct DriveInfo {
    mount: String,
    total: u64,
    avail: u64,
}

struct App {
    drives: Vec<DriveInfo>,
    selected_drive: usize,
    threshold_mb: f64,
    prev_threshold_mb: f64,
    entries: Vec<FileEntry>,
    filtered: Vec<usize>,
    filtered_total: u64,
    filter_dirty: bool,
    scan: Option<ScanState>,
    scanning: bool,
    permanent_delete: bool,
    confirm_delete: bool,
    result_modal: Option<String>,
    about_open: bool,
    logo_tex: Option<egui::TextureHandle>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let logo_tex = image::load_from_memory(include_bytes!("logo.png"))
            .ok()
            .map(|im| {
                let rgba = im.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                let ci = egui::ColorImage::from_rgba_unmultiplied(
                    size,
                    rgba.as_flat_samples().as_slice(),
                );
                cc.egui_ctx
                    .load_texture("logo", ci, egui::TextureOptions::LINEAR)
            });
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let mut drives: Vec<DriveInfo> = disks
            .list()
            .iter()
            .map(|d| DriveInfo {
                mount: d.mount_point().to_string_lossy().into_owned(),
                total: d.total_space(),
                avail: d.available_space(),
            })
            .collect();
        drives.sort_by(|a, b| a.mount.cmp(&b.mount));
        drives.dedup_by(|a, b| a.mount == b.mount);

        Self {
            drives,
            selected_drive: 0,
            threshold_mb: 100.0,
            prev_threshold_mb: 100.0,
            entries: Vec::new(),
            filtered: Vec::new(),
            filtered_total: 0,
            filter_dirty: false,
            scan: None,
            scanning: false,
            permanent_delete: false,
            confirm_delete: false,
            result_modal: None,
            about_open: false,
            logo_tex,
        }
    }

    fn start_scan(&mut self) {
        if let Some(scan) = &self.scan {
            scan.cancel.store(true, Ordering::Relaxed);
        }
        let root = self.drives[self.selected_drive].mount.clone();
        let (tx, rx) = mpsc::channel();
        let scanned = Arc::new(AtomicU64::new(0));
        let total_bytes = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));

        self.entries.clear();
        self.filter_dirty = true;
        self.scanning = true;
        self.scan = Some(ScanState {
            scanned: scanned.clone(),
            total_bytes: total_bytes.clone(),
            cancel: cancel.clone(),
            rx,
            root: root.clone(),
        });

        std::thread::spawn(move || {
            let walker = jwalk::WalkDir::new(&root)
                .skip_hidden(false)
                .follow_links(false);
            for entry in walker.into_iter().flatten() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                scanned.fetch_add(1, Ordering::Relaxed);
                if entry.file_type().is_file() {
                    if let Ok(md) = entry.metadata() {
                        let size = md.len();
                        total_bytes.fetch_add(size, Ordering::Relaxed);
                        if size >= MIN_TRACK {
                            let _ = tx.send(ScanMsg::Entry(entry.path(), size));
                        }
                    }
                }
            }
            let _ = tx.send(ScanMsg::Done);
        });
    }

    fn recompute_filter(&mut self) {
        let threshold = (self.threshold_mb * 1024.0 * 1024.0) as u64;
        let entries = &self.entries;
        let mut filtered: Vec<usize> = (0..entries.len())
            .filter(|&i| entries[i].size >= threshold)
            .collect();
        filtered.sort_unstable_by(|&a, &b| entries[b].size.cmp(&entries[a].size));
        self.filtered_total = filtered.iter().map(|&i| entries[i].size).sum();
        self.filtered = filtered;
        self.filter_dirty = false;
    }

    fn perform_delete(&mut self) {
        let mut ok = 0usize;
        let mut freed = 0u64;
        let mut errors: Vec<String> = Vec::new();
        let mut deleted: HashSet<PathBuf> = HashSet::new();

        for &i in &self.filtered {
            let e = &self.entries[i];
            if !e.selected {
                continue;
            }
            let res = if self.permanent_delete {
                std::fs::remove_file(&e.path).map_err(|er| er.to_string())
            } else {
                trash::delete(&e.path).map_err(|er| er.to_string())
            };
            match res {
                Ok(()) => {
                    ok += 1;
                    freed += e.size;
                    deleted.insert(e.path.clone());
                }
                Err(msg) => errors.push(format!("{}: {}", e.path.display(), msg)),
            }
        }

        self.entries.retain(|e| !deleted.contains(&e.path));
        self.filter_dirty = true;

        let destino = if self.permanent_delete {
            "eliminados permanentemente"
        } else {
            "enviados a la Papelera de reciclaje"
        };
        let mut msg = format!("{} archivos {} ({} liberados).", ok, destino, fmt_size(freed));
        if !errors.is_empty() {
            msg.push_str(&format!("\n\nNo se pudieron eliminar {}:\n", errors.len()));
            let shown: Vec<String> = errors.iter().take(8).cloned().collect();
            msg.push_str(&shown.join("\n"));
            if errors.len() > 8 {
                msg.push_str(&format!("\n… y {} más", errors.len() - 8));
            }
        }
        self.result_modal = Some(msg);
    }
}

// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Recoger resultados del hilo de escaneo
        let mut msgs = Vec::new();
        if let Some(scan) = &self.scan {
            while let Ok(m) = scan.rx.try_recv() {
                msgs.push(m);
            }
        }
        if !msgs.is_empty() {
            for m in msgs {
                match m {
                    ScanMsg::Entry(path, size) => self.entries.push(FileEntry {
                        path,
                        size,
                        selected: false,
                    }),
                    ScanMsg::Done => {
                        self.scanning = false;
                    }
                }
            }
            self.filter_dirty = true;
        }
        if self.scanning {
            ctx.request_repaint_after(Duration::from_millis(150));
        }

        if self.threshold_mb != self.prev_threshold_mb {
            self.prev_threshold_mb = self.threshold_mb;
            self.filter_dirty = true;
        }
        if self.filter_dirty {
            self.recompute_filter();
        }

        let sel_count = self
            .filtered
            .iter()
            .filter(|&&i| self.entries[i].selected)
            .count();
        let sel_size: u64 = self
            .filtered
            .iter()
            .filter(|&&i| self.entries[i].selected)
            .map(|&i| self.entries[i].size)
            .sum();

        // ------------------------------------------------------ Panel superior
        egui::TopBottomPanel::top("controles").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Unidad:").strong());
                let current = self
                    .drives
                    .get(self.selected_drive)
                    .map(|d| d.mount.clone())
                    .unwrap_or_else(|| "—".into());
                egui::ComboBox::from_id_salt("unidad")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for (i, d) in self.drives.iter().enumerate() {
                            let label = format!(
                                "{}  ({} libres de {})",
                                d.mount,
                                fmt_size(d.avail),
                                fmt_size(d.total)
                            );
                            ui.selectable_value(&mut self.selected_drive, i, label);
                        }
                    });

                if let Some(d) = self.drives.get(self.selected_drive) {
                    let used = d.total.saturating_sub(d.avail);
                    let frac = if d.total > 0 {
                        used as f32 / d.total as f32
                    } else {
                        0.0
                    };
                    ui.add_sized(
                        [260.0, 18.0],
                        egui::ProgressBar::new(frac).text(format!(
                            "{} usados de {}",
                            fmt_size(used),
                            fmt_size(d.total)
                        )),
                    );
                }

                ui.separator();

                if self.scanning {
                    if ui.button("⏹ Detener").clicked() {
                        if let Some(scan) = &self.scan {
                            scan.cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    ui.spinner();
                } else if ui
                    .add_enabled(
                        !self.drives.is_empty(),
                        egui::Button::new(egui::RichText::new("🔍 Escanear disco").strong()),
                    )
                    .clicked()
                {
                    self.start_scan();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("ℹ Acerca de…").clicked() {
                        self.about_open = true;
                    }
                });
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Medidor — tamaño mínimo:").strong());
                ui.add(
                    egui::Slider::new(&mut self.threshold_mb, 1.0..=102400.0)
                        .logarithmic(true)
                        .show_value(false),
                );
                ui.label(
                    egui::RichText::new(fmt_mb(self.threshold_mb))
                        .strong()
                        .size(16.0),
                );
                ui.separator();
                ui.label("Rápido:");
                for (label, mb) in [
                    ("10 MB", 10.0),
                    ("100 MB", 100.0),
                    ("500 MB", 500.0),
                    ("1 GB", 1024.0),
                    ("5 GB", 5120.0),
                ] {
                    if ui.selectable_label(self.threshold_mb == mb, label).clicked() {
                        self.threshold_mb = mb;
                    }
                }
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("☑ Seleccionar todo").clicked() {
                    for &i in &self.filtered {
                        self.entries[i].selected = true;
                    }
                }
                if ui.button("☐ Quitar selección").clicked() {
                    for &i in &self.filtered {
                        self.entries[i].selected = false;
                    }
                }
                ui.separator();
                ui.checkbox(
                    &mut self.permanent_delete,
                    "Eliminación permanente (sin Papelera)",
                );
                ui.separator();
                let btn = egui::Button::new(
                    egui::RichText::new(format!(
                        "🗑 Eliminar seleccionados ({} — {})",
                        sel_count,
                        fmt_size(sel_size)
                    ))
                    .color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgb(178, 34, 34));
                if ui.add_enabled(sel_count > 0, btn).clicked() {
                    self.confirm_delete = true;
                }
            });
            ui.add_space(6.0);
        });

        // ------------------------------------------------------ Barra inferior
        egui::TopBottomPanel::bottom("estado").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if let Some(scan) = &self.scan {
                    let n = scan.scanned.load(Ordering::Relaxed);
                    let bytes = scan.total_bytes.load(Ordering::Relaxed);
                    if self.scanning {
                        ui.spinner();
                        ui.label(format!(
                            "Escaneando {} …  {} elementos examinados  ·  {} analizados  ·  {} candidatos",
                            scan.root,
                            fmt_num(n),
                            fmt_size(bytes),
                            fmt_num(self.entries.len() as u64)
                        ));
                    } else {
                        ui.label(format!(
                            "✔ Escaneo de {} completado: {} elementos examinados ({}).",
                            scan.root,
                            fmt_num(n),
                            fmt_size(bytes)
                        ));
                    }
                    ui.separator();
                }
                ui.label(format!(
                    "Mostrando {} archivos ≥ {}  ·  Total: {}",
                    fmt_num(self.filtered.len() as u64),
                    fmt_mb(self.threshold_mb),
                    fmt_size(self.filtered_total)
                ));
            });
            ui.add_space(4.0);
        });

        // ------------------------------------------------------ Tabla central
        let mut open_path: Option<PathBuf> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.entries.is_empty() && !self.scanning {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new(
                            "Elige una unidad y pulsa «🔍 Escanear disco» para empezar.",
                        )
                        .size(18.0)
                        .weak(),
                    );
                });
                return;
            }

            use egui_extras::{Column, TableBuilder};
            let entries = &mut self.entries;
            let filtered = &self.filtered;

            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .column(Column::exact(30.0))
                .column(Column::exact(44.0))
                .column(Column::exact(110.0))
                .column(Column::initial(320.0).at_least(140.0).clip(true))
                .column(Column::remainder().clip(true))
                .column(Column::exact(46.0))
                .header(24.0, |mut header| {
                    header.col(|ui| {
                        ui.strong("Sel.");
                    });
                    header.col(|ui| {
                        ui.strong("Aviso");
                    });
                    header.col(|ui| {
                        ui.strong("Tamaño");
                    });
                    header.col(|ui| {
                        ui.strong("Archivo");
                    });
                    header.col(|ui| {
                        ui.strong("Directorio");
                    });
                    header.col(|ui| {
                        ui.strong("Abrir");
                    });
                })
                .body(|body| {
                    body.rows(26.0, filtered.len(), |mut row| {
                        let idx = filtered[row.index()];
                        let e = &mut entries[idx];
                        row.col(|ui| {
                            ui.checkbox(&mut e.selected, "");
                        });
                        row.col(|ui| {
                            if let Some(info) = windows_file_info(&e.path) {
                                let (icon_rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(20.0, 20.0),
                                    egui::Sense::hover(),
                                );
                                let p = ui.painter();
                                p.circle_filled(
                                    icon_rect.center(),
                                    9.0,
                                    egui::Color32::from_rgb(255, 200, 0),
                                );
                                p.text(
                                    icon_rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "?",
                                    egui::FontId::proportional(14.0),
                                    egui::Color32::BLACK,
                                );
                                resp.on_hover_ui(|ui| {
                                    ui.set_max_width(380.0);
                                    ui.strong(info.titulo);
                                    ui.label(info.funcion);
                                    ui.add_space(4.0);
                                    ui.colored_label(
                                        egui::Color32::from_rgb(255, 200, 0),
                                        format!("Si se borra: {}", info.si_se_borra),
                                    );
                                });
                            }
                        });
                        row.col(|ui| {
                            ui.monospace(fmt_size(e.size));
                        });
                        row.col(|ui| {
                            let name = e
                                .path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            ui.label(name).on_hover_text(e.path.display().to_string());
                        });
                        row.col(|ui| {
                            let dir = e
                                .path
                                .parent()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            ui.label(dir);
                        });
                        row.col(|ui| {
                            if ui
                                .button("📂")
                                .on_hover_text("Mostrar en el Explorador")
                                .clicked()
                            {
                                open_path = Some(e.path.clone());
                            }
                        });
                    });
                });
        });

        if let Some(path) = open_path {
            let _ = std::process::Command::new("explorer")
                .arg(format!("/select,{}", path.display()))
                .spawn();
        }

        // ------------------------------------------------ Modal de confirmación
        if self.confirm_delete {
            let warn_count = self
                .filtered
                .iter()
                .filter(|&&i| {
                    self.entries[i].selected && windows_file_info(&self.entries[i].path).is_some()
                })
                .count();
            let modal = egui::Modal::new(egui::Id::new("confirmar")).show(ctx, |ui| {
                ui.set_width(440.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    ui.heading("⚠ Confirmar eliminación");
                    ui.add_space(10.0);
                    ui.label(format!(
                        "Vas a eliminar {} archivos ({} en total).",
                        sel_count,
                        fmt_size(sel_size)
                    ));
                    ui.add_space(4.0);
                    if self.permanent_delete {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 60, 60),
                            "Eliminación PERMANENTE: no se podrán recuperar.",
                        );
                    } else {
                        ui.label("Se enviarán a la Papelera de reciclaje.");
                    }
                    if warn_count > 0 {
                        ui.add_space(6.0);
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 200, 0),
                            format!(
                                "¡Atención! {} de los archivos seleccionados están marcados\ncomo importantes para Windows (icono amarillo «?»).\nRevisa su información antes de continuar.",
                                warn_count
                            ),
                        );
                    }
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 220.0).max(0.0) / 2.0;
                        ui.add_space(w);
                        if ui
                            .add_sized([100.0, 32.0], egui::Button::new("Cancelar"))
                            .clicked()
                        {
                            self.confirm_delete = false;
                        }
                        ui.add_space(20.0);
                        let del_btn = egui::Button::new(
                            egui::RichText::new("Eliminar").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(178, 34, 34));
                        if ui.add_sized([100.0, 32.0], del_btn).clicked() {
                            self.perform_delete();
                            self.confirm_delete = false;
                        }
                    });
                    ui.add_space(6.0);
                });
            });
            if modal.should_close() {
                self.confirm_delete = false;
            }
        }

        // ---------------------------------------------------- Modal Acerca de
        if self.about_open {
            ctx.request_repaint(); // animación continua
            let modal = egui::Modal::new(egui::Id::new("acerca_de")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    ui.heading("Analizador de Disco");
                    ui.label(egui::RichText::new("versión 1.0").weak());
                    ui.add_space(8.0);

                    // Rotozoom en mosaico: la imagen repetida llena todo el
                    // recuadro girando, con zoom oscilando entre 800 % y 30 %
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(400.0, 250.0),
                        egui::Sense::hover(),
                    );
                    if let Some(tex) = &self.logo_tex {
                        let t = ui.input(|i| i.time) as f32;
                        let phase = 0.5 + 0.5 * (t * 1.5).sin(); // ciclo ~4 s
                        let scale = 0.3 + (8.0 - 0.3) * phase; // 30 % … 800 %
                        let angle = t * 1.2;
                        let ts = tex.size_vec2();
                        let base = 110.0; // ancho del azulejo al 100 %
                        let tile = egui::vec2(base, base * ts.y / ts.x) * scale;
                        let painter = ui.painter_at(rect);
                        let (sin_a, cos_a) = angle.sin_cos();
                        let center = rect.center();
                        // azulejos suficientes para cubrir el rectángulo rotado
                        let radius = 0.5 * rect.size().length();
                        let nx = (radius / tile.x).ceil() as i32;
                        let ny = (radius / tile.y).ceil() as i32;
                        let mut mesh = egui::Mesh::with_texture(tex.id());
                        for j in -ny..=ny {
                            for i in -nx..=nx {
                                let x0 = (i as f32 - 0.5) * tile.x;
                                let y0 = (j as f32 - 0.5) * tile.y;
                                let corners = [
                                    (x0, y0, egui::pos2(0.0, 0.0)),
                                    (x0 + tile.x, y0, egui::pos2(1.0, 0.0)),
                                    (x0 + tile.x, y0 + tile.y, egui::pos2(1.0, 1.0)),
                                    (x0, y0 + tile.y, egui::pos2(0.0, 1.0)),
                                ];
                                let idx = mesh.vertices.len() as u32;
                                for (x, y, uv) in corners {
                                    mesh.vertices.push(egui::epaint::Vertex {
                                        pos: center
                                            + egui::vec2(
                                                x * cos_a - y * sin_a,
                                                x * sin_a + y * cos_a,
                                            ),
                                        uv,
                                        color: egui::Color32::WHITE,
                                    });
                                }
                                mesh.indices.extend_from_slice(&[
                                    idx,
                                    idx + 1,
                                    idx + 2,
                                    idx,
                                    idx + 2,
                                    idx + 3,
                                ]);
                            }
                        }
                        painter.add(egui::Shape::mesh(mesh));
                    } else {
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "(logo.png no disponible)",
                            egui::FontId::proportional(14.0),
                            ui.visuals().weak_text_color(),
                        );
                    }

                    ui.add_space(8.0);
                    ui.label("© 2026 BitxoSoft. Todos los derechos reservados.");
                    ui.add_space(12.0);
                    if ui
                        .add_sized([110.0, 32.0], egui::Button::new(egui::RichText::new("OK").strong()))
                        .clicked()
                    {
                        self.about_open = false;
                    }
                    ui.add_space(6.0);
                });
            });
            if modal.should_close() {
                self.about_open = false;
            }
        }

        // --------------------------------------------------- Modal de resultado
        if self.result_modal.is_some() {
            let mut close = false;
            let msg = self.result_modal.clone().unwrap_or_default();
            let modal = egui::Modal::new(egui::Id::new("resultado")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    ui.heading("Resultado");
                    ui.add_space(10.0);
                    ui.label(msg);
                    ui.add_space(14.0);
                    if ui
                        .add_sized([110.0, 32.0], egui::Button::new("Aceptar"))
                        .clicked()
                    {
                        close = true;
                    }
                    ui.add_space(6.0);
                });
            });
            if close || modal.should_close() {
                self.result_modal = None;
            }
        }
    }
}

// ---------------------------------------------------------------------------

/// Base de datos de archivos y carpetas importantes de Windows:
/// qué función cumplen y qué pasaría si se borran.
struct WinFileInfo {
    titulo: &'static str,
    funcion: &'static str,
    si_se_borra: &'static str,
}

fn windows_file_info(path: &std::path::Path) -> Option<WinFileInfo> {
    fn info(
        titulo: &'static str,
        funcion: &'static str,
        si_se_borra: &'static str,
    ) -> Option<WinFileInfo> {
        Some(WinFileInfo {
            titulo,
            funcion,
            si_se_borra,
        })
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let full = path.to_string_lossy().to_lowercase();

    // --- Archivos concretos ---
    match name.as_str() {
        "pagefile.sys" => {
            return info(
                "Archivo de paginación (memoria virtual)",
                "Es la memoria virtual de Windows: cuando la RAM se llena, los datos se mueven aquí para que el sistema siga funcionando.",
                "Windows suele tenerlo bloqueado y no deja borrarlo en uso. Si desapareciera, los programas podrían quedarse sin memoria y fallar; se recrea al reiniciar. Su tamaño se ajusta en Configuración avanzada del sistema.",
            )
        }
        "hiberfil.sys" => {
            return info(
                "Archivo de hibernación",
                "Guarda el contenido de la RAM al hibernar y hace posible el «Inicio rápido» de Windows.",
                "Perderías la hibernación y el Inicio rápido. La forma correcta de eliminarlo es ejecutar «powercfg /h off» como administrador, no borrarlo a mano.",
            )
        }
        "swapfile.sys" => {
            return info(
                "Archivo de intercambio de apps",
                "Archivo de paginación específico para las aplicaciones de Microsoft Store.",
                "Las apps de la Tienda podrían fallar temporalmente; Windows lo recrea al reiniciar.",
            )
        }
        "ntoskrnl.exe" => {
            return info(
                "Núcleo de Windows (kernel)",
                "Es el corazón del sistema operativo: gestiona la memoria, los procesos y el hardware.",
                "Windows NO volvería a arrancar. Nunca debe borrarse.",
            )
        }
        "explorer.exe" => {
            return info(
                "Explorador de Windows",
                "Dibuja el escritorio, la barra de tareas, el menú Inicio y las ventanas de carpetas.",
                "Al iniciar sesión no habría escritorio ni barra de tareas: pantalla vacía.",
            )
        }
        "ntdll.dll" | "kernel32.dll" | "user32.dll" | "gdi32.dll" | "hal.dll" => {
            return info(
                "Biblioteca esencial del sistema",
                "DLL fundamental que usan casi todos los programas de Windows para funcionar.",
                "El sistema no arrancaría o ningún programa podría abrirse. Nunca debe borrarse.",
            )
        }
        "bootmgr" | "bootmgfw.efi" | "winload.exe" | "winload.efi" => {
            return info(
                "Gestor / cargador de arranque",
                "Es la pieza que carga Windows cuando enciendes el equipo.",
                "El equipo no podría iniciar el sistema operativo: pantalla de error al encender.",
            )
        }
        "smss.exe" | "csrss.exe" | "wininit.exe" | "winlogon.exe" | "services.exe"
        | "lsass.exe" | "svchost.exe" => {
            return info(
                "Proceso esencial del sistema",
                "Proceso crítico de Windows: inicio de sesión, servicios y seguridad del sistema.",
                "Windows no funcionaría correctamente o no podrías iniciar sesión.",
            )
        }
        "ntuser.dat" => {
            return info(
                "Registro del perfil de usuario",
                "Contiene toda la configuración personal (registro) de un usuario de este equipo.",
                "Ese perfil se restablecería de fábrica: se perdería su configuración personal.",
            )
        }
        "usrclass.dat" => {
            return info(
                "Registro de clases del usuario",
                "Parte del registro del perfil: asociaciones de archivos y configuración de la interfaz.",
                "Se perderían asociaciones de archivos y ajustes de la interfaz de ese usuario.",
            )
        }
        "memory.dmp" => {
            return info(
                "Volcado de memoria (pantallazo azul)",
                "Se crea tras un error grave del sistema y sirve para diagnosticar la causa.",
                "No pasa nada: es seguro borrarlo si no necesitas investigar ningún cuelgue.",
            )
        }
        _ => {}
    }

    // --- Ubicaciones (de más específica a más general) ---
    if full.contains("\\windows\\minidump\\") && name.ends_with(".dmp") {
        return info(
            "Minivolcado de errores",
            "Registro pequeño creado tras un pantallazo azul, útil solo para diagnóstico.",
            "No pasa nada: es seguro borrarlo si no estás investigando cuelgues.",
        );
    }
    if full.contains("\\windows\\system32\\config\\") {
        return info(
            "Registro de Windows (colmena)",
            "Aquí viven las colmenas del Registro (SYSTEM, SOFTWARE, SAM…): toda la configuración del sistema.",
            "Windows no arrancaría o perdería toda su configuración. Nunca debe borrarse.",
        );
    }
    if full.contains("\\windows\\winsxs\\") {
        return info(
            "Almacén de componentes (WinSxS)",
            "Guarda todas las versiones de los archivos del sistema y de las actualizaciones instaladas.",
            "Borrar aquí a mano puede romper Windows Update y la reparación del sistema. Límpialo solo con el Liberador de espacio en disco o «DISM /StartComponentCleanup».",
        );
    }
    if full.contains("\\windows\\installer\\") {
        return info(
            "Caché de instaladores",
            "Copias de los instaladores (.msi/.msp) de los programas instalados en el equipo.",
            "No podrías desinstalar, reparar ni actualizar esos programas correctamente.",
        );
    }
    if full.contains("\\windows\\servicing\\") {
        return info(
            "Componentes de Windows Update",
            "Archivos que Windows Update necesita para instalar actualizaciones.",
            "Las actualizaciones de Windows podrían fallar a partir de entonces.",
        );
    }
    if full.contains("\\windows\\softwaredistribution\\") {
        return info(
            "Descargas de Windows Update",
            "Almacén temporal de las actualizaciones descargadas pendientes o ya aplicadas.",
            "Generalmente seguro: Windows vuelve a descargar lo que necesite. Mejor vaciarlo con el servicio Windows Update detenido.",
        );
    }
    if full.contains("\\system volume information\\") {
        return info(
            "Restaurar sistema",
            "Puntos de restauración e instantáneas (copias de seguridad internas) de la unidad.",
            "Perderías los puntos de restauración. Gestiónalo desde «Protección del sistema», no borrando a mano.",
        );
    }
    if full.contains("$recycle.bin") {
        return info(
            "Papelera de reciclaje",
            "Archivos ya borrados que esperan ser recuperados o eliminados definitivamente.",
            "Se perdería definitivamente el contenido de la Papelera. Sin más consecuencias.",
        );
    }
    if full.contains("\\program files\\windowsapps\\") {
        return info(
            "Apps de Microsoft Store",
            "Carpeta protegida donde se instalan las aplicaciones de la Tienda.",
            "Las apps afectadas dejarían de funcionar. Desinstálalas desde Configuración → Aplicaciones.",
        );
    }
    if full.contains("\\windows\\") {
        return info(
            "Archivo de la carpeta Windows",
            "Está dentro de la carpeta del sistema operativo.",
            "Precaución: podría ser necesario para Windows o para alguno de sus componentes. Bórralo solo si sabes exactamente qué es.",
        );
    }
    None
}

fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

fn fmt_mb(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{:.0} MB", mb)
    }
}

fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(c);
    }
    out
}
