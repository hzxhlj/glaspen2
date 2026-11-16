//! 笔迹几何: 中心线细分 + 可变宽度轮廓。纯数学, 无平台依赖、无 IO。
//!
//! 为什么需要细分: ink-stroke-modeler 的输出是按时间(120Hz)采样的 —— 笔速
//! 越快相邻采样点间距越大(快速撇捺上十几像素一段), 直接连折线会把圆弧画成
//! 肉眼可见的多边形拐角, 这是笔迹"发硬、不自然"的主因。这里把采样点当作
//! centripetal Catmull-Rom 样条的控制点重新细分到 ~1.5px 步长, 边界点贴住
//! 一条真正光滑的曲线; 线宽(半径)同时用同一条 Hermite 的切线插值, 并限制
//! 不过冲(单调), 避免压感跳变在采样点处留下宽度台阶。
//!
//! 调用方: Windows 覆盖层实时绘制(`glaspen-windows/overlay/pen.rs`)、本
//! crate 静态渲染(`pagerender::render_strokes`)。两边共用同一套几何, 保证
//! "玻璃上看到的"与"导出/缩略图里的"形状一致。

/// 细分目标步长(绘制坐标像素)。1.5px 下 1:1 观看已无法分辨折点。
pub const DENSIFY_STEP: f32 = 1.5;

/// 单笔细分点数上限: 超长涂鸦自动放大步长, 保证轮廓填充开销有界。
pub const DENSIFY_MAX_POINTS: usize = 6000;

/// 半径下限(像素): 避免零宽笔迹整段消失。
pub const MIN_RADIUS: f32 = 0.25;

/// 圆形笔帽的弧线分段数(半圆)。
const CAP_SEGS: usize = 8;

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    (dx * dx + dy * dy).sqrt()
}

/// 三次 Bézier 求值(二维点)。
fn cubic(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32), t: f32) -> (f32, f32) {
    let mt = 1.0 - t;
    let a = mt * mt * mt;
    let b = 3.0 * mt * mt * t;
    let c = 3.0 * mt * t * t;
    let d = t * t * t;
    (
        p0.0 * a + p1.0 * b + p2.0 * c + p3.0 * d,
        p0.1 * a + p1.1 * b + p2.1 * c + p3.1 * d,
    )
}

/// 三次 Bézier 求值(标量, 线宽用)。
fn cubic1(a0: f32, a1: f32, a2: f32, a3: f32, t: f32) -> f32 {
    let mt = 1.0 - t;
    mt * mt * mt * a0 + 3.0 * mt * mt * t * a1 + 3.0 * mt * t * t * a2 + t * t * t * a3
}

/// 把控制值夹到两个端点的区间内: 保证半径单调不过冲(不鼓包)。
fn clamp_unit(v: f32, a: f32, b: f32) -> f32 {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    v.clamp(lo, hi)
}

/// 中心线细分: `(x, y, r)` 点列 → 沿 centripetal Catmull-Rom 曲线重采样的
/// `(x, y, r)` 点列, 相邻点间距约 `step` 像素(曲率大的地方更密)。
///
/// - 首末点原样保留(笔迹起止位置不能漂移)
/// - 少于 2 点原样返回
/// - 半径用同一段 Hermite 的切线插值, 夹在相邻采样半径之间, 不出台阶也不鼓包
pub fn densify(points: &[(f32, f32, f32)], step: f32) -> Vec<(f32, f32, f32)> {
    let n = points.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return points.to_vec();
    }

    let mut poly_len = 0.0f32;
    for w in points.windows(2) {
        poly_len += dist((w[0].0, w[0].1), (w[1].0, w[1].1));
    }
    let mut step = if step.is_finite() && step > 0.05 {
        step
    } else {
        DENSIFY_STEP
    };
    if poly_len > 0.0 {
        // 点数上限: 步长按需放大
        step = step.max(poly_len / DENSIFY_MAX_POINTS as f32);
    }

    let cap = ((poly_len / step) as usize).saturating_add(n).saturating_add(8);
    let mut out: Vec<(f32, f32, f32)> = Vec::with_capacity(cap.min(DENSIFY_MAX_POINTS + 256));
    out.push(points[0]);

    for i in 0..n - 1 {
        let p1 = points[i];
        let p2 = points[i + 1];
        let p0 = if i == 0 { points[0] } else { points[i - 1] };
        let p3 = if i + 2 < n { points[i + 2] } else { points[n - 1] };

        // centripetal 节点参数(alpha = 0.5): 间距开方, 对采样疏密不均更稳
        let d1 = dist((p0.0, p0.1), (p1.0, p1.1)).sqrt().max(1e-3);
        let d2 = dist((p1.0, p1.1), (p2.0, p2.1)).sqrt().max(1e-3);
        let d3 = dist((p2.0, p2.1), (p3.0, p3.1)).sqrt().max(1e-3);
        let t1 = d1;
        let t2 = t1 + d2;
        let t3 = t2 + d3;
        let m = t2 - t1;

        // 端点切线(对节点参数的导数) → Bézier 控制点
        let k1 = m / 3.0 / t2;
        let k2 = m / 3.0 / (t3 - t1);
        let b1 = (
            p1.0 + (p2.0 - p0.0) * k1,
            p1.1 + (p2.1 - p0.1) * k1,
        );
        let b2 = (
            p2.0 - (p3.0 - p1.0) * k2,
            p2.1 - (p3.1 - p1.1) * k2,
        );
        let r1 = clamp_unit(p1.2 + (p2.2 - p0.2) * k1, p1.2, p2.2);
        let r2 = clamp_unit(p2.2 - (p3.2 - p1.2) * k2, p1.2, p2.2);

        // 细分段数按控制多边形长度估(曲率大 → 估长 > 弦长 → 更密)
        let est = dist((p1.0, p1.1), b1) + dist(b1, b2) + dist(b2, (p2.0, p2.1));
        let segs = ((est / step).ceil() as usize).clamp(1, 128);
        for k in 1..=segs {
            let t = k as f32 / segs as f32;
            let (x, y) = cubic((p1.0, p1.1), b1, b2, (p2.0, p2.1), t);
            let r = cubic1(p1.2, r1, r2, p2.2, t).max(MIN_RADIUS);
            out.push((x, y, r));
        }
    }

    out
}

/// 可变宽度轮廓: 中心线 `(x, y, r)` 点列 → 闭合轮廓多边形。
///
/// 两侧沿法线偏移半径, 两端接真正的半圆笔帽(而不是平头), 因此轮廓本身
/// 就是一笔完整墨迹 —— 调用方不必再补端点圆点。
///
/// 法线取自前后点中点差分: 相邻带在共享点处偏移量一致, 侧边无缝。点列是
/// [`densify`] 的输出时, 差分方向随曲线连续变化, 侧边即为光滑曲线。
pub fn outline(points: &[(f32, f32, f32)]) -> Vec<(f32, f32)> {
    let n = points.len();
    if n < 2 {
        return Vec::new();
    }

    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    let mut tangents = Vec::with_capacity(n);
    for i in 0..n {
        let (x, y, r) = points[i];
        let r = r.max(MIN_RADIUS);
        let (dx, dy) = if i == 0 {
            (points[1].0 - x, points[1].1 - y)
        } else if i == n - 1 {
            (x - points[i - 1].0, y - points[i - 1].1)
        } else {
            (
                points[i + 1].0 - points[i - 1].0,
                points[i + 1].1 - points[i - 1].1,
            )
        };
        let l = (dx * dx + dy * dy).sqrt();
        let (ux, uy) = if l > 1e-6 { (dx / l, dy / l) } else { (1.0, 0.0) };
        tangents.push((ux, uy));
        let (nx, ny) = (-uy, ux);
        left.push((x + nx * r, y + ny * r));
        right.push((x - nx * r, y - ny * r));
    }

    let mut out: Vec<(f32, f32)> = Vec::with_capacity(n * 2 + 2 * (CAP_SEGS + 1));
    out.extend(left.iter().copied());

    // 末端笔帽: 左 → 正前 → 右(绕端点中心走半圆)
    let (cx, cy, r) = points[n - 1];
    let (r, ux, uy) = (r.max(MIN_RADIUS), tangents[n - 1].0, tangents[n - 1].1);
    let (nx, ny) = (-uy, ux);
    for k in 1..=CAP_SEGS {
        let a = std::f32::consts::PI * k as f32 / CAP_SEGS as f32;
        let (s, c) = a.sin_cos();
        out.push((cx + nx * r * c + ux * r * s, cy + ny * r * c + uy * r * s));
    }

    // 右侧反向
    for p in right.iter().rev() {
        out.push(*p);
    }

    // 起笔笔帽: 右 → 正后 → 左
    let (cx, cy, r) = points[0];
    let (r, ux, uy) = (r.max(MIN_RADIUS), tangents[0].0, tangents[0].1);
    let (nx, ny) = (-uy, ux);
    for k in 1..=CAP_SEGS {
        let a = std::f32::consts::PI * k as f32 / CAP_SEGS as f32;
        let (s, c) = a.sin_cos();
        out.push((cx - nx * r * c - ux * r * s, cy - ny * r * c - uy * r * s));
    }

    out
}

/// 一步到位: 细分 + 轮廓(实时绘制的常用组合)。
pub fn densified_outline(points: &[(f32, f32, f32)], step: f32) -> Vec<(f32, f32)> {
    outline(&densify(points, step))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dist2(a: (f32, f32), b: (f32, f32)) -> f32 {
        dist(a, b)
    }

    #[test]
    fn densify_keeps_endpoints() {
        let pts = vec![(0.0, 0.0, 1.0), (10.0, 5.0, 2.0), (20.0, 0.0, 1.0)];
        let out = densify(&pts, 1.5);
        let first = out.first().copied().unwrap();
        let last = out.last().copied().unwrap();
        assert!((first.0 - 0.0).abs() < 1e-3 && (first.1 - 0.0).abs() < 1e-3);
        assert!((last.0 - 20.0).abs() < 1e-3 && (last.1 - 0.0).abs() < 1e-3);
    }

    #[test]
    fn densify_two_points_is_linear() {
        let pts = vec![(0.0, 0.0, 1.0), (10.0, 0.0, 1.0)];
        let out = densify(&pts, 1.0);
        assert!(out.len() >= 10, "should be resampled to ~1px steps");
        // 直线段上所有点必须落在 y=0 上(CR 两端切线沿连线)
        for p in &out {
            assert!(p.1.abs() < 1e-2, "stray point off the straight line: {p:?}");
        }
    }

    #[test]
    fn densify_spacing_roughly_matches_step() {
        let mut pts = Vec::new();
        for i in 0..6 {
            let a = i as f32 * 0.4;
            pts.push((a.cos() * 40.0, a.sin() * 40.0, 2.0));
        }
        let out = densify(&pts, 1.5);
        let mut max_gap = 0.0f32;
        for w in out.windows(2) {
            max_gap = max_gap.max(dist2((w[0].0, w[0].1), (w[1].0, w[1].1)));
        }
        assert!(max_gap < 5.0, "gap too large: {max_gap}");
    }

    #[test]
    fn densify_radius_stays_between_neighbours() {
        let pts = vec![
            (0.0, 0.0, 1.0),
            (5.0, 0.0, 4.0),
            (10.0, 0.0, 1.0),
            (15.0, 0.0, 4.0),
        ];
        let out = densify(&pts, 0.5);
        for p in &out {
            assert!(p.2 >= 1.0 - 1e-3 && p.2 <= 4.0 + 1e-3, "radius overshoot: {p:?}");
        }
    }

    #[test]
    fn densify_caps_point_count() {
        // 超长折线: 点数必须被 DENSIFY_MAX_POINTS 约束住
        let mut pts = Vec::new();
        for i in 0..2000 {
            let x = i as f32 * 3.0;
            pts.push((x, (i as f32 * 0.1).sin() * 20.0, 1.5));
        }
        let out = densify(&pts, 0.1);
        assert!(
            out.len() <= DENSIFY_MAX_POINTS + 3000,
            "point count exploded: {}",
            out.len()
        );
    }

    #[test]
    fn outline_is_closed_around_stroke() {
        let pts = vec![(0.0, 0.0, 2.0), (10.0, 0.0, 2.0), (20.0, 10.0, 3.0)];
        let o = outline(&pts);
        assert!(o.len() >= pts.len() * 2 + 2 * CAP_SEGS);
        // 所有轮廓点都在中心线 ±(最大半径 + 0.01) 的带内
        for (x, y) in &o {
            let mut best = f32::MAX;
            for w in pts.windows(2) {
                // 点到线段的距离
                let (x0, y0) = (w[0].0, w[0].1);
                let (dx, dy) = (w[1].0 - x0, w[1].1 - y0);
                let l2 = (dx * dx + dy * dy).max(1e-6);
                let t = (((*x - x0) * dx + (*y - y0) * dy) / l2).clamp(0.0, 1.0);
                let (px, py) = (x0 + dx * t, y0 + dy * t);
                best = best.min(((*x - px).powi(2) + (*y - py).powi(2)).sqrt());
            }
            assert!(best <= 3.0 + 0.05, "outline point too far from centre: {best}");
        }
    }

    #[test]
    fn outline_needs_two_points() {
        assert!(outline(&[(0.0, 0.0, 1.0)]).is_empty());
    }

    #[test]
    fn densified_outline_smooth_on_sharp_turn() {
        // 直角拐弯: 细分后轮廓点数应显著多于原始点数, 且不产生 NaN
        let pts = vec![(0.0, 0.0, 2.0), (30.0, 0.0, 2.0), (30.0, 30.0, 2.0)];
        let o = densified_outline(&pts, 1.5);
        assert!(o.len() > 50);
        for (x, y) in &o {
            assert!(x.is_finite() && y.is_finite());
        }
    }
}
