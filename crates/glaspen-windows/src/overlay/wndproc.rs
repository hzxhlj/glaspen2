/// 书写光标: 16×16 的小圆点(黑心 + 白环)。单色光标用 AND/XOR 双掩码拼,
/// 不依赖位图资源: AND=1 保留屏幕像素, AND=0 时由 XOR 决定画黑(0)还是白(1)。
/// 白环保证在深色/浅色两种背景上都看得见。首次调用建一次, 之后走缓存。
fn dot_cursor() -> windows::Win32::UI::WindowsAndMessaging::HCURSOR {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows::Win32::UI::WindowsAndMessaging::{CreateCursor, HCURSOR};

    static CACHE: AtomicIsize = AtomicIsize::new(0);
    let cached = CACHE.load(Ordering::SeqCst);
    if cached != 0 {
        return HCURSOR(cached as *mut core::ffi::c_void);
    }

    const W: usize = 16;
    const H: usize = 16;
    const STRIDE: usize = W / 8;
    let mut and_mask = [0xFFu8; STRIDE * H]; // 默认全保留(不遮挡屏幕)
    let mut xor_mask = [0x00u8; STRIDE * H];
    for y in 0..H {
        for x in 0..W {
            let dx = x as f32 + 0.5 - (W as f32) * 0.5;
            let dy = y as f32 + 0.5 - (H as f32) * 0.5;
            let d = (dx * dx + dy * dy).sqrt();
            let byte = y * STRIDE + x / 8;
            let bit = 0x80u8 >> (x % 8);
            if d <= 3.5 {
                and_mask[byte] &= !bit; // 圆心: 清屏后不置位 = 纯黑
            } else if d <= 5.0 {
                and_mask[byte] &= !bit; // 白环
                xor_mask[byte] |= bit;
            }
        }
    }

    let h = unsafe {
        CreateCursor(
            None,
            (W / 2) as i32,
            (H / 2) as i32,
            W as i32,
            H as i32,
            and_mask.as_ptr() as *const core::ffi::c_void,
            xor_mask.as_ptr() as *const core::ffi::c_void,
        )
    }
    .unwrap_or_default();
    CACHE.store(h.0 as isize, Ordering::SeqCst);
    h
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if STATE.load(Ordering::SeqCst).is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    match msg {
        WM_INPUT => {
            // 先分流:鼠标输入(滚轮缩放)与数位笔(HID)走不同处理;
            // 鼠标事件绝不能触碰笔状态/穿透样式(否则会打断下层操作)
            let hraw = HRAWINPUT(lparam.0 as *mut core::ffi::c_void);
            let mut size: u32 = 0;
            let _ = GetRawInputData(
                hraw,
                RID_INPUT,
                None,
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            );
            if size == 0 {
                return LRESULT(0);
            }
            let n = ((size as usize) + 7) / 8;
            let mut buf = vec![0u64; n];
            let written = GetRawInputData(
                hraw,
                RID_INPUT,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            );
            if written == 0 {
                return LRESULT(0);
            }
            let dw_type = u32::from_le_bytes(
                std::slice::from_raw_parts(buf.as_ptr() as *const u8, 4)
                    .try_into()
                    .unwrap(),
            );
            if dw_type == RIM_TYPEMOUSE.0 {
                let state = &mut *STATE.load(Ordering::SeqCst);
                handle_mouse_raw(&buf, state);
                return LRESULT(0);
            }
            if dw_type == RIM_TYPEKEYBOARD.0 {
                // 键盘仅用于录制 GIF 时检测 Ctrl+Alt+R 松开,其余忽略
                let state = &mut *STATE.load(Ordering::SeqCst);
                handle_keyboard_raw(&buf, state);
                return LRESULT(0);
            }
            if dw_type != RIM_TYPEHID.0 {
                return LRESULT(0); // 其余设备不经此窗口
            }
            let state = &mut *STATE.load(Ordering::SeqCst);
            if !state.draw.enabled {
                // 涂鸦关闭:保持穿透,不拦截、不处理
                set_input_blocking(hwnd, false);
                return LRESULT(0);
            }
            let _ = KillTimer(Some(hwnd), TIMER_UNBLOCK);
            let _ = KillTimer(Some(hwnd), TIMER_PEEK);

            // 飘渺模式:笔悬空/落笔 → 显示笔迹(仅首次,避免高频全量重绘)
            if state.draw.ethereal && !state.strokes_visible {
                show_strokes(state);
            }

            if let Some(dirty) = process_raw_hid(&buf) {
                let state = &mut *STATE.load(Ordering::SeqCst);
                state.canvas.present_rect(&dirty);
            }

            // 只有**落笔**才拦截鼠标(清 WS_EX_TRANSPARENT),抬笔 50ms 后恢复。
            // 以前每份 HID 报告都拦截: 笔搁在板上(悬停/在范围内)时鼠标
            // 完全点不了下层应用, 直接破坏"键鼠/笔分离"的核心卖点。
            //
            // 注意 timer 必须**无条件**续期, 不能跟着 in_stroke 走:
            // 抬笔那一份报告的处理顺序是
            //   ① 上面 KillTimer(TIMER_UNBLOCK) 先杀掉上一份报告设的恢复计时器
            //   ② process_raw_hid → handle_point(up) 把 in_stroke 置为 false
            //   ③ 走到这里时 in_stroke 已经是 false → 若此时才决定不设 timer,
            //      就再也没有 WM_TIMER 去调 set_input_blocking(false),
            //      WS_EX_TRANSPARENT 永久丢失 → 全屏覆盖层吃掉所有鼠标事件
            //      (表现为:写下一笔再抬笔,鼠标就再也点不到下层窗口)。
            // 所以: 拦截与否看 in_stroke, 计时器每份报告都重新上。
            if state.in_stroke {
                set_input_blocking(hwnd, true);
                // 书写期间鼠标被本窗口接管, 光标换成小圆点(不挡视线);
                // 抬笔恢复穿透后, 光标归属又交回下层窗口, 无需手动还原。
                let _ = windows::Win32::UI::WindowsAndMessaging::SetCursor(dot_cursor());
            }
            let _ = SetTimer(Some(hwnd), TIMER_UNBLOCK, UNBLOCK_DELAY_MS, None);
            LRESULT(0)
        }
        WM_TIMER => {
            let state = &mut *STATE.load(Ordering::SeqCst);
            match wparam.0 as usize {
                TIMER_UNBLOCK => {
                    let _ = KillTimer(Some(hwnd), TIMER_UNBLOCK);
                    // 笔离开:设 WS_EX_TRANSPARENT 恢复穿透
                    set_input_blocking(hwnd, false);
                    // 飘渺模式:隐藏笔迹
                    if state.draw.ethereal && state.strokes_visible && !state.in_stroke {
                        hide_strokes(state);
                    }
                }
                TIMER_PEEK => {
                    let _ = KillTimer(Some(hwnd), TIMER_PEEK);
                    // 飘渺模式翻页 peek 到期:无笔活动时隐藏
                    if state.draw.ethereal && !state.in_stroke && state.strokes_visible {
                        hide_strokes(state);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            let state = &mut *STATE.load(Ordering::SeqCst);
            match wparam.0 as i32 {
                // Ctrl+Alt+C 新建画布 / Ctrl+Alt+V 开关涂鸦 / Ctrl+Alt+Z 撤销
                1 => clear_screen(state),
                // Ctrl+Alt+E 同 Ctrl+Alt+C: 清屏(新建空白画布)
                17 => clear_screen(state),
                2 => toggle_enabled(state),
                3 => undo_last_stroke(state),
                // Ctrl+Alt+`/1 上一页/下一页(b7cb22d 同改)
                4 => navigate_page(state, false),
                5 => navigate_page(state, true),
                // Ctrl+Alt+G 导出 SVG + GIF 并复制到剪贴板
                6 => export_svg_gif_clipboard(state),
                // Ctrl+Alt+B 模糊背景(磨砂玻璃)
                7 => toggle_frosted(state),
                // Ctrl+Alt+X 切换固定/飘渺画布模式
                9 => handle_command(state, CMD_TOGGLE_ETHEREAL, usize::MAX),
                // Ctrl+Alt+Q 退出
                8 => unsafe {
                    let _ = DestroyWindow(hwnd);
                },
                // 无限画布:Ctrl+Alt+方向键 平移镜头(步长随缩放,越缩小步长越大)
                10 | 11 | 12 | 13 => {
                    if infinite_on() && state.draw.enabled {
                        let (_, _, z) = cam();
                        let step = 80.0 / z;
                        let (dx, dy) = match wparam.0 as i32 {
                            10 => (-step, 0.0), // ←
                            11 => (0.0, step),  // ↑
                            12 => (step, 0.0),  // →
                            _ => (0.0, -step),  // ↓
                        };
                        canvas_pan_by(state, dx, dy);
                    }
                }
                // 无限画布:Ctrl+Alt+PageUp/PageDown 键盘缩放(以视口中心为锚)
                14 | 15 => {
                    if infinite_on() && state.draw.enabled {
                        let factor = if wparam.0 as i32 == 14 {
                            1.15
                        } else {
                            1.0 / 1.15
                        };
                        canvas_zoom_at(
                            state,
                            factor,
                            state.canvas.w as f64 * 0.5,
                            state.canvas.h as f64 * 0.5,
                        );
                    }
                }
                // Ctrl+Alt+R 按住录制手写 GIF(松开由键盘 Raw Input 检测)
                16 => gif_record_start(state),
                // Ctrl+Alt+2 按住:手写草稿通道(集成开关守门,松开由 Raw Input 检测)
                17 => {
                    if state.draw.chat_integration {
                        ink_draft_start(state);
                    }
                }
                // Ctrl+Alt+3 按住:录制手写消息,松开直发(松开由 Raw Input 检测)
                18 => {
                    if state.draw.chat_integration {
                        chat_record_start(state);
                    }
                }
                // Ctrl+Alt+D 打开/重开设置面板(macOS ⌘⌃D 同义)
                19 => crate::show_settings_window(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_RECORD_DONE => {
            // 后台 GIF 编码/剪贴板完成(0=复制失败, 1=成功, 2=没有笔迹)
            match wparam.0 as usize {
                1 => hud_notify("GIF 已复制到剪贴板"),
                2 => hud_notify("没有笔迹或导出失败"),
                _ => hud_notify("GIF 复制失败"),
            }
            LRESULT(0)
        }
        WM_TRAY_COMMAND => {
            let state = &mut *STATE.load(Ordering::SeqCst);
            handle_command(state, wparam.0, lparam.0 as usize);
            LRESULT(0)
        }
        WM_KEYDOWN => DefWindowProcW(hwnd, msg, wparam, lparam),
        WM_DISPLAYCHANGE => {
            let state = &mut *STATE.load(Ordering::SeqCst);
            on_display_change(state);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DESTROY => {
            // 无限画布退出前存镜头(节流持久化可能丢最后一次拖动)
            if infinite_on() {
                persist_camera();
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── 入口 ──

