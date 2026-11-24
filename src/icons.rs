use crate::model::CategoryIcon;
use egui::{pos2, vec2, Color32, Painter, Rect, Shape, Stroke};

pub fn paint_play(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.22);
    let pts = vec![
        pos2(r.left() + r.width() * 0.12, r.top()),
        pos2(r.right(), r.center().y),
        pos2(r.left() + r.width() * 0.12, r.bottom()),
    ];
    p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
}

pub fn paint_pause(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.24);
    let bar_w = (r.width() * 0.30).max(2.0);
    p.rect_filled(
        Rect::from_min_max(r.min, pos2(r.left() + bar_w, r.bottom())),
        1.0,
        color,
    );
    p.rect_filled(
        Rect::from_min_max(pos2(r.right() - bar_w, r.top()), r.max),
        1.0,
        color,
    );
}

pub fn paint_stop(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.26);
    p.rect_filled(r, 1.0, color);
}

pub fn paint_record(p: &Painter, rect: Rect, color: Color32) {
    p.circle_filled(rect.center(), rect.width() * 0.25, color);
}

pub fn paint_prev(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.20);
    let bar_w = (r.width() * 0.18).max(2.0);
    p.rect_filled(
        Rect::from_min_max(r.min, pos2(r.left() + bar_w, r.bottom())),
        0.5,
        color,
    );
    let pts = vec![
        pos2(r.left() + bar_w + 1.0, r.center().y),
        pos2(r.right(), r.top()),
        pos2(r.right(), r.bottom()),
    ];
    p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
}

pub fn paint_next(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.20);
    let bar_w = (r.width() * 0.18).max(2.0);
    let pts = vec![
        pos2(r.left(), r.top()),
        pos2(r.right() - bar_w - 1.0, r.center().y),
        pos2(r.left(), r.bottom()),
    ];
    p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
    p.rect_filled(
        Rect::from_min_max(pos2(r.right() - bar_w, r.top()), r.max),
        0.5,
        color,
    );
}

pub fn paint_headphones(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.16);
    let stroke = Stroke::new((rect.width() * 0.10).max(1.5), color);
    let cx = r.center().x;
    let cy = r.center().y;
    let rad = r.width() * 0.42;

    // Headband arc
    let steps = 16;
    let mut arc_pts = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let angle = std::f32::consts::PI * (1.0 + i as f32 / steps as f32);
        arc_pts.push(pos2(cx + rad * angle.cos(), cy + rad * angle.sin() * 0.95));
    }
    p.add(Shape::line(arc_pts, stroke));

    // Left & right earcups
    let cup_w = (r.width() * 0.22).max(3.0);
    let cup_h = (r.height() * 0.40).max(5.0);
    let left_cup = Rect::from_min_size(pos2(cx - rad - cup_w * 0.35, cy), vec2(cup_w, cup_h));
    let right_cup = Rect::from_min_size(pos2(cx + rad - cup_w * 0.65, cy), vec2(cup_w, cup_h));
    p.rect_filled(left_cup, 2.0, color);
    p.rect_filled(right_cup, 2.0, color);
}

pub fn paint_mic(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.16);
    let cx = r.center().x;
    let stroke = Stroke::new((rect.width() * 0.09).max(1.5), color);

    // Capsule
    let cap_w = (r.width() * 0.34).max(4.0);
    let cap_h = (r.height() * 0.50).max(6.0);
    let cap = Rect::from_center_size(pos2(cx, r.top() + cap_h * 0.55), vec2(cap_w, cap_h));
    p.rect_filled(cap, cap_w * 0.5, color);

    // Cradle arc
    let cradle_r = r.width() * 0.32;
    let cradle_cy = r.top() + cap_h * 0.72;
    let steps = 12;
    let mut arc_pts = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let angle = std::f32::consts::PI * (i as f32 / steps as f32);
        arc_pts.push(pos2(
            cx + cradle_r * angle.cos(),
            cradle_cy + cradle_r * angle.sin() * 0.75,
        ));
    }
    p.add(Shape::line(arc_pts, stroke));

    // Stand stem & base
    let stem_top = cradle_cy + cradle_r * 0.75;
    p.line_segment([pos2(cx, stem_top), pos2(cx, r.bottom())], stroke);
    p.line_segment(
        [
            pos2(cx - r.width() * 0.25, r.bottom()),
            pos2(cx + r.width() * 0.25, r.bottom()),
        ],
        stroke,
    );
}

pub fn paint_category_icon(p: &Painter, rect: Rect, icon: CategoryIcon, color: Color32) {
    match icon {
        CategoryIcon::Home => paint_home(p, rect, color),
        CategoryIcon::Folder => paint_folder(p, rect, color),
        CategoryIcon::Music => paint_music(p, rect, color),
        CategoryIcon::Star => paint_star(p, rect, color),
        CategoryIcon::Mic => paint_mic(p, rect, color),
        CategoryIcon::Gamepad => paint_gamepad(p, rect, color),
    }
}

pub fn paint_home(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.12);
    let mid_y = r.top() + r.height() * 0.45;
    // Roof triangle
    let roof = vec![
        pos2(r.center().x, r.top()),
        pos2(r.right(), mid_y),
        pos2(r.left(), mid_y),
    ];
    p.add(Shape::convex_polygon(roof, color, Stroke::NONE));

    // House body
    let body = Rect::from_min_max(
        pos2(r.left() + r.width() * 0.16, mid_y - 1.0),
        pos2(r.right() - r.width() * 0.16, r.bottom()),
    );
    p.rect_filled(body, 0.5, color);

    // Chimney
    let chim = Rect::from_min_max(
        pos2(r.right() - r.width() * 0.30, r.top() + r.height() * 0.10),
        pos2(r.right() - r.width() * 0.18, mid_y - 1.0),
    );
    p.rect_filled(chim, 0.0, color);
}

pub fn paint_folder(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.14);
    let tab = Rect::from_min_size(r.min, vec2(r.width() * 0.44, r.height() * 0.22));
    p.rect_filled(tab, 1.5, color);
    let body = Rect::from_min_max(pos2(r.left(), r.top() + r.height() * 0.18), r.max);
    p.rect_filled(body, 1.5, color);
}

pub fn paint_music(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.18);
    let stroke = Stroke::new((rect.width() * 0.10).max(1.5), color);
    let head_r = r.width() * 0.16;
    p.circle_filled(pos2(r.left() + head_r, r.bottom() - head_r), head_r, color);
    p.circle_filled(pos2(r.right() - head_r, r.bottom() - head_r * 1.2), head_r, color);
    p.line_segment(
        [
            pos2(r.left() + head_r * 1.8, r.bottom() - head_r),
            pos2(r.left() + head_r * 1.8, r.top() + r.height() * 0.15),
        ],
        stroke,
    );
    p.line_segment(
        [
            pos2(r.right() - head_r * 0.2, r.bottom() - head_r * 1.2),
            pos2(r.right() - head_r * 0.2, r.top()),
        ],
        stroke,
    );
    p.line_segment(
        [
            pos2(r.left() + head_r * 1.8, r.top() + r.height() * 0.15),
            pos2(r.right() - head_r * 0.2, r.top()),
        ],
        Stroke::new((rect.width() * 0.14).max(2.0), color),
    );
}

pub fn paint_star(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.14);
    let c = r.center();
    let outer = r.width() * 0.48;
    let inner = outer * 0.42;
    let mut pts = Vec::with_capacity(10);
    for i in 0..10 {
        let angle = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
        let rad = if i % 2 == 0 { outer } else { inner };
        pts.push(pos2(c.x + rad * angle.cos(), c.y + rad * angle.sin()));
    }
    p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
}

pub fn paint_gamepad(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.15);
    let body = Rect::from_center_size(r.center(), vec2(r.width(), r.height() * 0.62));
    p.rect_filled(body, r.height() * 0.25, color);
}

pub fn paint_wrench(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.20);
    let shaft_stroke = Stroke::new((rect.width() * 0.16).max(2.5), color);
    p.line_segment([pos2(r.left() + 2.0, r.bottom() - 2.0), pos2(r.right() - 4.0, r.top() + 4.0)], shaft_stroke);
    p.circle_stroke(
        pos2(r.right() - 4.0, r.top() + 4.0),
        r.width() * 0.24,
        Stroke::new((rect.width() * 0.12).max(2.0), color),
    );
}

pub fn paint_repeat(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.20);
    let stroke = Stroke::new((rect.width() * 0.10).max(1.5), color);
    let top_y = r.top() + r.height() * 0.24;
    let bot_y = r.bottom() - r.height() * 0.24;
    p.line_segment([pos2(r.left(), top_y), pos2(r.right(), top_y)], stroke);
    p.line_segment([pos2(r.right(), bot_y), pos2(r.left(), bot_y)], stroke);
    // Arrowheads
    p.add(Shape::convex_polygon(
        vec![
            pos2(r.right() - 4.0, top_y - 3.5),
            pos2(r.right() + 1.0, top_y),
            pos2(r.right() - 4.0, top_y + 3.5),
        ],
        color,
        Stroke::NONE,
    ));
    p.add(Shape::convex_polygon(
        vec![
            pos2(r.left() + 4.0, bot_y - 3.5),
            pos2(r.left() - 1.0, bot_y),
            pos2(r.left() + 4.0, bot_y + 3.5),
        ],
        color,
        Stroke::NONE,
    ));
}

pub fn paint_auto_advance(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.22);
    let mid_x = r.center().x;
    p.add(Shape::convex_polygon(
        vec![
            pos2(r.left(), r.top()),
            pos2(mid_x, r.center().y),
            pos2(r.left(), r.bottom()),
        ],
        color,
        Stroke::NONE,
    ));
    p.add(Shape::convex_polygon(
        vec![
            pos2(mid_x, r.top()),
            pos2(r.right(), r.center().y),
            pos2(mid_x, r.bottom()),
        ],
        color,
        Stroke::NONE,
    ));
}

pub fn paint_ptt(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.shrink(rect.width() * 0.18);
    let bubble = Rect::from_min_max(r.min, pos2(r.right(), r.bottom() - r.height() * 0.22));
    p.rect_filled(bubble, 2.5, color);
    p.add(Shape::convex_polygon(
        vec![
            pos2(r.left() + r.width() * 0.22, bubble.bottom() - 1.0),
            pos2(r.left() + r.width() * 0.18, r.bottom()),
            pos2(r.left() + r.width() * 0.45, bubble.bottom() - 1.0),
        ],
        color,
        Stroke::NONE,
    ));
}
