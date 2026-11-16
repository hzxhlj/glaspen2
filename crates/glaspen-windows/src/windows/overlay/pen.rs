fn width_r(p: f32, scale: f32) -> f32 {
    // 压力 → 半径。原来的线性式 (0.75 + p*1.75) 跨度只有 3.3 倍, 细档
    // (scale=0.3) 下直径 0.45~1.5px, 写出来像一根粗细不变的细线 —— 与
    // OneNote 那种"轻压出锋、重压明显变粗"的观感差得最远的就是这里。
    // 改成二次曲线: 轻压更细(0.9), 重压更粗(3.5), 跨度约 4 倍, 中段也比
    // 原来粗, 抬笔/行笔不再糊成等宽。
    let p = p.clamp(0.0, 1.0);
    (0.9 + 2.6 * p * p) * scale.max(0.05)
}

/// 由中心线点列(x, y, 半径)构建可变宽度轮廓:
/// 先按 centripetal Catmull-Rom 把采样点细分到 ~1.5px 步长, 再沿法线 ±偏移
/// 半径(见 core::inkgeom)。细分是关键 —— 模型器按 120Hz 时间采样, 笔速快时
/// 相邻点十几像素一段, 直接连折线会把圆弧画成肉眼可见的折角。
/// 轮廓两端自带真半圆笔帽, 所以调用方不必再补端点圆点。
fn build_outline(pts: &[(f32, f32, f32)]) -> Vec<(f32, f32)> {
    glaspen_core::inkgeom::densified_outline(pts, glaspen_core::inkgeom::DENSIFY_STEP)
}

/// 合并脏矩形
fn merge_rect(dirty: &mut Option<RECT>, r: &RECT) {
    match dirty {
        None => *dirty = Some(*r),
        Some(d) => {
            d.left = d.left.min(r.left);
            d.top = d.top.min(r.top);
            d.right = d.right.max(r.right);
            d.bottom = d.bottom.max(r.bottom);
        }
    }
}

/// 轮廓色(黑/白,根据笔迹亮度取对比色,用于描边增强)
fn contrast_color(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let lum = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if lum > 128.0 {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    }
}

/// 用给定点列填充整笔轮廓 + 端点圆帽(带可选描边),返回脏矩形
fn fill_stroke_path(canvas: &mut OverlayCanvas, path: &[(f32, f32, f32)], ol: f32) -> Option<RECT> {
    if path.len() < 2 {
        return None;
    }
    let mut dirty: Option<RECT> = None;

    // 描边层:整笔轮廓放大 ol 后用对比色填充
    if ol > 0.0 {
        let ol_color = contrast_color(canvas.color.0, canvas.color.1, canvas.color.2);
        let saved = canvas.color;
        canvas.color = ol_color;
        let wide: Vec<(f32, f32, f32)> = path.iter().map(|&(x, y, r)| (x, y, r + ol)).collect();
        let outline = build_outline(&wide);
        if outline.len() >= 3 {
            let rect = canvas.fill_outline(&outline);
            merge_rect(&mut dirty, &rect);
        }
        canvas.color = saved;
    }

    // 主体。轮廓两端已带半圆笔帽, 不再补端点圆点 —— 那些圆点(描边开启时还是
    // r+ol, 比笔迹粗一圈)会在起笔/落笔处留下一个明显的"圈", 也就是写字时
    // 那个多余的圈圈特效。
    let outline = build_outline(path);
    if outline.len() >= 3 {
        let rect = canvas.fill_outline(&outline);
        merge_rect(&mut dirty, &rect);
    }
    dirty
}

/// 整笔轮廓重填 + 端点圆帽,返回脏矩形(仅新增段区域)
fn redraw_pen(state: &mut OverlayState, new_pts: &[(f32, f32, f32)]) -> Option<RECT> {
    let ol = if state.draw.outline_enabled { 1.0 } else { 0.0 };

    if state.pen_path.is_empty() && !new_pts.is_empty() {
        let (cx, cy, r) = new_pts[0];
        let _ = state.canvas.fill_dot(cx, cy, r);
    }
    let new_start = state.pen_path.len();
    state.pen_path.extend_from_slice(new_pts);
    let new_end = state.pen_path.len();

    // 整笔轮廓填充(单轮廓,非零环绕)+ 可选描边层
    fill_stroke_path(&mut state.canvas, &state.pen_path, ol);

    // dirty 只保留新增段区域(旧区域内容未变)
    let mut new_dirty: Option<RECT> = None;
    for &(px, py, r) in &state.pen_path[new_start.saturating_sub(1)..new_end] {
        let rr = r.ceil() as i32 + 1 + ol as i32;
        let rect = RECT {
            left: (px as i32 - rr).max(0),
            top: (py as i32 - rr).max(0),
            right: (px as i32 + rr + 1).min(state.canvas.w),
            bottom: (py as i32 + rr + 1).min(state.canvas.h),
        };
        merge_rect(&mut new_dirty, &rect);
    }
    new_dirty
}

/// 处理一个采样点:喂给 ink-stroke-modeler,输出平滑点列后轮廓填充。
/// x/y 为画布坐标(无限画布下 = 视图坐标经镜头逆变换);笔迹以画布坐标
/// 入库,绘制时再经镜头变换回视图。
fn handle_point(state: &mut OverlayState, x: f32, y: f32, p: f32, down: bool) -> Option<RECT> {
    // 模型器要求首事件 Down,后续 Move,抬起 Up
    let event_type = if !down {
        ModelerInputEventType::Up
    } else if state.in_stroke {
        ModelerInputEventType::Move
    } else {
        ModelerInputEventType::Down
    };
    let input = ModelerInput {
        event_type,
        pos: (x as f64, y as f64),
        time: state.start_time.elapsed().as_secs_f64(),
        pressure: p as f64,
    };
    let results = match state.stroke_modeler.update(input) {
        Ok(r) => r,
        Err(_) => return None, // Duplicate/负时间等,忽略
    };

    // 落笔时确定笔迹颜色
    if down && !state.in_stroke {
        state.canvas.color = (
            (state.draw.pen_r * 255.0) as u8,
            (state.draw.pen_g * 255.0) as u8,
            (state.draw.pen_b * 255.0) as u8,
        );
    }

    // 记录笔画到 STROKES/DB(用于撤销、导出、XOJ 保存)
    if down && !state.in_stroke {
        glaspen_core::export::glaspen2_begin_stroke(
            state.draw.pen_r,
            state.draw.pen_g,
            state.draw.pen_b,
            state.draw.width_scale,
        );
    }
    if !results.is_empty() {
        let scale = state.draw.width_scale as f32;
        for r in &results {
            // 相对时间必须带上:GIF 回放时间线按它展开,t=0 会让导出永远为空
            glaspen_core::export::glaspen2_add_point_t(
                r.pos.0,
                r.pos.1,
                (width_r(r.pressure as f32, scale) * 2.0) as f64,
                r.time,
            );
        }
    }
    let z = if infinite_on() { cam().2 as f32 } else { 1.0 };
    // 模型器输出是画布坐标;绘制前经镜头变换回视图,线宽同步缩放
    let pts: Vec<(f32, f32, f32)> = results
        .iter()
        .map(|r| {
            let (vx, vy) = view_from_canvas(r.pos.0, r.pos.1);
            (
                vx as f32,
                vy as f32,
                (width_r(r.pressure as f32, state.draw.width_scale as f32) * z).max(0.5),
            )
        })
        .collect();

    if !down {
        // 抬起:补最后一段轮廓 + 终点圆帽,清空并重置模型器
        state.in_stroke = false;
        let dirty = redraw_pen(state, &pts);
        glaspen_core::export::glaspen2_end_stroke();
        state.pen_path.clear();
        let params = modeler_params();
        let _ = state.stroke_modeler.reset_w_params(params);
        state.start_time = Instant::now();
        return dirty;
    }

    state.in_stroke = true;
    redraw_pen(state, &pts)
}

fn modeler_params() -> ModelerParams {
    ModelerParams {
        sampling_min_output_rate: 120.0,
        sampling_end_of_stroke_stopping_distance: 0.01,
        sampling_end_of_stroke_max_iterations: 20,
        sampling_max_outputs_per_call: 200,
        stylus_state_modeler_max_input_samples: 20,
        ..ModelerParams::suggested()
    }
}

/// 从 GetRawInputData 原始 buffer 手工解析 HID 报告
/// 布局: [RAWINPUTHEADER 24B][dwSizeHid 4B][dwCount 4B][报告 dwSizeHid*dwCount 字节]
