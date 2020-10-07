use anyhow::Result;
use druid::{
    piet::{PietText, Text, TextAttribute, TextLayoutBuilder},
    Color, ExtEventSink, Target, UpdateCtx,
};
use druid::{Env, PaintCtx};
use language::{new_highlight_config, new_parser, LapceLanguage};
use std::{
    borrow::Cow,
    io::{self, Read, Write},
    sync::Arc,
    thread,
};
use std::{collections::HashMap, fs::File};
use tree_sitter::{Parser, Tree};
use tree_sitter_highlight::{
    Highlight, HighlightConfiguration, HighlightEvent, Highlighter,
};
use xi_core_lib::{
    line_offset::{LineOffset, LogicalLines},
    selection::InsertDrift,
};
use xi_rope::{
    interval::IntervalBounds, rope::Rope, Cursor, DeltaBuilder, Interval,
    LinesMetric, RopeDelta, RopeInfo, Transformer,
};

use crate::{
    command::LapceUICommand,
    command::LAPCE_UI_COMMAND,
    editor::EditorOperator,
    editor::HighlightTextLayout,
    language,
    movement::{ColPosition, Movement, SelRegion, Selection},
    state::LapceState,
    state::Mode,
    theme::LapceTheme,
};

#[derive(Debug, Clone)]
pub struct InvalLines {
    pub start_line: usize,
    pub inval_count: usize,
    pub new_count: usize,
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub struct BufferId(pub usize);

#[derive(Clone)]
pub struct BufferUIState {
    pub id: BufferId,
    pub text_layouts: Vec<Option<HighlightTextLayout>>,
}

#[derive(Clone)]
pub struct Buffer {
    id: BufferId,
    rope: Rope,
    tree: Tree,
    highlight_config: Arc<HighlightConfiguration>,
    highlight_names: Vec<String>,
    pub highlights: Vec<(usize, usize, Highlight)>,
    pub line_highlights: HashMap<usize, Vec<(usize, usize, String)>>,
    pub highlight_version: String,
    event_sink: ExtEventSink,
    pub max_len_line: usize,
    pub max_len: usize,
    pub text_layouts: Vec<Option<HighlightTextLayout>>,
    // pub inval_lines: Option<InvalLines>,
}

impl Buffer {
    pub fn new(
        buffer_id: BufferId,
        path: &str,
        event_sink: ExtEventSink,
    ) -> Buffer {
        let rope = if let Ok(rope) = load_file(path) {
            rope
        } else {
            Rope::from("")
        };
        let mut parser = new_parser(LapceLanguage::Rust);
        let tree = parser.parse(&rope.to_string(), None).unwrap();

        let (highlight_config, highlight_names) =
            new_highlight_config(LapceLanguage::Rust);

        let mut buffer = Buffer {
            id: buffer_id,
            rope,
            tree,
            highlight_config: Arc::new(highlight_config),
            highlight_names,
            highlights: Vec::new(),
            line_highlights: HashMap::new(),
            highlight_version: "".to_string(),
            max_len_line: 0,
            max_len: 0,
            text_layouts: Vec::new(),
            event_sink,
        };
        buffer.text_layouts = vec![None; buffer.num_lines()];
        buffer.update_max_line_len();
        buffer.update_highlights(None);
        buffer
    }

    pub fn len(&self) -> usize {
        self.rope.len()
    }

    pub fn highlights_apply_delta(
        &mut self,
        delta: &RopeDelta,
    ) -> Vec<(usize, usize, Highlight)> {
        let mut transformer = Transformer::new(delta);
        self.highlights
            .iter()
            .map(|h| {
                (
                    transformer.transform(h.0, true),
                    transformer.transform(h.1, true),
                    h.2.clone(),
                )
            })
            .collect()
    }

    pub fn update_highlights(&mut self, inval_lines: Option<InvalLines>) {
        let version = uuid::Uuid::new_v4().to_string();
        self.line_highlights = HashMap::new();
        self.highlight_version = version.clone();

        let highlight_config = self.highlight_config.clone();
        let rope_str = self.slice_to_cow(..self.len()).to_string();
        let buffer_id = self.id.clone();
        let event_sink = self.event_sink.clone();
        thread::spawn(move || {
            let mut highlights: Vec<(usize, usize, Highlight)> = Vec::new();
            let mut highlighter = Highlighter::new();
            let mut current_hl: Option<Highlight> = None;
            for hightlight in highlighter
                .highlight(
                    &highlight_config,
                    &rope_str.as_bytes(),
                    None,
                    |_| None,
                )
                .unwrap()
            {
                if let Ok(highlight) = hightlight {
                    match highlight {
                        HighlightEvent::Source { start, end } => {
                            if let Some(hl) = current_hl {
                                highlights.push((start, end, hl.clone()));
                            }
                        }
                        HighlightEvent::HighlightStart(hl) => {
                            current_hl = Some(hl);
                        }
                        HighlightEvent::HighlightEnd => current_hl = None,
                    }
                }
            }

            event_sink.submit_command(
                LAPCE_UI_COMMAND,
                LapceUICommand::UpdateHighlights(
                    buffer_id, version, highlights,
                ),
                Target::Global,
            );
        });
    }

    pub fn get_line_highligh(
        &mut self,
        line: usize,
    ) -> &Vec<(usize, usize, String)> {
        if self.line_highlights.get(&line).is_none() {
            let mut line_highlight = Vec::new();
            let start_offset = self.offset_of_line(line);
            let end_offset = self.offset_of_line(line + 1) - 1;
            for (start, end, hl) in &self.highlights {
                if *start > end_offset {
                    break;
                }
                if *start >= start_offset && *start <= end_offset {
                    line_highlight.push((
                        start - start_offset,
                        end - start_offset,
                        self.highlight_names[hl.0].to_string(),
                    ));
                }
            }
            self.line_highlights.insert(line, line_highlight);
        }
        self.line_highlights.get(&line).unwrap()
    }

    pub fn correct_offset(&self, selection: &Selection) -> Selection {
        let mut result = Selection::new();
        for region in selection.regions() {
            let (line, col) = self.offset_to_line_col(region.start());
            let max_col = self.line_max_col(line, false);
            let (start, col) = if col > max_col {
                (self.offset_of_line(line) + max_col, max_col)
            } else {
                (region.start(), col)
            };

            let (line, col) = self.offset_to_line_col(region.start());
            let max_col = self.line_max_col(line, false);
            let end = if col > max_col {
                self.offset_of_line(line) + max_col
            } else {
                region.end()
            };

            let new_region =
                SelRegion::new(start, end, region.horiz().map(|h| h.clone()));
            result.add_region(new_region);
        }
        result
    }

    pub fn fill_horiz(&self, selection: &Selection) -> Selection {
        let mut result = Selection::new();
        for region in selection.regions() {
            let new_region = if region.horiz().is_some() {
                region.clone()
            } else {
                let (_, col) = self.offset_to_line_col(region.min());
                SelRegion::new(
                    region.start(),
                    region.end(),
                    Some(ColPosition::Col(col)),
                )
            };
            result.add_region(new_region);
        }
        result
    }

    fn update_text_layouts(&mut self, inval_lines: &InvalLines) {
        let mut new_layouts = Vec::new();
        if inval_lines.start_line < self.text_layouts.len() {
            new_layouts.extend_from_slice(
                &self.text_layouts[..inval_lines.start_line],
            );
        }
        for _ in 0..inval_lines.new_count {
            new_layouts.push(None);
        }
        if inval_lines.start_line + inval_lines.inval_count
            < self.text_layouts.len()
        {
            new_layouts.extend_from_slice(
                &self.text_layouts
                    [inval_lines.start_line + inval_lines.inval_count..],
            );
        }
        self.text_layouts = new_layouts;
    }

    fn update_size(&mut self, inval_lines: &InvalLines) {
        if self.max_len_line >= inval_lines.start_line
            && self.max_len_line
                < inval_lines.start_line + inval_lines.inval_count
        {
            self.update_max_line_len();
        } else {
            let mut max_len = 0;
            let mut max_len_line = 0;
            for line in inval_lines.start_line
                ..inval_lines.start_line + inval_lines.new_count
            {
                let line_len = self.line_len(line);
                if line_len > max_len {
                    max_len = line_len;
                    max_len_line = line;
                }
            }
            if max_len > self.max_len {
                self.max_len = max_len;
                self.max_len_line = max_len_line;
            } else if self.max_len >= inval_lines.start_line {
                self.max_len_line = self.max_len_line + inval_lines.new_count
                    - inval_lines.inval_count;
            }
        }
    }

    fn apply_delta(
        &mut self,
        selection: &Selection,
        delta: &RopeDelta,
    ) -> Selection {
        let (iv, newlen) = delta.summary();
        let old_logical_end_line = self.rope.line_of_offset(iv.end) + 1;
        let old_logical_end_offset =
            self.rope.offset_of_line(old_logical_end_line);
        self.rope = delta.apply(&self.rope);

        let logical_start_line = self.rope.line_of_offset(iv.start);
        let new_logical_end_line =
            self.rope.line_of_offset(iv.start + newlen) + 1;
        let old_hard_count = old_logical_end_line - logical_start_line;
        let new_hard_count = new_logical_end_line - logical_start_line;

        let inval_lines = InvalLines {
            start_line: logical_start_line,
            inval_count: old_hard_count,
            new_count: new_hard_count,
        };
        self.highlights = self.highlights_apply_delta(delta);
        self.update_highlights(Some(inval_lines.clone()));
        self.update_size(&inval_lines);
        self.update_text_layouts(&inval_lines);
        // self.inval_lines = Some(inval_lines);

        self.fill_horiz(&selection.apply_delta(
            delta,
            true,
            InsertDrift::Default,
        ))
    }

    pub fn yank(&self, selection: &Selection) -> Vec<String> {
        selection
            .regions()
            .iter()
            .map(|region| {
                self.rope
                    .slice_to_cow(region.min()..region.max())
                    .to_string()
            })
            .collect()
    }

    pub fn delete(&mut self, selection: &Selection, mode: &Mode) -> Selection {
        let mut builder = DeltaBuilder::new(self.rope.len());

        for region in selection.regions() {
            let end = if mode != &Mode::Insert {
                region.max()
            } else {
                region.max()
            };
            if end != region.min() {
                builder.delete(region.min()..end);
            }
        }
        self.apply_delta(selection, &builder.build())
    }

    pub fn delete_foreward(
        &mut self,
        selection: &Selection,
        mode: &Mode,
        count: usize,
    ) -> Selection {
        let mut builder = DeltaBuilder::new(self.rope.len());

        for region in selection.regions() {
            let end = if mode != &Mode::Insert {
                region.max()
            } else {
                Movement::Right
                    .update_region(&region, &self, count, true, false)
                    .max()
            };
            if end != region.min() {
                builder.delete(region.min()..end);
            }
        }
        self.apply_delta(selection, &builder.build())
    }

    pub fn delete_backward(&mut self, selection: &Selection) -> Selection {
        let mut builder = DeltaBuilder::new(self.rope.len());
        for region in selection.regions() {
            let start = if !region.is_caret() {
                region.min()
            } else {
                region.min() - 1
            };
            if start != region.max() {
                builder.delete(start..region.max());
            }
        }
        self.apply_delta(selection, &builder.build())
    }

    pub fn do_move(
        &mut self,
        mode: &Mode,
        movement: Movement,
        selection: &Selection,
        operator: Option<EditorOperator>,
        count: Option<usize>,
    ) -> Selection {
        if let Some(operator) = operator {
            let selection = movement.update_selection(
                selection,
                &self,
                count.unwrap_or(1),
                true,
                true,
            );
            let mut new_selection = Selection::new();
            for region in selection.regions() {
                let start_line = self.line_of_offset(region.min());
                let end_line = self.line_of_offset(region.max());
                let new_region = if movement.is_vertical() {
                    let region = SelRegion::new(
                        self.offset_of_line(start_line),
                        self.offset_of_line(end_line + 1),
                        Some(ColPosition::Col(0)),
                    );
                    region
                } else {
                    if movement.is_inclusive() {
                        SelRegion::new(
                            region.min(),
                            region.max() + 1,
                            region.horiz().map(|h| h.clone()),
                        )
                    } else {
                        region.clone()
                    }
                };
                new_selection.add_region(new_region);
            }
            match operator {
                EditorOperator::Delete(_) => self.delete(&new_selection, mode),
                EditorOperator::Yank(_) => {
                    self.delete_foreward(&new_selection, mode, 1)
                }
            }
        } else {
            movement.update_selection(
                &selection,
                &self,
                count.unwrap_or(1),
                mode == &Mode::Insert,
                mode == &Mode::Visual,
            )
        }
    }

    pub fn insert(
        &mut self,
        content: &str,
        selection: &Selection,
    ) -> Selection {
        let rope = Rope::from(content);
        let mut builder = DeltaBuilder::new(self.len());
        for region in selection.regions() {
            builder.replace(region.min()..region.max(), rope.clone());
        }
        self.apply_delta(selection, &builder.build())
    }

    pub fn indent_on_line(&self, line: usize) -> String {
        let line_start_offset = self.rope.offset_of_line(line);
        let word_boundary = WordCursor::new(&self.rope, line_start_offset)
            .next_non_blank_char();
        let indent = self.rope.slice_to_cow(line_start_offset..word_boundary);
        indent.to_string()
    }

    pub fn line_of_offset(&self, offset: usize) -> usize {
        self.rope.line_of_offset(offset)
    }

    pub fn offset_of_line(&self, line: usize) -> usize {
        self.rope.offset_of_line(line)
    }

    pub fn offset_to_line_col(&self, offset: usize) -> (usize, usize) {
        LogicalLines.offset_to_line_col(&self.rope, offset)
    }

    pub fn num_lines(&self) -> usize {
        self.line_of_offset(self.rope.len()) + 1
    }

    pub fn line_len(&self, line: usize) -> usize {
        self.offset_of_line(line + 1) - self.offset_of_line(line)
    }

    pub fn update_max_line_len(&mut self) {
        let mut pre_offset = 0;
        let mut max_len = 0;
        let mut max_len_line = 0;
        for line in 0..self.num_lines() {
            let offset = self.rope.offset_of_line(line);
            let line_len = offset - pre_offset;
            pre_offset = offset;
            if line_len > max_len {
                max_len = line_len;
                max_len_line = line;
            }
        }
        self.max_len = max_len;
        self.max_len_line = max_len_line;
    }

    pub fn last_line(&self) -> usize {
        self.line_of_offset(self.rope.len())
    }

    pub fn line_max_col(&self, line: usize, include_newline: bool) -> usize {
        match self.offset_of_line(line + 1) - self.offset_of_line(line) {
            n if n == 0 => 0,
            n if n == 1 => 0,
            n => match include_newline {
                true => n - 1,
                false => n - 2,
            },
        }
    }

    pub fn line_horiz_col(
        &self,
        line: usize,
        horiz: &ColPosition,
        include_newline: bool,
    ) -> usize {
        let max_col = self.line_max_col(line, include_newline);
        match horiz {
            &ColPosition::Col(n) => match max_col > n {
                true => n,
                false => max_col,
            },
            &ColPosition::End => max_col,
            _ => 0,
        }
    }

    pub fn line_end_offset(
        &self,
        offset: usize,
        include_newline: bool,
    ) -> usize {
        let line = self.line_of_offset(offset);
        let line_start_offset = self.offset_of_line(line);
        let line_end_offset = self.offset_of_line(line + 1);
        let line_end_offset = if line_end_offset - line_start_offset <= 1 {
            line_start_offset
        } else {
            if include_newline {
                line_end_offset - 1
            } else {
                line_end_offset - 2
            }
        };
        line_end_offset
    }

    pub fn first_non_blank_character_on_line(&self, line: usize) -> usize {
        let line_start_offset = self.rope.offset_of_line(line);
        WordCursor::new(&self.rope, line_start_offset).next_non_blank_char()
    }

    pub fn word_forward(&self, offset: usize) -> usize {
        WordCursor::new(&self.rope, offset).next_boundary().unwrap()
    }

    pub fn word_end_forward(&self, offset: usize) -> usize {
        WordCursor::new(&self.rope, offset).end_boundary().unwrap()
    }

    pub fn word_backword(&self, offset: usize) -> usize {
        WordCursor::new(&self.rope, offset).prev_boundary().unwrap()
    }

    pub fn slice_to_cow<T: IntervalBounds>(&self, range: T) -> Cow<str> {
        self.rope.slice_to_cow(range)
    }

    pub fn update_line_layouts(
        &mut self,
        text: &mut PietText,
        theme: &HashMap<String, Color>,
        line: usize,
        env: &Env,
    ) -> bool {
        if line >= self.num_lines() {
            return false;
        }

        let line_content = self
            .slice_to_cow(
                self.offset_of_line(line)..self.offset_of_line(line + 1),
            )
            .to_string();
        let line_hightlight = self.get_line_highligh(line).clone();
        if self.text_layouts[line].is_none()
            || self.text_layouts[line].as_ref().unwrap().text != line_content
            || self.text_layouts[line].as_ref().unwrap().highlights
                != line_hightlight
        {
            self.text_layouts[line] = Some(self.get_text_layout(
                text,
                theme,
                line,
                line_content,
                env,
            ));
            return true;
        }

        false

        // if let Some(text_layout) = self.text_layouts[line].as_ref() {
        //     if text_layout.text != line_content
        //         || &text_layout.highlights != self.get_line_highligh(line)
        //     {
        //         self.text_layouts[line] = Some(self.get_text_layout(
        //             text,
        //             data,
        //             line,
        //             line_content,
        //             env,
        //         ));
        //     }
        // } else {
        //     self.text_layouts[line] =
        //         Some(self.get_text_layout(text, data, line, line_content, env));
        // }
    }

    pub fn get_text_layout(
        &mut self,
        text: &mut PietText,
        theme: &HashMap<String, Color>,
        line: usize,
        line_content: String,
        env: &Env,
    ) -> HighlightTextLayout {
        let mut layout_builder = text
            .new_text_layout(line_content.clone())
            .font(env.get(LapceTheme::EDITOR_FONT).family, 13.0)
            .text_color(env.get(LapceTheme::EDITOR_FOREGROUND));
        for (start, end, hl) in self.get_line_highligh(line) {
            if let Some(color) = theme.get(hl) {
                layout_builder = layout_builder.range_attribute(
                    start..end,
                    TextAttribute::TextColor(color.clone()),
                );
            }
        }
        let layout = layout_builder.build().unwrap();
        HighlightTextLayout {
            layout,
            text: line_content,
            highlights: self.get_line_highligh(line).clone(),
        }
    }
}

fn load_file(path: &str) -> Result<Rope> {
    let mut f = File::open(path)?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    Ok(Rope::from(std::str::from_utf8(&bytes)?))
}

pub struct WordCursor<'a> {
    inner: Cursor<'a, RopeInfo>,
}

impl<'a> WordCursor<'a> {
    pub fn new(text: &'a Rope, pos: usize) -> WordCursor<'a> {
        let inner = Cursor::new(text, pos);
        WordCursor { inner }
    }

    /// Get previous boundary, and set the cursor at the boundary found.
    pub fn prev_boundary(&mut self) -> Option<usize> {
        if let Some(ch) = self.inner.prev_codepoint() {
            let mut prop = get_word_property(ch);
            let mut candidate = self.inner.pos();
            while let Some(prev) = self.inner.prev_codepoint() {
                let prop_prev = get_word_property(prev);
                if classify_boundary(prop_prev, prop).is_start() {
                    break;
                }
                prop = prop_prev;
                candidate = self.inner.pos();
            }
            self.inner.set(candidate);
            return Some(candidate);
        }
        None
    }

    pub fn next_non_blank_char(&mut self) -> usize {
        let mut candidate = self.inner.pos();
        while let Some(next) = self.inner.next_codepoint() {
            let prop = get_word_property(next);
            if prop != WordProperty::Space {
                break;
            }
            candidate = self.inner.pos();
        }
        self.inner.set(candidate);
        candidate
    }

    /// Get next boundary, and set the cursor at the boundary found.
    pub fn next_boundary(&mut self) -> Option<usize> {
        if let Some(ch) = self.inner.next_codepoint() {
            let mut prop = get_word_property(ch);
            let mut candidate = self.inner.pos();
            while let Some(next) = self.inner.next_codepoint() {
                let prop_next = get_word_property(next);
                if classify_boundary(prop, prop_next).is_start() {
                    break;
                }
                prop = prop_next;
                candidate = self.inner.pos();
            }
            self.inner.set(candidate);
            return Some(candidate);
        }
        None
    }

    pub fn end_boundary(&mut self) -> Option<usize> {
        self.inner.next_codepoint();
        if let Some(ch) = self.inner.next_codepoint() {
            let mut prop = get_word_property(ch);
            let mut candidate = self.inner.pos();
            while let Some(next) = self.inner.next_codepoint() {
                let prop_next = get_word_property(next);
                if classify_boundary(prop, prop_next).is_end() {
                    break;
                }
                prop = prop_next;
                candidate = self.inner.pos();
            }
            self.inner.set(candidate);
            return Some(candidate - 1);
        }
        None
    }

    /// Return the selection for the word containing the current cursor. The
    /// cursor is moved to the end of that selection.
    pub fn select_word(&mut self) -> (usize, usize) {
        let initial = self.inner.pos();
        let init_prop_after =
            self.inner.next_codepoint().map(get_word_property);
        self.inner.set(initial);
        let init_prop_before =
            self.inner.prev_codepoint().map(get_word_property);
        let mut start = initial;
        let init_boundary =
            if let (Some(pb), Some(pa)) = (init_prop_before, init_prop_after) {
                classify_boundary_initial(pb, pa)
            } else {
                WordBoundary::Both
            };
        let mut prop_after = init_prop_after;
        let mut prop_before = init_prop_before;
        if prop_after.is_none() {
            start = self.inner.pos();
            prop_after = prop_before;
            prop_before = self.inner.prev_codepoint().map(get_word_property);
        }
        while let (Some(pb), Some(pa)) = (prop_before, prop_after) {
            if start == initial {
                if init_boundary.is_start() {
                    break;
                }
            } else if !init_boundary.is_boundary() {
                if classify_boundary(pb, pa).is_boundary() {
                    break;
                }
            } else if classify_boundary(pb, pa).is_start() {
                break;
            }
            start = self.inner.pos();
            prop_after = prop_before;
            prop_before = self.inner.prev_codepoint().map(get_word_property);
        }
        self.inner.set(initial);
        let mut end = initial;
        prop_after = init_prop_after;
        prop_before = init_prop_before;
        if prop_before.is_none() {
            prop_before = self.inner.next_codepoint().map(get_word_property);
            end = self.inner.pos();
            prop_after = self.inner.next_codepoint().map(get_word_property);
        }
        while let (Some(pb), Some(pa)) = (prop_before, prop_after) {
            if end == initial {
                if init_boundary.is_end() {
                    break;
                }
            } else if !init_boundary.is_boundary() {
                if classify_boundary(pb, pa).is_boundary() {
                    break;
                }
            } else if classify_boundary(pb, pa).is_end() {
                break;
            }
            end = self.inner.pos();
            prop_before = prop_after;
            prop_after = self.inner.next_codepoint().map(get_word_property);
        }
        self.inner.set(end);
        (start, end)
    }
}

#[derive(PartialEq, Eq)]
enum WordBoundary {
    Interior,
    Start, // a boundary indicating the end of a word
    End,   // a boundary indicating the start of a word
    Both,
}

impl WordBoundary {
    fn is_start(&self) -> bool {
        *self == WordBoundary::Start || *self == WordBoundary::Both
    }

    fn is_end(&self) -> bool {
        *self == WordBoundary::End || *self == WordBoundary::Both
    }

    fn is_boundary(&self) -> bool {
        *self != WordBoundary::Interior
    }
}

fn classify_boundary(prev: WordProperty, next: WordProperty) -> WordBoundary {
    use self::WordBoundary::*;
    use self::WordProperty::*;
    match (prev, next) {
        (Lf, Lf) => Start,
        (Lf, Space) => Interior,
        (_, Lf) => End,
        (Lf, _) => Start,
        (Space, Space) => Interior,
        (_, Space) => End,
        (Space, _) => Start,
        (Punctuation, Other) => Both,
        (Other, Punctuation) => Both,
        _ => Interior,
    }
}

fn classify_boundary_initial(
    prev: WordProperty,
    next: WordProperty,
) -> WordBoundary {
    use self::WordBoundary::*;
    use self::WordProperty::*;
    match (prev, next) {
        (Lf, Other) => Start,
        (Other, Lf) => End,
        (Lf, Space) => Interior,
        (Lf, Punctuation) => Interior,
        (Space, Lf) => Interior,
        (Punctuation, Lf) => Interior,
        (Space, Punctuation) => Interior,
        (Punctuation, Space) => Interior,
        _ => classify_boundary(prev, next),
    }
}

#[derive(Copy, Clone, PartialEq)]
enum WordProperty {
    Lf,
    Space,
    Punctuation,
    Other, // includes letters and all of non-ascii unicode
}

fn get_word_property(codepoint: char) -> WordProperty {
    if codepoint <= ' ' {
        if codepoint == '\n' {
            return WordProperty::Lf;
        }
        return WordProperty::Space;
    } else if codepoint <= '\u{3f}' {
        if (0xfc00fffe00000000u64 >> (codepoint as u32)) & 1 != 0 {
            return WordProperty::Punctuation;
        }
    } else if codepoint <= '\u{7f}' {
        // Hardcoded: @[\]^`{|}~
        if (0x7800000178000001u64 >> ((codepoint as u32) & 0x3f)) & 1 != 0 {
            return WordProperty::Punctuation;
        }
    }
    WordProperty::Other
}

impl BufferUIState {
    // pub fn update(
    //     &mut self,
    //     text: &mut PietText,
    //     buffer: &mut Buffer,
    //     inval_lines: &InvalLines,
    //     buffer_lines: HashMap<usize, usize>,
    //     env: &Env,
    // ) {
    //     let mut new_layouts = Vec::new();
    //     if inval_lines.start_line < self.text_layouts.len() {
    //         new_layouts.extend_from_slice(
    //             &self.text_layouts[..inval_lines.start_line],
    //         );
    //     }
    //     for _ in 0..inval_lines.new_count {
    //         new_layouts.push(None);
    //     }
    //     if inval_lines.start_line + inval_lines.inval_count
    //         < self.text_layouts.len()
    //     {
    //         new_layouts.extend_from_slice(
    //             &self.text_layouts
    //                 [inval_lines.start_line + inval_lines.inval_count..],
    //         );
    //     }
    //     self.text_layouts = new_layouts;

    //     for (line, _) in buffer_lines.iter() {
    //         self.update_line_layouts(text, buffer, *line, env);
    //     }
    // }

    // pub fn update_layouts(
    //     &mut self,
    //     text: &mut PietText,
    //     buffer: &mut Buffer,
    //     buffer_lines: &[usize],
    //     env: &Env,
    // ) {
    //     for line in buffer_lines {
    //         self.update_line_layouts(text, buffer, *line, env);
    //     }
    // }

    // pub fn update_line_layouts(
    //     &mut self,
    //     text: &mut PietText,
    //     buffer: &mut Buffer,
    //     line: usize,
    //     env: &Env,
    // ) {
    //     if line >= buffer.num_lines() {
    //         return;
    //     }
    //     let line_content = buffer
    //         .slice_to_cow(
    //             buffer.offset_of_line(line)..buffer.offset_of_line(line + 1),
    //         )
    //         .to_string();
    //     if line >= self.text_layouts.len() {
    //         for _ in self.text_layouts.len()..line + 1 {
    //             self.text_layouts.push(None);
    //         }
    //     }

    //     if let Some(text_layout) = self.text_layouts[line].as_ref() {
    //         if text_layout.text != line_content
    //             || &text_layout.highlights != buffer.get_line_highligh(line)
    //         {
    //             self.text_layouts[line] = Some(Self::get_text_layout(
    //                 text,
    //                 buffer,
    //                 line,
    //                 line_content,
    //                 env,
    //             ));
    //         }
    //     } else {
    //         self.text_layouts[line] = Some(Self::get_text_layout(
    //             text,
    //             buffer,
    //             line,
    //             line_content,
    //             env,
    //         ));
    //     }
    // }

    // pub fn get_text_layout(
    //     text: &mut PietText,
    //     buffer: &mut Buffer,
    //     line: usize,
    //     line_content: String,
    //     env: &Env,
    // ) -> HighlightTextLayout {
    //     // let start_offset = buffer.offset_of_line(line);
    //     let mut layout_builder = text
    //         .new_text_layout(line_content.clone())
    //         .font(env.get(LapceTheme::EDITOR_FONT).family, 13.0)
    //         .text_color(env.get(LapceTheme::EDITOR_FOREGROUND));
    //     // for (start, end, hl) in buffer.get_line_highligh(line) {
    //     //     if let Some(color) = LAPCE_STATE.theme.lock().unwrap().get(hl) {
    //     //         layout_builder = layout_builder.range_attribute(
    //     //             start..end,
    //     //             TextAttribute::TextColor(color.clone()),
    //     //         );
    //     //     }
    //     // }
    //     let layout = layout_builder.build().unwrap();
    //     HighlightTextLayout {
    //         layout,
    //         text: line_content,
    //         highlights: buffer.get_line_highligh(line).clone(),
    //     }
    // }
}
