use egui::{pos2, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use crate::theme::{BORDER_SUBTLE, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY};

pub struct Series<'a> {
    pub label: &'a str,
    pub color: Color32,
    pub values: &'a [f64],
    pub fill_alpha: u8,
}

pub struct ThresholdLine<'a> {
    pub label: &'a str,
    pub value: f64,
    pub color: Color32,
}

/// Renders a multi-series percentage (0..y_max) time-series chart with area fill,
/// horizontal threshold markers, gridlines, and hover crosshair inspection.
pub fn draw_time_series_chart(
    ui: &mut egui::Ui,
    title: &str,
    unit: &str,
    y_max: f64,
    height: f32,
    timestamps: &[String],
    series: &[Series<'_>],
    thresholds: &[ThresholdLine<'_>],
) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(title).size(13.5).strong().color(TEXT_PRIMARY));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for s in series.iter().rev() {
                let latest = s.values.last().copied().unwrap_or(0.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    ui.painter().circle_filled(dot_rect.center(), 3.5, s.color);
                    ui.label(
                        egui::RichText::new(format!("{} {:.1}{}", s.label, latest, unit))
                            .size(11.0)
                            .monospace()
                            .color(TEXT_SECONDARY),
                    );
                });
            }
        });
    });

    ui.add_space(4.0);

    let avail_w = ui.available_width().max(120.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(avail_w, height), Sense::hover());
    let painter = ui.painter_at(rect);

    // Plot area with left margin for Y labels
    let left_pad = 36.0;
    let right_pad = 8.0;
    let top_pad = 6.0;
    let bottom_pad = 16.0;

    let plot = Rect::from_min_max(
        pos2(rect.min.x + left_pad, rect.min.y + top_pad),
        pos2(rect.max.x - right_pad, rect.max.y - bottom_pad),
    );

    // Subtle dark canvas
    painter.rect_filled(plot, CornerRadius::same(6), Color32::from_rgb(13, 18, 28));
    painter.rect_stroke(
        plot,
        CornerRadius::same(6),
        Stroke::new(1.0, BORDER_SUBTLE),
        egui::StrokeKind::Inside,
    );

    if plot.width() < 20.0 || plot.height() < 20.0 {
        return;
    }

    let y_max = y_max.max(1.0);
    let val_to_y = |v: f64| -> f32 {
        let clamped = (v / y_max).clamp(0.0, 1.0) as f32;
        plot.max.y - clamped * plot.height()
    };

    // Gridlines (0, 25, 50, 75, 100%)
    for i in 0..=4 {
        let frac = i as f64 / 4.0;
        let val = frac * y_max;
        let y = val_to_y(val);
        if i > 0 && i < 4 {
            painter.line_segment(
                [pos2(plot.min.x, y), pos2(plot.max.x, y)],
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(45, 58, 82, 90)),
            );
        }
        let label_txt = if y_max >= 100.0 {
            format!("{:.0}{}", val, unit)
        } else {
            format!("{:.0}", val)
        };
        painter.text(
            pos2(plot.min.x - 5.0, y),
            Align2::RIGHT_CENTER,
            label_txt,
            FontId::monospace(9.5),
            TEXT_MUTED,
        );
    }

    // Threshold dashed lines
    for th in thresholds {
        if th.value <= y_max {
            let y = val_to_y(th.value);
            let col = Color32::from_rgba_unmultiplied(th.color.r(), th.color.g(), th.color.b(), 145);
            draw_dashed_hline(&painter, plot.min.x, plot.max.x, y, col);
            painter.text(
                pos2(plot.max.x - 4.0, y - 2.0),
                Align2::RIGHT_BOTTOM,
                th.label,
                FontId::proportional(9.0),
                col,
            );
        }
    }

    // Series curves
    let max_len = series.iter().map(|s| s.values.len()).max().unwrap_or(0);
    if max_len >= 2 {
        let step_x = plot.width() / (max_len.saturating_sub(1).max(1) as f32);

        // Draw secondary/fill series first, primary last
        for s in series.iter().rev() {
            if s.values.len() < 2 {
                continue;
            }
            let offset = max_len.saturating_sub(s.values.len());
            let mut pts: Vec<Pos2> = Vec::with_capacity(s.values.len());
            for (idx, &v) in s.values.iter().enumerate() {
                let x = plot.min.x + ((offset + idx) as f32) * step_x;
                let y = val_to_y(v);
                pts.push(pos2(x, y));
            }

            if s.fill_alpha > 0 && pts.len() >= 2 {
                let fill_col = Color32::from_rgba_unmultiplied(
                    s.color.r(),
                    s.color.g(),
                    s.color.b(),
                    s.fill_alpha,
                );
                let mut mesh = egui::Mesh::default();
                for i in 0..pts.len() - 1 {
                    let p0 = pts[i];
                    let p1 = pts[i + 1];
                    let b0 = pos2(p0.x, plot.max.y);
                    let b1 = pos2(p1.x, plot.max.y);
                    let base = mesh.vertices.len() as u32;
                    mesh.colored_vertex(p0, fill_col);
                    mesh.colored_vertex(p1, fill_col);
                    let bot_col = Color32::from_rgba_unmultiplied(s.color.r(), s.color.g(), s.color.b(), 4);
                    mesh.colored_vertex(b1, bot_col);
                    mesh.colored_vertex(b0, bot_col);
                    mesh.add_triangle(base, base + 1, base + 2);
                    mesh.add_triangle(base, base + 2, base + 3);
                }
                painter.add(Shape::mesh(mesh));
            }

            painter.add(Shape::line(pts, Stroke::new(1.8, s.color)));
        }

        // X-axis first/last timestamps
        if let (Some(first), Some(last)) = (timestamps.first(), timestamps.last()) {
            painter.text(
                pos2(plot.min.x + 2.0, plot.max.y + 3.0),
                Align2::LEFT_TOP,
                first,
                FontId::monospace(9.5),
                TEXT_MUTED,
            );
            painter.text(
                pos2(plot.max.x - 2.0, plot.max.y + 3.0),
                Align2::RIGHT_TOP,
                last,
                FontId::monospace(9.5),
                TEXT_MUTED,
            );
        }

        // Hover crosshair
        if let Some(hover_pos) = response.hover_pos() {
            if plot.contains(hover_pos) {
                let rel = ((hover_pos.x - plot.min.x) / plot.width()).clamp(0.0, 1.0);
                let idx = ((rel * ((max_len - 1) as f32)).round() as usize).min(max_len - 1);
                let cx = plot.min.x + (idx as f32) * step_x;
                painter.line_segment(
                    [pos2(cx, plot.min.y), pos2(cx, plot.max.y)],
                    Stroke::new(1.0, Color32::from_rgba_unmultiplied(200, 215, 235, 110)),
                );

                for s in series {
                    if let Some(&v) = s.values.get(idx) {
                        let cy = val_to_y(v);
                        painter.circle_filled(pos2(cx, cy), 3.5, s.color);
                    }
                }

                let ts = timestamps.get(idx).map(|s| s.as_str()).unwrap_or("");
                response.on_hover_ui_at_pointer(|ui| {
                    ui.label(egui::RichText::new(ts).monospace().size(11.0).color(TEXT_MUTED));
                    for s in series {
                        if let Some(&v) = s.values.get(idx) {
                            ui.label(
                                egui::RichText::new(format!("{}: {:.1}{}", s.label, v, unit))
                                    .monospace()
                                    .size(11.5)
                                    .color(s.color),
                            );
                        }
                    }
                });
            }
        }
    } else {
        painter.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "Collecting telemetry...",
            FontId::proportional(11.5),
            TEXT_MUTED,
        );
    }
}

fn draw_dashed_hline(painter: &egui::Painter, x0: f32, x1: f32, y: f32, color: Color32) {
    let dash = 5.0;
    let gap = 4.0;
    let mut x = x0;
    while x < x1 {
        let end = (x + dash).min(x1);
        painter.line_segment([pos2(x, y), pos2(end, y)], Stroke::new(1.0, color));
        x += dash + gap;
    }
}

