use crate::scrcpy::client::scrcpy_control::AndroidKeyEventAction;

/// UHID 键盘报告长度（标准 8 字节）。
///
/// 结构：
/// - [0] 修饰键位图；
/// - [1] 保留位；
/// - [2..8] 6个普通按键 usage。
pub const UHID_KEYBOARD_REPORT_LEN: usize = 8;

/// UHID 键盘状态。
///
/// 说明：
/// - `modifiers` 保存 Ctrl/Shift/Alt/Meta 的位图；
/// - `keys` 保存当前按下的普通键 usage（最多6个）。
#[derive(Debug, Clone)]
pub struct UhidKeyboardState {
    /// 修饰键位图。
    modifiers: u8,
    /// 普通按键槽位（最多6键同时按下）。
    keys: [u8; 6],
}

impl Default for UhidKeyboardState {
    fn default() -> Self {
        Self {
            modifiers: 0,
            keys: [0; 6],
        }
    }
}

impl UhidKeyboardState {
    /// 创建空状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 根据 Android 按键事件更新状态。
    ///
    /// 处理规则：
    /// 1. 若是修饰键（Ctrl/Shift/Alt/Meta），更新 `modifiers` 位图；
    /// 2. 否则尝试把 Android keycode 映射为 HID usage；
    /// 3. Down 时入槽，Up 时出槽。
    pub fn update_key(&mut self, action: AndroidKeyEventAction, keycode: u32) {
        if let Some(mask) = android_modifier_keycode_to_mask(keycode) {
            match action {
                AndroidKeyEventAction::Down => self.modifiers |= mask,
                AndroidKeyEventAction::Up => self.modifiers &= !mask,
            }
            return;
        }

        let Some(usage) = android_keycode_to_hid_usage(keycode) else {
            return;
        };

        match action {
            AndroidKeyEventAction::Down => self.press_usage(usage),
            AndroidKeyEventAction::Up => self.release_usage(usage),
        }
    }

    /// 清空全部按键状态。
    ///
    /// 常用于：
    /// - 会话销毁前；
    /// - UHID 设备重建后首次同步；
    /// - 异常恢复时的兜底释放。
    pub fn release_all(&mut self) {
        self.modifiers = 0;
        self.keys = [0; 6];
    }

    /// 构建 8 字节 HID 输入报告。
    pub fn to_report(&self) -> [u8; UHID_KEYBOARD_REPORT_LEN] {
        let mut report = [0u8; UHID_KEYBOARD_REPORT_LEN];
        report[0] = self.modifiers;
        report[2..8].copy_from_slice(&self.keys);
        report
    }

    /// 普通键按下入槽。
    ///
    /// 策略：
    /// - 重复按下同一键时忽略；
    /// - 有空槽则直接写入；
    /// - 无空槽时淘汰最旧键，避免状态卡死。
    fn press_usage(&mut self, usage: u8) {
        if self.keys.contains(&usage) {
            return;
        }

        if let Some(slot) = self.keys.iter_mut().find(|k| **k == 0) {
            *slot = usage;
            return;
        }

        // 超过 6 键时采用最旧键淘汰策略，避免状态卡死。
        self.keys.rotate_left(1);
        self.keys[5] = usage;
    }

    /// 普通键释放出槽。
    ///
    /// 说明：
    /// - 找到目标 usage 后置零；
    /// - 通过“排序 + 旋转”把 0 移到末尾，保持数组紧凑。
    fn release_usage(&mut self, usage: u8) {
        if let Some(idx) = self.keys.iter().position(|k| *k == usage) {
            self.keys[idx] = 0;
            self.keys.sort_unstable();
            let zeros = self.keys.iter().take_while(|k| **k == 0).count();
            self.keys.rotate_left(zeros);
        }
    }
}

/// Android 修饰键 keycode -> HID modifier 位图。
fn android_modifier_keycode_to_mask(keycode: u32) -> Option<u8> {
    match keycode {
        59 => Some(0x02), // KEYCODE_SHIFT_LEFT
        60 => Some(0x20), // KEYCODE_SHIFT_RIGHT
        57 => Some(0x01), // KEYCODE_ALT_LEFT
        58 => Some(0x40), // KEYCODE_ALT_RIGHT
        113 => Some(0x04), // KEYCODE_CTRL_LEFT
        114 => Some(0x10), // KEYCODE_CTRL_RIGHT
        117 => Some(0x08), // KEYCODE_META_LEFT
        118 => Some(0x80), // KEYCODE_META_RIGHT
        _ => None,
    }
}

/// Android keycode -> HID keyboard usage（常用键覆盖）。
///
/// 注意：
/// - 只映射当前项目常用键；
/// - 未覆盖键会被忽略，不会抛错。
fn android_keycode_to_hid_usage(keycode: u32) -> Option<u8> {
    match keycode {
        // A-Z
        29..=54 => Some((keycode - 29 + 4) as u8),
        // 0-9（Android: 7..16）
        7 => Some(39),
        8..=16 => Some((keycode - 8 + 30) as u8),

        // 常见控制键
        66 => Some(40), // ENTER
        67 => Some(42), // DEL/BACKSPACE
        61 => Some(43), // TAB
        62 => Some(44), // SPACE
        111 => Some(41), // ESCAPE

        // 方向键
        19 => Some(82),
        20 => Some(81),
        21 => Some(80),
        22 => Some(79),

        // 功能键
        131..=142 => Some((keycode - 131 + 58) as u8), // F1-F12

        // 标点常用键
        69 => Some(45), // -
        70 => Some(46), // =
        71 => Some(47), // [
        72 => Some(48), // ]
        73 => Some(49), // \
        74 => Some(51), // ;
        75 => Some(52), // '
        55 => Some(54), // ,
        56 => Some(55), // .
        76 => Some(56), // /
        68 => Some(53), // `

        _ => None,
    }
}
