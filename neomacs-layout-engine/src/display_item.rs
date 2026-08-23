use crate::buffer_source::producer::frame::ReplacementCoveredSpan;
use crate::display_property::DisplayPropertyClassification;
use neomacs_display_protocol::types::FaceId;
use neovm_core::buffer::{BufferId, CharPos0, EmacsBytePos};
use neovm_core::emacs_core::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DisplaySourceId(u64);

impl DisplaySourceId {
    pub(crate) const fn new(id: u64) -> Self {
        Self(id)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for DisplaySourceId {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DisplaySourcePosition {
    Buffer {
        buffer_id: BufferId,
        char_pos: CharPos0,
        byte_pos: EmacsBytePos,
    },
    LispString {
        source_id: DisplaySourceId,
        char_index: usize,
        byte_index: usize,
    },
    Synthetic {
        source_id: DisplaySourceId,
        offset: usize,
    },
}

impl DisplaySourcePosition {
    pub(crate) const fn buffer(
        buffer_id: BufferId,
        char_pos: CharPos0,
        byte_pos: EmacsBytePos,
    ) -> Self {
        Self::Buffer {
            buffer_id,
            char_pos,
            byte_pos,
        }
    }

    pub(crate) const fn lisp_string(source_id: u64, char_index: usize, byte_index: usize) -> Self {
        Self::LispString {
            source_id: DisplaySourceId::new(source_id),
            char_index,
            byte_index,
        }
    }

    pub(crate) const fn synthetic(source_id: u64, offset: usize) -> Self {
        Self::Synthetic {
            source_id: DisplaySourceId::new(source_id),
            offset,
        }
    }

    pub(crate) fn lisp_string_char_index(&self) -> Option<usize> {
        match self {
            Self::LispString { char_index, .. } => Some(*char_index),
            _ => None,
        }
    }

    /// Advance within this source without changing its coordinate space.
    ///
    /// Buffer positions, Lisp-string indices, and synthetic offsets are
    /// intentionally separate enum arms.  Fragmentation code therefore
    /// cannot advance a string remainder as though it were buffer text.
    pub(crate) fn advanced_by(&self, char_offset: usize, byte_offset: usize) -> Self {
        match self {
            Self::Buffer {
                buffer_id,
                char_pos,
                byte_pos,
            } => Self::buffer(
                *buffer_id,
                CharPos0::new(char_pos.get().saturating_add(char_offset)),
                EmacsBytePos::new(byte_pos.get().saturating_add(byte_offset)),
            ),
            Self::LispString {
                source_id,
                char_index,
                byte_index,
            } => Self::lisp_string(
                source_id.get(),
                char_index.saturating_add(char_offset),
                byte_index.saturating_add(byte_offset),
            ),
            Self::Synthetic { source_id, offset } => {
                Self::synthetic(source_id.get(), offset.saturating_add(char_offset))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SourceSpan {
    pub(crate) start: DisplaySourcePosition,
    pub(crate) end: DisplaySourcePosition,
}

impl SourceSpan {
    pub(crate) const fn new(start: DisplaySourcePosition, end: DisplaySourcePosition) -> Self {
        Self { start, end }
    }

    pub(crate) fn buffer_end_charpos(&self) -> Option<CharPos0> {
        let DisplaySourcePosition::Buffer { char_pos, .. } = self.end else {
            return None;
        };
        Some(char_pos)
    }

    pub(crate) fn buffer_byte_len(&self) -> Option<usize> {
        let DisplaySourcePosition::Buffer {
            byte_pos: start, ..
        } = self.start
        else {
            return None;
        };
        let DisplaySourcePosition::Buffer { byte_pos: end, .. } = self.end else {
            return None;
        };
        end.get().checked_sub(start.get())
    }

    #[cfg(test)]
    pub(crate) const fn lisp_string(
        source_id: u64,
        start_char: usize,
        end_char: usize,
        start_byte: usize,
        end_byte: usize,
    ) -> Self {
        Self::new(
            DisplaySourcePosition::lisp_string(source_id, start_char, start_byte),
            DisplaySourcePosition::lisp_string(source_id, end_char, end_byte),
        )
    }

    pub(crate) const fn synthetic(source_id: u64, start_offset: usize, end_offset: usize) -> Self {
        Self::new(
            DisplaySourcePosition::synthetic(source_id, start_offset),
            DisplaySourcePosition::synthetic(source_id, end_offset),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RenderFaceRef {
    #[allow(dead_code)]
    Inherit,
    FaceId(FaceId),
}

/// Semantic source range whose rendered primitives share one transient
/// `mouse-face` appearance.  The end position (together with the source
/// identity) is stable when a run is clipped and resumed on a wrapped row.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DisplayPointerSourceRange {
    source: DisplaySourcePosition,
    start_char_index: usize,
    end_char_index: usize,
    overlay_owner: Option<Value>,
    occurrence: DisplayPointerOccurrence,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) enum DisplayPointerOccurrence {
    #[default]
    Source,
    OverlayString {
        overlay_id: Value,
        kind: crate::display_origin::OverlayStringKind,
    },
    BufferDisplayReplacement {
        buffer_id: BufferId,
        anchor_charpos: CharPos0,
    },
}

impl DisplayPointerSourceRange {
    #[cfg(test)]
    pub(crate) fn ending_at(source: DisplaySourcePosition, end_char_index: usize) -> Self {
        Self {
            source,
            start_char_index: 0,
            end_char_index,
            overlay_owner: None,
            occurrence: DisplayPointerOccurrence::Source,
        }
    }

    pub(crate) fn effective(
        source: DisplaySourcePosition,
        start_char_index: usize,
        end_char_index: usize,
        overlay_owner: Option<Value>,
    ) -> Self {
        Self {
            source,
            start_char_index,
            end_char_index,
            overlay_owner,
            occurrence: DisplayPointerOccurrence::Source,
        }
    }

    pub(crate) fn in_occurrence(mut self, occurrence: DisplayPointerOccurrence) -> Self {
        self.occurrence = occurrence;
        self
    }

    #[cfg(test)]
    pub(crate) fn buffer_id(&self) -> Option<BufferId> {
        match self.source {
            DisplaySourcePosition::Buffer { buffer_id, .. } => Some(buffer_id),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn source_id(&self) -> Option<DisplaySourceId> {
        match self.source {
            DisplaySourcePosition::LispString { source_id, .. }
            | DisplaySourcePosition::Synthetic { source_id, .. } => Some(source_id),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) const fn end_char_index(&self) -> usize {
        self.end_char_index
    }

    fn protocol_identity(
        &self,
    ) -> neomacs_display_protocol::glyph_matrix::GlyphPointerSourceIdentity {
        use neomacs_display_protocol::glyph_matrix::{
            GlyphPointerOccurrenceIdentity, GlyphPointerSourceIdentity, GlyphPointerSourceKind,
        };
        let (kind, source_id) = match self.source {
            DisplaySourcePosition::Buffer { buffer_id, .. } => {
                (GlyphPointerSourceKind::Buffer, buffer_id.0)
            }
            DisplaySourcePosition::LispString { source_id, .. } => {
                (GlyphPointerSourceKind::LispString, source_id.get())
            }
            DisplaySourcePosition::Synthetic { source_id, .. } => {
                (GlyphPointerSourceKind::Synthetic, source_id.get())
            }
        };
        let occurrence = match self.occurrence {
            DisplayPointerOccurrence::Source => GlyphPointerOccurrenceIdentity::Source,
            DisplayPointerOccurrence::OverlayString { overlay_id, kind } => {
                GlyphPointerOccurrenceIdentity::OverlayString {
                    overlay_id: overlay_id.bits() as u64,
                    after: matches!(kind, crate::display_origin::OverlayStringKind::After),
                }
            }
            DisplayPointerOccurrence::BufferDisplayReplacement {
                buffer_id,
                anchor_charpos,
            } => GlyphPointerOccurrenceIdentity::BufferDisplayReplacement {
                buffer_id: buffer_id.0,
                anchor: anchor_charpos.get() as u64,
            },
        };
        GlyphPointerSourceIdentity {
            kind,
            source_id,
            range_start: self.start_char_index as u64,
            range_end: self.end_char_index as u64,
            property_owner: self.overlay_owner.map_or(0, |owner| owner.bits() as u64),
            occurrence,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DisplayPointerAppearance {
    source: DisplayPointerSourceRange,
    face: RenderFaceRef,
}

impl DisplayPointerAppearance {
    pub(crate) const fn new(source: DisplayPointerSourceRange, face: RenderFaceRef) -> Self {
        Self { source, face }
    }

    #[cfg(test)]
    pub(crate) const fn source(&self) -> &DisplayPointerSourceRange {
        &self.source
    }

    #[cfg(test)]
    pub(crate) const fn face(&self) -> RenderFaceRef {
        self.face
    }

    pub(crate) fn glyph_metadata(
        &self,
    ) -> Option<neomacs_display_protocol::glyph_matrix::GlyphPointerAppearance> {
        let RenderFaceRef::FaceId(face_id) = self.face else {
            return None;
        };
        Some(
            neomacs_display_protocol::glyph_matrix::GlyphPointerAppearance {
                source: self.source.protocol_identity(),
                face_id,
            },
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DisplayItem {
    pub(crate) span: SourceSpan,
    pub(crate) face: RenderFaceRef,
    pub(crate) kind: DisplayItemKind,
    pub(crate) layout: DisplayItemLayout,
    pub(crate) pointer_appearance: Option<DisplayPointerAppearance>,
}

impl DisplayItem {
    pub(crate) const fn new(span: SourceSpan, face: RenderFaceRef, kind: DisplayItemKind) -> Self {
        Self {
            span,
            face,
            kind,
            layout: DisplayItemLayout {
                raise: None,
                height: None,
                space_width: None,
                break_after_row: false,
            },
            pointer_appearance: None,
        }
    }

    pub(crate) const fn with_layout(mut self, layout: DisplayItemLayout) -> Self {
        self.layout = layout;
        self
    }

    /// Mark this item so the current display row ends immediately after it.
    /// See [`DisplayItemLayout::break_after_row`].
    pub(crate) const fn with_break_after_row(mut self) -> Self {
        self.layout.break_after_row = true;
        self
    }

    pub(crate) fn with_pointer_appearance(
        mut self,
        appearance: Option<DisplayPointerAppearance>,
    ) -> Self {
        self.pointer_appearance = appearance;
        self
    }

    #[cfg(test)]
    pub(crate) fn pointer_appearance(&self) -> Option<&DisplayPointerAppearance> {
        self.pointer_appearance.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct DisplayItemLayout {
    pub(crate) raise: Option<f32>,
    pub(crate) height: Option<f32>,
    pub(crate) space_width: Option<f32>,
    /// End the current display row immediately after this item, without
    /// consuming another buffer character. Set for a `SourceMappedText` that
    /// stands in for a display-table entry whose glyph vector ends in a newline
    /// (e.g. whitespace-mode's `[$ \n]` on `?\n`): GNU treats the trailing `\n`
    /// glyph as its own end-of-line display element (`ITERATOR_AT_END_OF_LINE_P`
    /// tests `it->c == '\n'` for display-vector elements too, xdisp.c), so the
    /// leading glyphs render and then the row breaks.
    pub(crate) break_after_row: bool,
}

impl DisplayItemLayout {
    pub(crate) fn horizontal_advance_px(self, ch: char, advance_px: f32) -> f32 {
        if ch != ' ' {
            return advance_px;
        }
        self.space_width
            .filter(|factor| factor.is_finite() && *factor > 0.0)
            .map(|factor| advance_px * factor)
            .unwrap_or(advance_px)
    }

    pub(crate) fn vertical_offset_px(self, row_height_px: f32) -> f32 {
        self.raise
            .filter(|factor| factor.is_finite())
            // GNU stores `it->voffset` as an integer.  The floating product
            // is therefore truncated toward zero before it reaches glyph
            // metrics or drawing (xdisp.c `handle_single_display_spec`).
            .map(|factor| -(factor * row_height_px.max(1.0)).trunc())
            .unwrap_or(0.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DisplayItemKind {
    TextRun(DisplayTextRun),
    SourceMappedText(DisplaySourceMappedText),
    ControlChar { ch: char },
    Glyphless(DisplayGlyphless),
    Stretch(DisplayStretch),
    MediaReplacement(DisplayMediaReplacement),
    RowBreak(DisplayRowBreak),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BufferDisplayReplacementSource {
    buffer_id: BufferId,
    char_pos: CharPos0,
    byte_pos: EmacsBytePos,
    end_char_pos: CharPos0,
    end_byte_pos: EmacsBytePos,
}

impl BufferDisplayReplacementSource {
    #[cfg(test)]
    pub(crate) fn new(buffer_id: BufferId, char_pos: CharPos0, byte_pos: EmacsBytePos) -> Self {
        Self {
            buffer_id,
            char_pos,
            byte_pos,
            end_char_pos: char_pos.add_len(neovm_core::buffer::CharLen::new(1)),
            end_byte_pos: byte_pos,
        }
    }

    pub(crate) fn spanning(
        buffer_id: BufferId,
        char_pos: CharPos0,
        byte_pos: EmacsBytePos,
        end_char_pos: CharPos0,
        end_byte_pos: EmacsBytePos,
    ) -> Self {
        Self {
            buffer_id,
            char_pos,
            byte_pos,
            end_char_pos,
            end_byte_pos,
        }
    }

    pub(crate) fn buffer_id(self) -> BufferId {
        self.buffer_id
    }

    pub(crate) const fn pointer_occurrence(self) -> DisplayPointerOccurrence {
        DisplayPointerOccurrence::BufferDisplayReplacement {
            buffer_id: self.buffer_id,
            anchor_charpos: self.char_pos,
        }
    }

    fn span(self) -> SourceSpan {
        SourceSpan::new(
            DisplaySourcePosition::buffer(self.buffer_id, self.char_pos, self.byte_pos),
            DisplaySourcePosition::buffer(self.buffer_id, self.end_char_pos, self.end_byte_pos),
        )
    }

    fn item(self, face_id: FaceId, kind: DisplayItemKind) -> DisplayItem {
        self.item_with_face(RenderFaceRef::FaceId(face_id), kind)
    }

    pub(crate) fn display_item(self, face_id: FaceId, kind: DisplayItemKind) -> DisplayItem {
        self.item(face_id, kind)
    }

    fn item_with_face(self, face: RenderFaceRef, kind: DisplayItemKind) -> DisplayItem {
        DisplayItem::new(self.span(), face, kind)
    }

    pub(crate) fn item_from_replacement_string_item(self, item: DisplayItem) -> DisplayItem {
        let glyph_string_start = item.span.start.clone();
        let kind = match item.kind {
            DisplayItemKind::TextRun(run) => DisplayItemKind::SourceMappedText(
                DisplaySourceMappedText::from_string_run(run.text, glyph_string_start),
            ),
            kind => kind,
        };
        self.item_with_face(item.face, kind)
            .with_layout(item.layout)
            .with_pointer_appearance(item.pointer_appearance)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DisplayPropertyReplacementDescriptor {
    value: Value,
    classification: DisplayPropertyClassification,
    replacement_source: BufferDisplayReplacementSource,
    /// The buffer range this replacement stands for, derived once by the
    /// producer. `covered.start()` is GNU's B (what the glyphs are stamped
    /// with) and `covered.resume()` is GNU's E (where the walk continues).
    /// Consumers READ the resume; nobody outside
    /// [`ReplacementCoveredSpan`] derives one.
    covered: ReplacementCoveredSpan,
    pointer_appearance: Option<DisplayPointerAppearance>,
}

impl DisplayPropertyReplacementDescriptor {
    pub(crate) fn new(
        value: Value,
        classification: DisplayPropertyClassification,
        replacement_source: BufferDisplayReplacementSource,
        covered: ReplacementCoveredSpan,
    ) -> Self {
        Self {
            value,
            classification,
            replacement_source,
            covered,
            pointer_appearance: None,
        }
    }

    pub(crate) fn classification(&self) -> &DisplayPropertyClassification {
        &self.classification
    }

    pub(crate) fn replacement_source(&self) -> BufferDisplayReplacementSource {
        self.replacement_source
    }

    pub(crate) fn anchor_charpos(&self) -> CharPos0 {
        self.covered.start()
    }

    /// GNU's E. The renderer APPLIES this to the walk once the replacement is
    /// appended; it is the producer's answer, not the renderer's.
    pub(crate) fn resume_charpos(&self) -> i64 {
        self.covered.resume().get() as i64
    }

    pub(crate) fn pointer_appearance(&self) -> Option<&DisplayPointerAppearance> {
        self.pointer_appearance.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BufferDisplayPropertyReplacementItem {
    descriptor: DisplayPropertyReplacementDescriptor,
    start_byte_pos: EmacsBytePos,
    end_byte_pos: EmacsBytePos,
    covered: ReplacementCoveredSpan,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BufferDisplayPropertyFallbackItem {
    item: DisplayItem,
    start_byte_idx: usize,
    start_charpos: i64,
    source_char: Option<char>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BufferDisplayPropertyReplacementAnchor {
    byte_idx: usize,
    charpos: i64,
}

impl BufferDisplayPropertyReplacementItem {
    pub(crate) fn new(
        value: Value,
        classification: DisplayPropertyClassification,
        replacement_source: BufferDisplayReplacementSource,
        start_byte_pos: EmacsBytePos,
        end_byte_pos: EmacsBytePos,
        covered: ReplacementCoveredSpan,
    ) -> Self {
        Self {
            descriptor: DisplayPropertyReplacementDescriptor::new(
                value,
                classification,
                replacement_source,
                covered,
            ),
            start_byte_pos,
            end_byte_pos,
            covered,
        }
    }

    pub(crate) fn descriptor(&self) -> &DisplayPropertyReplacementDescriptor {
        &self.descriptor
    }

    pub(crate) fn with_pointer_appearance(
        mut self,
        appearance: Option<DisplayPointerAppearance>,
    ) -> Self {
        self.descriptor.pointer_appearance = appearance;
        self
    }

    pub(crate) fn start_byte_idx(&self, text_start_byte: usize) -> Option<usize> {
        self.start_byte_pos.get().checked_sub(text_start_byte)
    }

    pub(crate) fn source_anchor(
        &self,
        text_start_byte: usize,
    ) -> Option<BufferDisplayPropertyReplacementAnchor> {
        Some(BufferDisplayPropertyReplacementAnchor {
            byte_idx: self.start_byte_idx(text_start_byte)?,
            charpos: self.start_charpos(),
        })
    }

    pub(crate) fn start_charpos(&self) -> i64 {
        self.covered.start().get() as i64
    }

    pub(crate) fn source_text<'a>(
        &self,
        text_start_byte: usize,
        text: &'a [u8],
    ) -> Option<&'a [u8]> {
        text.get(self.start_byte_idx(text_start_byte)?..)
    }

    pub(crate) fn fallback_display_item(
        &self,
        text_start_byte: usize,
        text: &[u8],
        face: RenderFaceRef,
    ) -> Option<BufferDisplayPropertyFallbackItem> {
        let start_byte_idx = self.start_byte_idx(text_start_byte)?;
        let end_byte_idx = self.end_byte_pos.get().checked_sub(text_start_byte)?;
        let source_text = std::str::from_utf8(text.get(start_byte_idx..end_byte_idx)?).ok()?;
        if source_text.is_empty() {
            return None;
        }
        let source_char = source_text.chars().next();
        let replacement_source = self.descriptor.replacement_source();
        let item = DisplayItem::new(
            SourceSpan::new(
                DisplaySourcePosition::buffer(
                    replacement_source.buffer_id(),
                    self.covered.start(),
                    self.start_byte_pos,
                ),
                DisplaySourcePosition::buffer(
                    replacement_source.buffer_id(),
                    self.covered.resume(),
                    self.end_byte_pos,
                ),
            ),
            face,
            DisplayItemKind::TextRun(DisplayTextRun::new(source_text.to_owned())),
        )
        .with_pointer_appearance(self.descriptor.pointer_appearance().cloned());
        Some(BufferDisplayPropertyFallbackItem {
            item,
            start_byte_idx,
            start_charpos: self.start_charpos(),
            source_char,
        })
    }
}

impl BufferDisplayPropertyFallbackItem {
    pub(crate) fn into_parts(self) -> (DisplayItem, usize, i64, Option<char>) {
        (
            self.item,
            self.start_byte_idx,
            self.start_charpos,
            self.source_char,
        )
    }
}

impl BufferDisplayPropertyReplacementAnchor {
    pub(crate) fn matches(self, byte_idx: usize, charpos: i64) -> bool {
        self.byte_idx == byte_idx && self.charpos == charpos
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayTextRun {
    pub(crate) text: Box<str>,
}

impl DisplayTextRun {
    pub(crate) fn new(text: impl Into<Box<str>>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplaySourceMappedText {
    pub(crate) text: Box<str>,
    /// Source of glyph indices when buffer coverage and glyph provenance are
    /// intentionally different (a string-valued `display` replacement).
    /// `None` retains the covered-start rule used by escape/composition
    /// expansions.
    pub(crate) glyph_string_start: Option<DisplaySourcePosition>,
    /// Optional homogeneous named face carried by a display-table glyph
    /// vector. Mixed or zero faces leave this unset and inherit the active
    /// buffer face.
    pub(crate) face_name: Option<Box<str>>,
}

impl DisplaySourceMappedText {
    pub(crate) fn new(text: impl Into<Box<str>>) -> Self {
        Self {
            text: text.into(),
            glyph_string_start: None,
            face_name: None,
        }
    }

    pub(crate) fn from_string_run(
        text: impl Into<Box<str>>,
        glyph_string_start: DisplaySourcePosition,
    ) -> Self {
        debug_assert!(matches!(
            glyph_string_start,
            DisplaySourcePosition::LispString { .. }
        ));
        Self {
            text: text.into(),
            glyph_string_start: Some(glyph_string_start),
            face_name: None,
        }
    }

    pub(crate) fn with_face_name(mut self, face_name: Option<String>) -> Self {
        self.face_name = face_name.map(Into::into);
        self
    }

    pub(crate) fn face_name(&self) -> Option<&str> {
        self.face_name.as_deref()
    }

    /// Keep the displayed text and its glyph-coordinate origin transactional
    /// when a row clips this item and carries the remainder forward.
    pub(crate) fn into_remainder_after(self, emitted_chars: usize) -> Option<Self> {
        let split_byte = self
            .text
            .char_indices()
            .nth(emitted_chars)
            .map(|(byte, _)| byte)?;
        Some(Self {
            text: self.text[split_byte..].into(),
            glyph_string_start: self
                .glyph_string_start
                .map(|start| start.advanced_by(emitted_chars, split_byte)),
            face_name: self.face_name,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GlyphlessMethod {
    ZeroWidth,
    #[allow(dead_code)]
    ThinSpace,
    #[allow(dead_code)]
    // supported rendering mode; current production classifier does not select it
    HexCode,
    EmptyBox,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GlyphlessJoinerPolicy {
    ClassifyAsGlyphless,
    PreserveForComposition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayGlyphless {
    pub(crate) ch: char,
    pub(crate) method: GlyphlessMethod,
}

pub(crate) fn control_char_caret_char(ch: char) -> Option<char> {
    match ch {
        '\u{0000}'..='\u{001f}' => Some(char::from((ch as u8) + b'@')),
        '\u{007f}' => Some('?'),
        _ => None,
    }
}

pub(crate) fn glyphless_method_for_char(
    ch: char,
    joiner_policy: GlyphlessJoinerPolicy,
) -> Option<GlyphlessMethod> {
    if joiner_policy == GlyphlessJoinerPolicy::PreserveForComposition
        && crate::composition::is_composition_joiner(ch)
    {
        return None;
    }

    let cp = ch as u32;
    match cp {
        // NB: the C1 controls (U+0080..U+009F) and unassigned specials
        // (U+FFF0..U+FFF8) are NON-PRINTABLE, so GNU escapes them as `\`+octal in
        // the escape-glyph face -- they are classified by `is_escape_glyph_octal`
        // (see `classify_text_source_char`) BEFORE this table is consulted, so
        // they are intentionally absent here (was `GlyphlessMethod::HexCode`).
        0xfffc => Some(GlyphlessMethod::EmptyBox),
        // Fast paths for the common invisible chars: the format-control chars in
        // the arm below (ZWSP/ZWJ/LRM/tags -- all `Cf`, so also caught by the
        // category check) plus the variation selectors (`Mn`) and line/paragraph
        // separators (`Zl`/`Zp`), which are NOT `Cf` and so need listing here.
        0xfeff
        | 0x200b..=0x200f
        | 0x2028..=0x2029
        | 0xe0001..=0xe007f
        | 0xe0100..=0xe01ef
        | 0xfe00..=0xfe0f => Some(GlyphlessMethod::ZeroWidth),
        // GNU's `format-control` group (glyphless-char-display-control default
        // `thin-space`): general-category `Cf` chars, rendered invisible
        // (ZeroWidth) like the ZWSP/ZWJ/LRM fast paths above -- otherwise a `Cf`
        // char a font can't draw (e.g. U+FFF9..U+FFFB interlinear annotations)
        // falls through to a `.notdef` box.
        //
        // BUT only the `Cf` chars that are also `Default_Ignorable_Code_Point`:
        // Unicode/GNU RENDER the non-ignorable format controls (they carry
        // visible or shaping meaning), so they must NOT be hidden -- see
        // `is_non_ignorable_format_control`. The one in etc/HELLO is U+180E
        // MONGOLIAN VOWEL SEPARATOR (removed from Default_Ignorable in Unicode
        // 6.3): GNU emits it as part of the Mongolian text (composed clusters
        // bypass the glyphless path, xdisp.c `get_next_display_element`), so a
        // blanket "all `Cf` -> ZeroWidth" wrongly dropped it and diverged from
        // GNU on the TTY. Also excludes U+00AD (SHY, has a visible glyph).
        // Guarded on `cp >= 0x80` so ASCII (never `Cf`, the hot path) skips the
        // category lookup that `is_escape_glyph_octal` already fast-paths past.
        _ if cp >= 0x80
            && cp != 0xad
            && is_format_control(cp)
            && !is_non_ignorable_format_control(cp) =>
        {
            Some(GlyphlessMethod::ZeroWidth)
        }
        _ => None,
    }
}

/// True if `cp` is a general-category `Cf` (format-control) character -- GNU's
/// `format-control` glyphless group. ASCII is never `Cf`; callers fast-path it.
fn is_format_control(cp: u32) -> bool {
    use neovm_core::emacs_core::emacs_char::{UnicodeCategory, char_general_category};
    char_general_category(cp) == Some(UnicodeCategory::Format as i64)
}

/// The general-category `Cf` characters that are NOT
/// `Default_Ignorable_Code_Point`, i.e. the format controls Unicode/GNU still
/// *render* rather than hide. Within `Cf` this is exactly the
/// `Prepended_Concatenation_Mark` set (Arabic/Syriac/Kaithi number & ayah
/// signs, which prepend to following digits), the Egyptian Hieroglyph format
/// controls (which drive hieroglyph quadrat layout), and U+180E MONGOLIAN
/// VOWEL SEPARATOR (removed from `Default_Ignorable` in Unicode 6.3). Keeping
/// these out of the glyphless ZeroWidth rule matches GNU, which emits them
/// (via a font glyph or as part of a composed cluster) instead of hiding them.
fn is_non_ignorable_format_control(cp: u32) -> bool {
    matches!(cp,
        0x0600..=0x0605   // Arabic number/year/footnote/etc. signs
        | 0x06DD          // Arabic end of ayah
        | 0x070F          // Syriac abbreviation mark
        | 0x0890..=0x0891 // Arabic pound / piastre marks
        | 0x08E2          // Arabic disputed end of ayah
        | 0x110BD | 0x110CD // Kaithi number sign / number sign above
        | 0x180E          // Mongolian vowel separator
        | 0x13430..=0x1343F // Egyptian Hieroglyph format controls
    )
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DisplayLength {
    #[allow(dead_code)]
    Columns(u16),
    Pixels(f32),
    Em(f32),
    /// A `(space :width/:height/:ascent …)` operand kept verbatim as Lisp.
    ///
    /// GNU stores the property object and evaluates it with
    /// `calc_pixel_width_or_height` (xdisp.c:30355); there is no second,
    /// typed decode of the expression grammar. Keeping the operand as Lisp
    /// means every form GNU accepts reaches the evaluator — including forms
    /// a typed mirror would have to enumerate, such as `(NUM . EXPR)`
    /// products and `(image …)` operands (issue #204).
    Expr(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DisplayStretchWidth {
    Length(DisplayLength),
    /// `:align-to` operand, kept verbatim as Lisp — see [`DisplayLength::Expr`].
    AlignTo(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DisplayStretch {
    pub(crate) width: DisplayStretchWidth,
    pub(crate) height: Option<DisplayLength>,
    pub(crate) ascent: Option<DisplayLength>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DisplayImageItem {
    pub(crate) image_id: i32,
    pub(crate) source_rect: neomacs_display_protocol::ImageSourceRect,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) ascent: f32,
    pub(crate) horizontal_margin: f32,
    pub(crate) vertical_margin: f32,
    pub(crate) opaque_background: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DisplayVideoItem {
    pub(crate) video_id: i32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) loop_count: i32,
    pub(crate) autoplay: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DisplayXwidgetItem {
    pub(crate) xwidget_id: i32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DisplaySurfaceItem {
    pub(crate) surface_id: i32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DisplayMediaReplacement {
    pub(crate) kind: DisplayMediaReplacementKind,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) ascent: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum DisplayMediaReplacementKind {
    Image {
        image_id: u32,
        source_rect: neomacs_display_protocol::ImageSourceRect,
        horizontal_margin: f32,
        vertical_margin: f32,
        opaque_background: Option<u32>,
    },
    /// A valid image replacement whose GNU slice resolves to no pixels. The
    /// source text is still consumed, but no placeholder or drawable glyph is
    /// emitted.
    EmptyImageSlice,
    Video {
        video_id: u32,
        loop_count: i32,
        autoplay: bool,
    },
    Xwidget {
        xwidget_id: u32,
    },
    Surface {
        surface_id: u32,
    },
}

impl DisplayMediaReplacement {
    pub(crate) fn replacement_stretch(self) -> DisplayStretch {
        DisplayStretch {
            width: DisplayStretchWidth::Length(DisplayLength::Pixels(self.width)),
            height: Some(DisplayLength::Pixels(self.height)),
            ascent: Some(DisplayLength::Pixels(self.ascent)),
        }
    }

    pub(crate) fn image(image: DisplayImageItem) -> Self {
        let horizontal_margin = display_replacement_margin(image.horizontal_margin);
        let vertical_margin = display_replacement_margin(image.vertical_margin);
        Self {
            kind: DisplayMediaReplacementKind::Image {
                image_id: image.image_id.max(0) as u32,
                source_rect: image.source_rect,
                horizontal_margin,
                vertical_margin,
                opaque_background: image.opaque_background,
            },
            width: display_replacement_dimension(image.width) + 2.0 * horizontal_margin,
            height: display_replacement_dimension(image.height) + 2.0 * vertical_margin,
            ascent: display_replacement_ascent(image.ascent) + vertical_margin,
        }
    }

    pub(crate) const fn empty_image_slice() -> Self {
        Self {
            kind: DisplayMediaReplacementKind::EmptyImageSlice,
            width: 0.0,
            height: 0.0,
            ascent: 0.0,
        }
    }

    pub(crate) fn video(video: DisplayVideoItem) -> Self {
        Self {
            kind: DisplayMediaReplacementKind::Video {
                video_id: video.video_id.max(0) as u32,
                loop_count: video.loop_count,
                autoplay: video.autoplay,
            },
            width: display_replacement_dimension(video.width),
            height: display_replacement_dimension(video.height),
            ascent: display_replacement_ascent(video.height),
        }
    }

    pub(crate) fn xwidget(xwidget: DisplayXwidgetItem) -> Self {
        Self {
            kind: DisplayMediaReplacementKind::Xwidget {
                xwidget_id: xwidget.xwidget_id.max(0) as u32,
            },
            width: display_replacement_dimension(xwidget.width),
            height: display_replacement_dimension(xwidget.height),
            ascent: display_replacement_ascent(xwidget.height),
        }
    }

    pub(crate) fn surface(surface: DisplaySurfaceItem) -> Self {
        Self {
            kind: DisplayMediaReplacementKind::Surface {
                surface_id: surface.surface_id.max(0) as u32,
            },
            width: display_replacement_dimension(surface.width),
            height: display_replacement_dimension(surface.height),
            ascent: display_replacement_ascent(surface.height),
        }
    }
}

fn display_replacement_dimension(value: f32) -> f32 {
    if value.is_finite() {
        value.max(1.0)
    } else {
        1.0
    }
}

fn display_replacement_margin(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn display_replacement_ascent(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayRowBreak {
    pub(crate) reason: DisplayRowBreakReason,
    pub(crate) line_height: DisplayLineHeightPolicy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DisplayLineHeightPolicy {
    /// The newline contributes its face's normal height and configured line
    /// spacing to the display row.
    #[default]
    Default,
    /// GNU `line-height t`: the newline contributes no default height or line
    /// spacing; visible row contents alone determine the row geometry.
    ContentOnly,
}

impl DisplayLineHeightPolicy {
    pub(crate) fn from_property(value: Option<Value>) -> Self {
        if value.is_some_and(|value| value.is_t()) {
            Self::ContentOnly
        } else {
            Self::Default
        }
    }
}

impl DisplayRowBreak {
    pub(crate) const fn explicit_newline() -> Self {
        Self {
            reason: DisplayRowBreakReason::ExplicitNewline,
            line_height: DisplayLineHeightPolicy::Default,
        }
    }

    pub(crate) const fn with_line_height(mut self, line_height: DisplayLineHeightPolicy) -> Self {
        self.line_height = line_height;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DisplayRowBreakReason {
    ExplicitNewline,
    #[allow(dead_code)]
    Wrap,
    #[allow(dead_code)]
    Truncate,
    #[allow(dead_code)]
    EndOfSource,
}

#[cfg(test)]
#[path = "display_item_test.rs"]
mod tests;
