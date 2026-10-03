//! `textinput` — 单行文本输入框的**纯逻辑**。
//!
//! 一个虚拟输入框：它自己维护"文本 + 光标 + 选区 + 历史"这几样状态，对外只暴露
//! 编辑动作（打字 / 退格 / 删除 / 移动 / 选区 / 按词跳转）和查询（文本 / 光标 /
//! 选区 / 历史）。
//!
//! 它**不管输入框之外的事**：
//!
//! - 不认识任何引擎、不认识 ECS、不认识 UI（零依赖，只用 `std`）；
//! - 不认识键盘事件（调用方把"按了左键"翻成 [`TextInput::move_left`]）；
//! - 不认识补全、命令、剪贴板、文件（历史是内存里的字符串列表，写盘是调用方的事）；
//! - 不创建任何对象、不打印日志、不读文件。
//!
//! 这样"输入框该有什么行为"就只有一份实现、一份单测，界面层只负责把键位映射过来、
//! 把状态画出来。
//!
//! # 坐标
//!
//! 光标和选区都是**字节偏移**（`usize`），因为按字节切片是字符串处理里最通用的
//! 坐标。所有偏移都保证落在**字符边界**上：中文、emoji 都不会被切开。
//!
//! # 心智模型
//!
//! ```text
//!            anchor            cursor
//!               │                 │
//!   "give swo"  s  s            （选区 = [anchor, cursor) 或反过来）
//!               └──── 选区 ──────┘
//! ```
//!
//! - `anchor = None` → 没有选区，光标就是一个插入点；
//! - `anchor = Some(a) != cursor` → 选区是 `a` 和 `cursor` 之间的区间（谁大谁小都行）；
//! - `anchor = Some(a) == cursor` → 选区是空的（`Shift` 按了但还没动），下一动作要么
//!   把它拉出来、要么收掉；
//! - **任何编辑都会把锚点收掉**（锚点绝不会在文本变短之后还留在旧位置上）。
//!
//! # 历史
//!
//! 历史和终端一致，**不循环**：`↑` 从空输入进最近一条，按到最老一条就停住；`↓`
//! 往新的一条走，在最新一条上再往下就回到空输入。任何编辑都会退出历史模式。
//!
//! # 例子
//!
//! ```
//! use textinput::TextInput;
//!
//! let mut input = TextInput::new();
//! input.insert("give sword");
//! input.move_word_left(false);          // 光标跳到 "sword" 开头
//! assert_eq!(input.text(), "give sword");
//! assert_eq!(input.cursor(), 5);
//!
//! input.move_word_left(true);           // Shift：从锚点选到 "sword" 末尾
//! assert_eq!(input.selection(), Some((0, 5)));
//! input.insert("cash ");                // 打字替换选区
//! assert_eq!(input.text(), "cash sword");
//! ```

/// 内存里保留的最大历史条数；超了丢最老的。
pub const DEFAULT_HISTORY_LIMIT: usize = 100;

/// 一个单行文本输入框的全部状态。
///
/// `Clone` / `Debug` 是为了调用方方便（可以整个塞进自己的状态里）；它是普通数据，
/// 没有任何隐藏的全局状态。
#[derive(Clone, Debug, Default)]
pub struct TextInput {
    /// 输入串（单行，不含换行 / 制表等控制字符）。
    text: String,
    /// 光标（字节偏移，永远落在字符边界上）。
    cursor: usize,
    /// 选区锚点（字节偏移）。`None` = 没有选区。
    anchor: Option<usize>,
    /// 历史条目，最近的在最后。
    history: Vec<String>,
    /// 正在翻的历史下标（`None` = 没在翻）。
    history_cursor: Option<usize>,
    /// 历史改过、还没被调用方写走。
    history_dirty: bool,
    /// 每变一次 +1（光标闪烁之类的"输入变了"通知）。
    revision: u64,
}

impl TextInput {
    /// 一个空的输入框。
    pub fn new() -> Self {
        Self::default()
    }

    // --- 查询 -----------------------------------------------------------------

    /// 当前文本。
    pub fn text(&self) -> &str {
        &self.text
    }

    /// 当前文本是空的吗。
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// 光标（字节偏移）。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// 当前状态的第几次变化（每次编辑 / 移动 / 翻历史都会 +1）。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// 当前选区 `(start, end)`（`start < end`，字节偏移）；没有选区时 `None`。
    ///
    /// 返回值一定落在文本范围内、并且落在字符边界上 —— 调用方可以直接拿去切片。
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        let len = self.text.len();
        let start = floor_boundary(&self.text, anchor.min(self.cursor).min(len));
        let end = floor_boundary(&self.text, anchor.max(self.cursor).min(len));
        (start < end).then_some((start, end))
    }

    // --- 编辑 -----------------------------------------------------------------

    /// 在光标处插入一段文本；有选区时先替换掉选区（和普通输入框一致）。
    ///
    /// 控制字符（换行 / 制表……）会被丢掉：输入框是单行的，粘贴多行文本也不该把它
    /// 撑坏。
    pub fn insert(&mut self, text: &str) {
        let filtered: String = text
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        if filtered.is_empty() {
            return;
        }
        self.delete_selection();
        // 锚点可能只是"塌"在光标上（按过 Shift 没动）：插入后它会在左边变成一段
        // 莫名其妙的选区，所以插完统一收掉。
        self.anchor = None;
        self.cursor = self.cursor.min(self.text.len());
        self.text.insert_str(self.cursor, &filtered);
        self.cursor += filtered.len();
        self.after_edit();
    }

    /// 删掉光标前的一个字符；有选区就先删选区。
    pub fn backspace(&mut self) {
        if self.delete_selection() {
            self.after_edit();
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let previous = previous_boundary(&self.text, self.cursor);
        self.text.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.after_edit();
    }

    /// 删掉光标后的一个字符；有选区就先删选区。
    pub fn delete(&mut self) {
        if self.delete_selection() {
            self.after_edit();
            return;
        }
        if self.cursor >= self.text.len() {
            return;
        }
        let next = next_boundary(&self.text, self.cursor);
        self.text.replace_range(self.cursor..next, "");
        self.after_edit();
    }

    /// 整段替换（补全接受一条候选时用）：光标钳进新文本，选区收掉。
    pub fn replace(&mut self, text: impl Into<String>, cursor: usize) {
        self.text = text.into();
        self.cursor = floor_boundary(&self.text, cursor.min(self.text.len()));
        self.anchor = None;
        self.after_edit();
    }

    /// 清空（打开面板、执行完一条命令之后）。历史列表本身留着。
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.anchor = None;
        self.after_edit();
    }

    /// 全选。空输入是 no-op。
    pub fn select_all(&mut self) {
        if self.text.is_empty() {
            return;
        }
        self.anchor = Some(0);
        self.cursor = self.text.len();
        self.touch();
    }

    // --- 移动 -----------------------------------------------------------------

    /// 光标左移一个字符（到头就停住）。`extend` = `Shift` 按住：从锚点拉选区。
    pub fn move_left(&mut self, extend: bool) {
        self.set_cursor(previous_boundary(&self.text, self.cursor), extend);
    }

    /// 光标右移一个字符（到头就停住）。
    pub fn move_right(&mut self, extend: bool) {
        self.set_cursor(next_boundary(&self.text, self.cursor), extend);
    }

    /// 光标左移一个**单词**（`Option/Ctrl + ←`）：先跳过光标左边的空白，再跳过一整段
    /// 非空白；到头就停住。
    pub fn move_word_left(&mut self, extend: bool) {
        let mut cursor = self.cursor.min(self.text.len());
        while cursor > 0 {
            let previous = previous_boundary(&self.text, cursor);
            if !self.is_whitespace(previous, cursor) {
                break;
            }
            cursor = previous;
        }
        while cursor > 0 {
            let previous = previous_boundary(&self.text, cursor);
            if self.is_whitespace(previous, cursor) {
                break;
            }
            cursor = previous;
        }
        self.set_cursor(cursor, extend);
    }

    /// 光标右移一个**单词**（`Option/Ctrl + →`）：先跳过后面的空白，再跳过一整段
    /// 非空白；到头就停住。
    pub fn move_word_right(&mut self, extend: bool) {
        let len = self.text.len();
        let mut cursor = self.cursor.min(len);
        while cursor < len {
            let next = next_boundary(&self.text, cursor);
            if !self.is_whitespace(cursor, next) {
                break;
            }
            cursor = next;
        }
        while cursor < len {
            let next = next_boundary(&self.text, cursor);
            if self.is_whitespace(cursor, next) {
                break;
            }
            cursor = next;
        }
        self.set_cursor(cursor, extend);
    }

    /// 光标到开头（`Home`）。
    pub fn move_to_start(&mut self, extend: bool) {
        self.set_cursor(0, extend);
    }

    /// 光标到末尾（`End`）。
    pub fn move_to_end(&mut self, extend: bool) {
        self.set_cursor(self.text.len(), extend);
    }

    // --- 历史 -----------------------------------------------------------------

    /// 全部历史条目（最近的在最后）。
    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// 正在翻历史吗。
    pub fn history_browsing(&self) -> bool {
        self.history_cursor.is_some()
    }

    /// 历史改过、还没被写走吗。
    pub fn history_dirty(&self) -> bool {
        self.history_dirty
    }

    /// 取走"历史改过"的标记（写盘方读一次就够了）。
    pub fn take_history_dirty(&mut self) -> bool {
        std::mem::take(&mut self.history_dirty)
    }

    /// 用从文件读回来的内容替换历史（不置脏标记）。
    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.history_cursor = None;
        self.history_dirty = false;
    }

    /// 记一条刚执行过的输入：连续重复的不再记，超上限丢最老的，并打上脏标记。
    pub fn remember(&mut self, entry: &str) {
        let entry = entry.trim();
        if entry.is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) == Some(entry) {
            return;
        }
        self.history.push(entry.to_string());
        if self.history.len() > DEFAULT_HISTORY_LIMIT {
            let excess = self.history.len() - DEFAULT_HISTORY_LIMIT;
            self.history.drain(..excess);
        }
        self.history_dirty = true;
    }

    /// `↑`：往更老的一条走。不在历史里就从最近一条开始；到最老一条就**停住**（不循环）。
    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_cursor {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.recall(index);
    }

    /// `↓`：往更新的一条走。在最新一条上再往下就回到**空输入**；本来就没在翻历史
    /// 则是 no-op。
    pub fn history_next(&mut self) {
        match self.history_cursor {
            Some(index) if index + 1 < self.history.len() => self.recall(index + 1),
            Some(_) => {
                self.text.clear();
                self.cursor = 0;
                self.anchor = None;
                self.history_cursor = None;
                self.touch();
            }
            None => {}
        }
    }

    // --- 内部 -----------------------------------------------------------------

    /// 把光标移到 `offset`（自动钳进文本、落到字符边界）。
    ///
    /// `extend` = `Shift`：第一次扩展时把锚点钉在原来的光标处；否则收掉选区。
    /// 移动光标**不算**编辑，所以不会退出历史模式（终端里也是这样）。
    fn set_cursor(&mut self, offset: usize, extend: bool) {
        let offset = floor_boundary(&self.text, offset.min(self.text.len()));
        let anchor = if extend {
            // 第一次扩展时把锚点钉在原来的光标处，之后保持不动。
            Some(self.anchor.unwrap_or(self.cursor))
        } else {
            None
        };
        let changed = offset != self.cursor || anchor != self.anchor;
        self.cursor = offset;
        self.anchor = anchor;
        if changed {
            self.touch();
        }
    }

    /// 删掉选区（如果有），把光标留在删除位置，返回是否真的删了东西。
    fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.anchor = None;
        true
    }

    /// 编辑之后的收尾：锚点收掉、退出历史模式、通知"变了"。
    fn after_edit(&mut self) {
        self.anchor = None;
        self.leave_history();
        self.touch();
    }

    /// 退出历史模式（历史列表留着）。
    fn leave_history(&mut self) {
        self.history_cursor = None;
    }

    /// 翻到历史里的第 `index` 条：整行替换、光标到末尾、选区收掉。
    fn recall(&mut self, index: usize) {
        self.text = self.history[index].clone();
        self.cursor = self.text.len();
        self.anchor = None;
        self.history_cursor = Some(index);
        self.touch();
    }

    /// `text[start..end]` 全是空白吗（用来找单词边界）。
    fn is_whitespace(&self, start: usize, end: usize) -> bool {
        self.text[start..end].chars().all(char::is_whitespace)
    }

    /// 记一次状态变化（光标闪烁靠它重新计时）。
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

/// `cursor` 左边那个字符的起始字节（已经在 0 就是 0）。
fn previous_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

/// `cursor` 右边那个字符的结束字节（已经在末尾就是 `text.len()`）。
fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .char_indices()
        .nth(1)
        .map(|(index, _)| cursor + index)
        .unwrap_or(text.len())
}

/// 把字节下标降到最近的**字符边界**（并且不超过字符串长度）。
fn floor_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 打字 / 退格 / 删除都发生在光标处。
    #[test]
    fn editing_happens_at_the_cursor() {
        let mut input = TextInput::new();
        input.insert("gve");
        input.move_left(false);
        input.move_left(false);
        input.insert("i");
        assert_eq!(input.text(), "give");
        assert_eq!(input.cursor(), 2);

        input.backspace();
        assert_eq!(input.text(), "gve");
        assert_eq!(input.cursor(), 1);

        input.move_left(false);
        input.delete();
        assert_eq!(input.text(), "ve");
    }

    /// 回归：一直退格要能把文本删空，**包括第一个字符**。
    #[test]
    fn backspace_deletes_down_to_the_first_character() {
        let mut input = TextInput::new();
        input.insert("abc");
        for expected in ["ab", "a", ""] {
            input.backspace();
            assert_eq!(input.text(), expected);
        }
        // 空了之后退格是 no-op。
        input.backspace();
        assert_eq!(input.text(), "");
        assert_eq!(input.cursor(), 0);
    }

    /// 中文整个字符地走、整个字符地删，不会切进字符中间。
    #[test]
    fn the_cursor_walks_whole_characters() {
        let mut input = TextInput::new();
        input.insert("玩家");
        assert_eq!(input.cursor(), "玩家".len());
        input.move_left(false);
        assert_eq!(input.cursor(), "玩".len());
        input.move_right(false);
        assert_eq!(input.cursor(), "玩家".len());
        input.move_right(false);
        assert_eq!(input.cursor(), "玩家".len(), "到头就停住");
        input.backspace();
        assert_eq!(input.text(), "玩");
    }

    /// 回归：**纯移动**绝不能改文本（以前界面会因此出现 `a|ab` 这种重影）。
    #[test]
    fn moving_the_cursor_never_changes_the_text() {
        let mut input = TextInput::new();
        input.insert("ab");
        for _ in 0..4 {
            input.move_left(false);
            assert_eq!(input.text(), "ab");
        }
        assert_eq!(input.cursor(), 0);
        for _ in 0..4 {
            input.move_right(false);
            assert_eq!(input.text(), "ab");
        }
        assert_eq!(input.cursor(), 2);

        input.move_to_start(false);
        assert_eq!(input.text(), "ab");
        input.move_to_end(false);
        assert_eq!(input.text(), "ab");
    }

    /// 单词跳转：先吃空白再吃一整段非空白，到头停住，而且不改文本。
    #[test]
    fn word_jumps_skip_spaces_and_whole_words() {
        let mut input = TextInput::new();
        input.insert("give p1 sword ");
        let end = input.text().len();

        input.move_word_left(false);
        assert_eq!(input.cursor(), "give p1 ".len(), "跳到 sword 开头");
        input.move_word_left(false);
        assert_eq!(input.cursor(), "give ".len(), "跳到 p1 开头");
        input.move_word_left(false);
        assert_eq!(input.cursor(), 0);
        input.move_word_left(false);
        assert_eq!(input.cursor(), 0, "到头就停住");
        assert_eq!(input.text(), "give p1 sword ");

        input.move_word_right(false);
        assert_eq!(input.cursor(), "give".len(), "跳到 p1 末尾");
        input.move_word_right(false);
        assert_eq!(input.cursor(), "give p1".len());
        input.move_word_right(false);
        assert_eq!(input.cursor(), "give p1 sword".len(), "停在 sword 末尾");
        input.move_word_right(false);
        assert_eq!(input.cursor(), end, "再往右吃掉末尾的空格");
        input.move_word_right(false);
        assert_eq!(input.cursor(), end);
        assert_eq!(input.text(), "give p1 sword ");
    }

    /// `Shift` 拉选区；普通移动收起选区；越过锚点可以从另一边重新长出来。
    #[test]
    fn shift_extends_the_selection_and_plain_moves_collapse_it() {
        let mut input = TextInput::new();
        input.insert("abcd");
        input.move_to_start(false);
        input.move_right(false);
        input.move_right(false);
        input.move_right(true);
        assert_eq!(input.selection(), Some((2, 3)));

        // 普通移动：收起选区，文本不动。
        input.move_left(false);
        assert_eq!(input.selection(), None);
        assert_eq!(input.cursor(), 2);
        assert_eq!(input.text(), "abcd");

        // Shift 越过锚点：从另一边重新选出来。
        input.move_right(true);
        input.move_left(true);
        assert_eq!(input.selection(), None);
        input.move_left(true);
        assert_eq!(input.selection(), Some((1, 2)));
    }

    /// 回归：`Shift` 把锚点"塌"在光标上之后，编辑不能让锚点留在文本外面
    /// （以前界面渲染时会 `&text[1..2]` 越界 panic）。
    #[test]
    fn a_collapsed_anchor_cannot_outrun_the_text() {
        let mut input = TextInput::new();
        input.insert("ab");
        // Shift+→ 在末尾：锚点和光标都停在 2，选区是空的。
        input.move_right(true);
        assert_eq!(input.cursor(), 2);
        assert_eq!(input.selection(), None);

        input.backspace();
        assert_eq!(input.text(), "a");
        assert_eq!(input.cursor(), 1);
        assert_eq!(input.selection(), None);

        // 打字也不能凭空拉出一段选区。
        input.insert("b");
        input.move_right(true);
        input.insert("c");
        assert_eq!(input.text(), "abc");
        assert_eq!(input.selection(), None);

        // 就算锚点真的越界了（防御），`selection` 也只钳到文本范围内。
        input.anchor = Some(99);
        let selection = input.selection();
        assert!(selection.is_none_or(|(start, end)| start < end && end <= input.text().len()));
    }

    /// 选区上的编辑：打字 / 退格 / 删除都替换掉选中的那段。
    #[test]
    fn selection_edits_replace_the_selected_text() {
        let mut input = TextInput::new();
        input.insert("give sword");
        input.move_left(true);
        input.move_left(true);
        assert_eq!(input.selection(), Some((8, 10)), "选中结尾的 rd");

        input.insert("X");
        assert_eq!(input.text(), "give swoX");
        assert_eq!(input.cursor(), input.text().len());
        assert_eq!(input.selection(), None);

        input.move_left(true);
        assert_eq!(input.selection(), Some((8, 9)));
        input.backspace();
        assert_eq!(input.text(), "give swo");
        assert_eq!(input.selection(), None);

        input.move_left(true);
        input.move_left(true);
        assert_eq!(input.selection(), Some((6, 8)));
        input.delete();
        assert_eq!(input.text(), "give s");
        assert_eq!(input.selection(), None);
    }

    /// 全选；`clear` 会收掉选区。
    #[test]
    fn select_all_and_clear() {
        let mut input = TextInput::new();
        input.select_all();
        assert_eq!(input.selection(), None, "空输入没什么可选");

        input.insert("give sword");
        input.select_all();
        assert_eq!(input.selection(), Some((0, input.text().len())));
        input.clear();
        assert_eq!(input.selection(), None);
        assert_eq!(input.text(), "");
        assert_eq!(input.cursor(), 0);
    }

    /// 多行 / 控制字符被丢掉：粘贴进来也不会把单行输入框撑坏。
    #[test]
    fn control_characters_are_dropped() {
        let mut input = TextInput::new();
        input.insert("a\nb\tc\rd");
        assert_eq!(input.text(), "abcd");
    }

    /// 历史：不在最老一条循环，最新一条再往下是空输入，空输入时 `↓` 是 no-op。
    #[test]
    fn history_walks_without_wrapping() {
        let mut input = TextInput::new();
        assert!(!input.history_browsing());
        input.history_previous();
        assert_eq!(input.text(), "", "没有历史时 `↑` 是 no-op");

        input.remember("one");
        input.remember("two");
        input.history_previous();
        assert_eq!(input.text(), "two");
        input.history_previous();
        assert_eq!(input.text(), "one");
        input.history_previous();
        assert_eq!(input.text(), "one", "到最老一条就停住");
        assert!(input.history_browsing());

        input.history_next();
        assert_eq!(input.text(), "two");
        input.history_next();
        assert_eq!(input.text(), "", "最新一条再往下是空输入");
        assert!(!input.history_browsing());
        input.history_next();
        assert_eq!(input.text(), "", "空输入时 `↓` 是 no-op");
        input.history_previous();
        assert_eq!(input.text(), "two", "`↑` 又回到最新一条");
    }

    /// 任何编辑都退出历史模式。
    #[test]
    fn editing_leaves_history_mode() {
        let mut input = TextInput::new();
        input.remember("one");
        input.history_previous();
        assert!(input.history_browsing());
        input.insert("x");
        assert!(!input.history_browsing());

        input.history_previous();
        input.backspace();
        assert!(!input.history_browsing());

        input.history_previous();
        input.move_left(false);
        input.delete();
        assert!(!input.history_browsing());
    }

    /// 记历史：连续重复的不再记、超上限丢最老的、会打脏标记。
    #[test]
    fn remembering_history_dedupes_caps_and_marks_dirty() {
        let mut input = TextInput::new();
        input.remember("give sword");
        assert_eq!(input.history(), ["give sword"]);
        assert!(input.history_dirty());
        assert!(input.take_history_dirty());
        assert!(!input.history_dirty(), "取走之后就不脏了");

        input.remember("give sword");
        assert_eq!(input.history().len(), 1, "连续重复不记");
        assert!(!input.history_dirty());
        input.remember("   ");
        assert_eq!(input.history().len(), 1, "空串不记");

        for index in 0..DEFAULT_HISTORY_LIMIT + 10 {
            input.remember(&format!("echo {index}"));
        }
        assert_eq!(input.history().len(), DEFAULT_HISTORY_LIMIT);
        assert_eq!(input.history()[0], "echo 10");
    }

    /// `set_history` 替换列表、清掉翻历史状态和脏标记（读文件回来时用）。
    #[test]
    fn set_history_replaces_and_clears_the_dirty_flag() {
        let mut input = TextInput::new();
        input.remember("stale");
        input.history_previous();
        input.set_history(vec!["a".into(), "b".into()]);
        assert_eq!(input.history(), ["a", "b"]);
        assert!(!input.history_browsing());
        assert!(!input.history_dirty());
    }

    /// `replace`（补全接受候选）整段换掉并钳住光标。
    #[test]
    fn replace_swaps_the_whole_line() {
        let mut input = TextInput::new();
        input.insert("give gr");
        input.move_left(true);
        input.replace("give sword ", 11);
        assert_eq!(input.text(), "give sword ");
        assert_eq!(input.cursor(), 11);
        assert_eq!(input.selection(), None);
        assert!(!input.history_browsing());

        // 越界的光标会被钳回文本末尾。
        input.replace("hi", 99);
        assert_eq!(input.cursor(), 2);
    }

    /// `revision` 在每次变化时 +1；真的没变（到头还在按）不重置闪烁。
    #[test]
    fn revision_tracks_changes() {
        let mut input = TextInput::new();
        let revision = input.revision();
        input.insert("a");
        assert_ne!(input.revision(), revision);
        let revision = input.revision();
        input.move_left(false);
        assert_ne!(input.revision(), revision);
        let revision = input.revision();
        input.move_left(false);
        assert_eq!(input.revision(), revision, "已经在开头：没有变化");

        let revision = input.revision();
        input.clear();
        assert_ne!(input.revision(), revision);
    }
}
